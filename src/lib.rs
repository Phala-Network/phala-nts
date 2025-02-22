// This file is part of cfnts.
// Copyright (c) 2019, Cloudflare. All rights reserved.
// See LICENSE for licensing information.

mod client;
mod dns_resolver;
mod ntp;
mod nts_ke;

pub use client::get_time;
pub use ntp::client::NtpResult;
