const MAX_UTF16_UNITS: usize = 128;

pub(crate) fn valid(value: &str) -> bool {
    !value.trim().is_empty()
        && value.encode_utf16().count() <= MAX_UTF16_UNITS
        && !value.chars().any(char::is_control)
}
