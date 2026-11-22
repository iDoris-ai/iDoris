//! Platforms without a probe (neither macOS nor Linux): no watch can be established, the caller falls back to something slower.

use super::Probe;

#[derive(Debug)]
pub(crate) struct Watch;

pub(crate) type Hint = ();

pub(crate) fn establish(_pid: u32, _port: u16) -> Option<(Watch, Option<Hint>)> {
    None
}

impl Watch {
    pub(crate) fn check(&self, hint: Option<Hint>) -> (Probe, Option<Hint>) {
        (Probe::Unavailable, hint)
    }
}
