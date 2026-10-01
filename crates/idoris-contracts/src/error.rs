use std::fmt;

/// Everything that can go wrong turning a JSON value into a contract type:
/// either serde's structural deserialize (missing/extra/mistyped fields,
/// unknown enum variants — `#[serde(deny_unknown_fields)]` everywhere, to
/// match the TS side's zod `.strict()` behaviour) or one of the extra
/// constraints JSON Schema expresses that plain Rust types can't (minLength,
/// minItems, numeric ranges, the `sha256:` digest pattern, ...), checked by
/// [`Contract::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractError(pub String);

impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ContractError {}

impl ContractError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// A contract type that round-trips through JSON and knows how to check the
/// constraints its `serde` derive can't express on its own.
pub trait Contract: Sized + serde::de::DeserializeOwned {
    /// Constraints JSON Schema declares (minLength, minItems, numeric
    /// ranges, patterns, ...) that a successful `serde` deserialize does not
    /// already guarantee. Implementations should assume the value already
    /// deserialized cleanly and only need to check the "extra" rules.
    fn validate(&self) -> Result<(), ContractError>;
}

/// Deserialize `value` into `T` and run its [`Contract::validate`] — the
/// Rust-side equivalent of the TS side's `schema.safeParse(value)`.
pub fn parse<T: Contract>(value: &serde_json::Value) -> Result<T, ContractError> {
    let parsed: T =
        serde_json::from_value(value.clone()).map_err(|err| ContractError::new(err.to_string()))?;
    parsed.validate()?;
    Ok(parsed)
}

/// True when `s` is non-empty after the JSON Schema `minLength: 1` sense
/// (byte length, matching how the schemas are written — none of them use
/// unicode-aware length constraints).
pub(crate) fn non_empty(s: &str) -> bool {
    !s.is_empty()
}
