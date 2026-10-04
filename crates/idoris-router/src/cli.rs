//! Minimal command-line surface for the packaged `idoris` binary.

use std::ffi::OsString;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Serve,
}

const USAGE: &str = "用法: idoris [serve]";

pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut args = args.into_iter();
    match (args.next(), args.next()) {
        (None, None) => Ok(Command::Serve),
        (Some(command), None) if command == "serve" => Ok(Command::Serve),
        _ => Err(USAGE.to_string()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn bare_and_serve_are_the_only_supported_forms() {
        assert_eq!(parse_args([]).unwrap(), Command::Serve);
        assert_eq!(
            parse_args([OsString::from("serve")]).unwrap(),
            Command::Serve
        );
        for args in [
            vec![OsString::from("nope")],
            vec![OsString::from("serve"), OsString::from("extra")],
        ] {
            assert_eq!(parse_args(args).unwrap_err(), USAGE);
        }
    }
}
