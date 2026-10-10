//! Trusted correlation metadata from Agent24. These values are identifiers
//! only; the server-generated request record id remains a separate authority.

use axum::http::HeaderMap;

const MAX_ID_UTF16_UNITS: usize = 128;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestCorrelation {
    pub session_id: Option<String>,
    pub trace_id: Option<String>,
    pub parent_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrelationError {
    Session,
    Trace,
    Parent,
}

impl CorrelationError {
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::Session => "INVALID_CORRELATION_SESSION",
            Self::Trace => "INVALID_CORRELATION_TRACE_ID",
            Self::Parent => "INVALID_CORRELATION_PARENT_ID",
        }
    }
}

pub fn parse(headers: &HeaderMap) -> Result<RequestCorrelation, CorrelationError> {
    Ok(RequestCorrelation {
        session_id: parse_one(headers, "x-idoris-session", CorrelationError::Session)?,
        trace_id: parse_one(headers, "x-idoris-trace-id", CorrelationError::Trace)?,
        parent_id: parse_one(headers, "x-idoris-parent-id", CorrelationError::Parent)?,
    })
}

fn parse_one(
    headers: &HeaderMap,
    name: &str,
    error: CorrelationError,
) -> Result<Option<String>, CorrelationError> {
    let mut values = headers.get_all(name).iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(error);
    }
    let raw = std::str::from_utf8(first.as_bytes()).map_err(|_| error)?;
    validate_value(raw).map_err(|()| error)?;
    Ok(Some(raw.to_string()))
}

fn validate_value(raw: &str) -> Result<(), ()> {
    if raw.trim().is_empty()
        || raw.encode_utf16().count() > MAX_ID_UTF16_UNITS
        || raw.chars().any(char::is_control)
    {
        return Err(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use axum::http::{HeaderMap, HeaderValue};

    use super::*;

    #[test]
    fn accepted_values_are_preserved_without_trimming() {
        let mut headers = HeaderMap::new();
        headers.insert("x-idoris-session", "  session value  ".parse().unwrap());
        headers.insert("x-idoris-trace-id", "trace-1".parse().unwrap());
        let parsed = parse(&headers).unwrap();
        assert_eq!(parsed.session_id.as_deref(), Some("  session value  "));
        assert_eq!(parsed.trace_id.as_deref(), Some("trace-1"));
        assert_eq!(parsed.parent_id, None);
    }

    #[test]
    fn utf16_boundary_accepts_128_units_and_rejects_129() {
        for (value, accepted) in [
            ("😀".repeat(64), true),
            (format!("{}a", "😀".repeat(64)), false),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(
                "x-idoris-trace-id",
                HeaderValue::from_bytes(value.as_bytes()).unwrap(),
            );
            assert_eq!(parse(&headers).is_ok(), accepted);
        }
    }

    #[test]
    fn duplicate_non_utf8_blank_and_control_values_fail_closed() {
        let mut duplicate = HeaderMap::new();
        duplicate.append("x-idoris-session", "a".parse().unwrap());
        duplicate.append("x-idoris-session", "b".parse().unwrap());
        assert_eq!(parse(&duplicate).unwrap_err(), CorrelationError::Session);

        let mut non_utf8 = HeaderMap::new();
        non_utf8.insert(
            "x-idoris-trace-id",
            HeaderValue::from_bytes(b"\xff").unwrap(),
        );
        assert_eq!(parse(&non_utf8).unwrap_err(), CorrelationError::Trace);

        for raw in [b"   ".as_slice(), b"bad\tparent".as_slice()] {
            let mut headers = HeaderMap::new();
            headers.insert("x-idoris-parent-id", HeaderValue::from_bytes(raw).unwrap());
            assert_eq!(parse(&headers).unwrap_err(), CorrelationError::Parent);
        }
    }
}
