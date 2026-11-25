//! Platforms without a probe (neither macOS nor Linux): no watch can be established, the caller falls back to something slower.

use std::net::IpAddr;

use super::{Probe, Quick};

#[derive(Debug)]
pub(crate) struct Watch;

pub(crate) type Hint = ();

pub(crate) fn establish(_pid: u32, _port: u16, _dial: IpAddr) -> Option<Watch> {
    None
}

pub(crate) fn process_generation(_pid: u32) -> Option<String> {
    None
}

pub(crate) fn executable_identity(_pid: u32) -> Option<super::ExecutableIdentity> {
    None
}

impl Watch {
    pub(crate) fn generation(&self) -> Option<String> {
        None
    }

    pub(crate) fn quick(&self, hint: Option<Hint>) -> Quick<Hint> {
        Quick::Answer(Probe::Unavailable, hint)
    }

    pub(crate) fn scan(&self, hint: Option<Hint>) -> (Probe, Option<Hint>) {
        (Probe::Unavailable, hint)
    }
}

pub(crate) fn acl_grants_others_write_fd(_fd: i32, _owner: u32) -> Option<bool> {
    None
}

pub(crate) fn acl_grants_others_write_link(_path: &std::path::Path, _owner: u32) -> Option<bool> {
    None
}

pub(crate) fn signing_team(_pid: u32) -> Option<String> {
    None
}
