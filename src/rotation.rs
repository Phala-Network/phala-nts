use std::collections::HashMap;
use std::io;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time;
use std::time::SystemTime;

use memcache;
use memcache::MemcacheError;

use lazy_static::lazy_static;
use prometheus::{opts, register_counter, register_int_counter, IntCounter, Opts};
use slog::{debug, error, info, trace, warn};

use ring::digest;
use ring::hmac;

lazy_static! {
    static ref ROTATION_COUNTER: IntCounter =
        register_int_counter!("ntp_key_rotations_total", "Number of key rotations").unwrap();
    static ref FAILURE_COUNTER: IntCounter = register_int_counter!(
        "ntp_key_rotations_failed_total",
        "Number of failures in key rotation"
    )
    .unwrap();
}
pub type KeyID = [u8; 4];

pub struct RotatingKeys {
    pub memcache_url: String,
    pub prefix: String,
    pub duration: i64,
    pub forward_periods: i64,
    pub backward_periods: i64,
    pub master_key: Vec<u8>,
    pub latest: KeyID,
    pub keys: HashMap<KeyID, Vec<u8>>,
    pub logger: slog::Logger,
}

/// This function writes a i64 as 4 bytes in big endian.
/// Since we are using timestamps we are fine with 4 bytes.
/// Rollover doesn't matter here since we don't have 38 years worth
/// of cookies.
fn be_bytes(n: i64) -> [u8; 4] {
    let mut ret: [u8; 4] = [0; 4];
    let mut u = n as u32;
    for i in 0..3 {
        ret[3 - i] = u as u8;
        u = u >> 8;
    }
    ret
}

trait VecMap {
    fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, MemcacheError>;
}

struct MemcacheVecMap {
    client: memcache::Client,
}

impl VecMap for MemcacheVecMap {
    fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, MemcacheError> {
        self.client.get::<Vec<u8>>(key)
    }
}

impl RotatingKeys {
    pub fn rotate_keys(&mut self) -> Result<(), Box<std::error::Error>> {
        ROTATION_COUNTER.inc();
        let mut client = memcache::Client::connect(self.memcache_url.clone())?;
        let now = SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
        let timestamp = now.as_secs() as i64;
        let mut vecmap = MemcacheVecMap { client: client };
        self.internal_rotate(&mut vecmap, timestamp)
    }

    fn internal_rotate(
        &mut self,
        client: &mut dyn VecMap,
        timestamp: i64,
    ) -> Result<(), Box<std::error::Error>> {
        let mut failed = false;
        for i in -self.backward_periods..(self.forward_periods + 1) {
            let epoch = self.epoch(timestamp, i);
            let db_loc = format!("{}/{}", self.prefix, epoch);
            let db_val = client.get(&db_loc)?;
            let key_id = be_bytes(epoch);
            match db_val {
                Some(s) => {
                    self.keys.insert(key_id, self.compute_wrap(s));
                }
                None => {
                    FAILURE_COUNTER.inc();
                    error!(self.logger, "cannot read from memcache"; "key"=>db_loc, "memcache_url"=>self.memcache_url.clone());
                    failed = true;
                }
            }
        }
        self.keys
            .remove(&be_bytes(self.epoch(timestamp, -self.backward_periods - 1)));
        self.latest = be_bytes(self.epoch(timestamp, 0)); // Not all of our friends may have gotten the same forwards keys as we did
        if failed {
            return Err(
                io::Error::new(io::ErrorKind::Other, "A request to memcached failed").into(),
            );
        } else {
            return Ok(());
        }
    }

    fn compute_wrap(&self, val: Vec<u8>) -> Vec<u8> {
        let key = hmac::SigningKey::new(&digest::SHA256, &self.master_key);
        hmac::sign(&key, &val).as_ref().to_vec()
    }

    fn epoch(&self, seconds: i64, offset: i64) -> i64 {
        ((seconds / self.duration) + offset) * self.duration
    }

    pub fn latest(&self) -> (KeyID, Vec<u8>) {
        (self.latest, self.keys[&self.latest].clone())
    }
}

pub fn periodic_rotate(rotor: Arc<RwLock<RotatingKeys>>) {
    let mut rotor = rotor.clone();
    thread::spawn(move || loop {
        inner(&mut rotor);
        let restlen = read_sleep(&rotor);
        thread::sleep(time::Duration::from_secs(restlen as u64));
    });
}

fn inner(rotor: &mut Arc<RwLock<RotatingKeys>>) {
    rotor.write().unwrap().rotate_keys();
}

fn read_sleep(rotor: &Arc<RwLock<RotatingKeys>>) -> i64 {
    rotor.read().unwrap().duration
}

#[cfg(test)]
mod test {
    use super::*;
    use std::collections::HashMap;

    struct HashMapVecMap {
        table: HashMap<String, Option<Vec<u8>>>,
    }

    impl VecMap for HashMapVecMap {
        fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, MemcacheError> {
            Ok(self.table[&key.to_owned()].clone())
        }
    }

    #[test]
    fn test_rotation() {
        use sloggers::null::NullLoggerBuilder;
        use sloggers::Build;
        let mut testmap = HashMapVecMap {
            table: HashMap::new(),
        };
        testmap
            .table
            .insert("test/1".to_string(), Some(vec![1; 32]));
        testmap
            .table
            .insert("test/2".to_string(), Some(vec![2; 32]));
        testmap
            .table
            .insert("test/3".to_string(), Some(vec![3; 32]));
        testmap
            .table
            .insert("test/4".to_string(), Some(vec![4; 32]));
        testmap.table.insert("test/5".to_string(), None);
        testmap.table.insert("test/0".to_string(), None);

        let mut test_rotor = RotatingKeys {
            memcache_url: "unused".to_owned(),
            prefix: "test".to_owned(),
            duration: 1,
            forward_periods: 1,
            backward_periods: 1,
            master_key: vec![0, 32],
            latest: [1, 2, 3, 4],
            keys: HashMap::new(),
            logger: NullLoggerBuilder.build().unwrap(),
        };
        test_rotor.internal_rotate(&mut testmap, 2).unwrap();
        let old_latest = test_rotor.latest;
        test_rotor.internal_rotate(&mut testmap, 3).unwrap();
        let new_latest = test_rotor.latest;
        assert_ne!(old_latest, new_latest);
        let res = test_rotor.internal_rotate(&mut testmap, 1);
        if let Ok(_) = res {
            panic!("Success should not have happened!")
        }
        let res = test_rotor.internal_rotate(&mut testmap, 4);
        if let Ok(_) = res {
            panic!("Success should not have happened!")
        }
    }
}