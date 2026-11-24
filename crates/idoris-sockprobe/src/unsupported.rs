//! Platforms without a probe (neither macOS nor Linux): no watch can be established, the caller falls back to something slower.

use std::net::IpAddr;

use super::{Probe, Quick};

#[derive(Debug)]
pub(crate) struct Watch;

pub(crate) type Hint = ();

pub(crate) fn establish(_pid: u32, _port: u16, _dial: IpAddr) -> Option<Watch> {
    None
}

impl Watch {
    pub(crate) fn quick(&self, hint: Option<Hint>) -> Quick<Hint> {
        Quick::Answer(Probe::Unavailable, hint)
    }

    pub(crate) fn scan(&self, hint: Option<Hint>) -> (Probe, Option<Hint>) {
        (Probe::Unavailable, hint)
    }
}
