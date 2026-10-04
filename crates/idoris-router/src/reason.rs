#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonKind {
    PrivacyEnforced,
    Budget,
    IntentMatch,
    Degraded,
}

impl ReasonKind {
    pub const ALL: [&'static str; 4] = ["privacy_enforced", "budget", "intent_match", "degraded"];

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "privacy_enforced" => Self::PrivacyEnforced,
            "budget" => Self::Budget,
            "intent_match" => Self::IntentMatch,
            "degraded" => Self::Degraded,
            _ => return None,
        })
    }
}
