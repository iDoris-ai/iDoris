#[derive(Debug, Clone, Copy)]
pub(crate) struct UsageFact {
    /// None means the call completed but cost truth is unavailable; do not
    /// persist it as zero. Some(0) is an explicitly-known free inference.
    pub cost_minor: Option<i64>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    /// False for cache replay: no new inference happened, so no usage row.
    pub inference: bool,
}

impl UsageFact {
    pub const fn inference(cost_minor: Option<i64>) -> Self {
        Self {
            cost_minor,
            tokens_in: None,
            tokens_out: None,
            inference: true,
        }
    }

    pub const fn inference_with_tokens(
        cost_minor: Option<i64>,
        tokens_in: u64,
        tokens_out: u64,
    ) -> Self {
        Self {
            cost_minor,
            tokens_in: Some(tokens_in),
            tokens_out: Some(tokens_out),
            inference: true,
        }
    }

    pub const fn cached() -> Self {
        Self {
            cost_minor: None,
            tokens_in: None,
            tokens_out: None,
            inference: false,
        }
    }
}
