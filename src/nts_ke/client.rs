use log::debug;
use std::convert::TryFrom;
use std::error::Error;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use rustls;
use webpki_roots;

use super::records;

use crate::nts_ke::records::{
    deserialize,
    process_record,

    // Functions.
    serialize,
    // Records.
    AeadAlgorithmRecord,
    // Errors.
    DeserializeError,

    EndOfMessageRecord,

    // Enums.
    KnownAeadAlgorithm,
    KnownNextProtocol,
    NTSKeys,
    NextProtocolRecord,
    NtsKeParseError,
    Party,

    // Structs.
    ReceivedNtsKeRecordState,

    // Constants.
    HEADER_SIZE,
};

type Cookie = Vec<u8>;

const DEFAULT_NTP_PORT: u16 = 123;
const DEFAULT_KE_PORT: u16 = 4460;
const DEFAULT_SCHEME: u16 = 0;
const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub struct ClientConfig {
    pub host: String,
    pub port: Option<u16>,
    pub use_ipv6: bool,
}

#[derive(Clone, Debug)]
pub struct NtsKeResult {
    pub cookies: Vec<Cookie>,
    pub next_protocols: Vec<u16>,
    pub aead_scheme: u16,
    pub next_server: String,
    pub next_port: u16,
    pub keys: NTSKeys,
    pub use_ipv6: bool,
}

/// run_nts_client executes the nts client with the config in config file
pub fn run_nts_ke_client(client_config: ClientConfig) -> Result<NtsKeResult, Box<dyn Error>> {
    let alpn_proto = String::from("ntske/1");
    let alpn_bytes = alpn_proto.into_bytes();
    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    tls_config.alpn_protocols = vec![alpn_bytes];

    let rc_config = Arc::new(tls_config);
    let hostname = rustls::pki_types::ServerName::try_from(client_config.host.as_str())
        .expect("server hostname is invalid");
    let mut client =
        rustls::ClientConnection::new(rc_config, hostname.to_owned()).expect("Failed to connect");
    debug!("Connecting");
    let port = client_config.port.unwrap_or(DEFAULT_KE_PORT);

    let mut ip_addrs = (client_config.host.as_str(), port).to_socket_addrs()?;
    let addr;
    if client_config.use_ipv6 {
        // mandated to use ipv6
        addr = ip_addrs.find(|&x| x.is_ipv6());
        if addr.is_none() {
            return Err(Box::new(NtsKeParseError::NoIpv6AddrFound));
        }
    } else {
        // mandated to use ipv4
        addr = ip_addrs.find(|&x| x.is_ipv4());
        if addr.is_none() {
            return Err(Box::new(NtsKeParseError::NoIpv4AddrFound));
        }
    }
    let mut stream = TcpStream::connect_timeout(&addr.unwrap(), TIMEOUT)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;

    let mut tls_stream = rustls::Stream::new(&mut client, &mut stream);

    let next_protocol_record = NextProtocolRecord::from(vec![KnownNextProtocol::Ntpv4]);
    let aead_record = AeadAlgorithmRecord::from(vec![KnownAeadAlgorithm::AeadAesSivCmac256]);
    let end_record = EndOfMessageRecord;

    let clientrec = &mut serialize(next_protocol_record);
    clientrec.append(&mut serialize(aead_record));
    clientrec.append(&mut serialize(end_record));
    tls_stream.write_all(clientrec)?;
    tls_stream.flush()?;
    debug!("Request transmitted");
    let keys = records::gen_key(tls_stream.conn).unwrap();

    let mut state = ReceivedNtsKeRecordState {
        finished: false,
        next_protocols: Vec::new(),
        aead_scheme: Vec::new(),
        cookies: Vec::new(),
        next_server: None,
        next_port: None,
    };

    while !state.finished {
        let mut header: [u8; HEADER_SIZE] = [0; HEADER_SIZE];

        // We should use `read_exact` here because we always need to read 4 bytes to get the
        // header.
        if let Err(error) = tls_stream.read_exact(&mut header[..]) {
            return Err(Box::new(error));
        }

        // Retrieve a body length from the 3rd and 4th bytes of the header.
        let body_length = u16::from_be_bytes([header[2], header[3]]);
        let mut body = vec![0; body_length as usize];

        // `read_exact` the length of the body.
        if let Err(error) = tls_stream.read_exact(body.as_mut_slice()) {
            return Err(Box::new(error));
        }

        // Reconstruct the whole record byte array to let the `records` module deserialize it.
        let mut record_bytes = Vec::from(&header[..]);
        record_bytes.append(&mut body);

        // `deserialize` has an invariant that the slice needs to be long enough to make it a
        // valid record, which in this case our slice is exactly as long as specified in the
        // length field.
        match deserialize(Party::Client, record_bytes.as_slice()) {
            Ok(record) => {
                let status = process_record(record, &mut state);
                match status {
                    Ok(_) => {}
                    Err(err) => {
                        return Err(err);
                    }
                }
            }
            Err(DeserializeError::UnknownNotCriticalRecord) => {
                // If it's not critical, just ignore the error.
                debug!("unknown record type");
            }
            Err(DeserializeError::UnknownCriticalRecord) => {
                // TODO: This should propertly handled by sending an Error record.
                debug!("error: unknown critical record");
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "unknown critical record",
                )));
            }
            Err(DeserializeError::Parsing(error)) => {
                // TODO: This shouldn't be wrapped as a trait object.
                debug!("error: {}", error);
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    error,
                )));
            }
        }
    }
    debug!("saw the end of the response");
    stream.shutdown(Shutdown::Write)?;

    let aead_scheme = if state.aead_scheme.is_empty() {
        DEFAULT_SCHEME
    } else {
        state.aead_scheme[0]
    };

    Ok(NtsKeResult {
        aead_scheme,
        cookies: state.cookies,
        next_protocols: state.next_protocols,
        next_server: state.next_server.unwrap_or(client_config.host.clone()),
        next_port: state.next_port.unwrap_or(DEFAULT_NTP_PORT),
        keys,
        use_ipv6: client_config.use_ipv6,
    })
}
