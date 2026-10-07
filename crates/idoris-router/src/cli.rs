//! Minimal command-line surface for the packaged `idoris` binary.

use std::ffi::OsString;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Serve,
    Key(KeyCommand),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCommand {
    Issue,
}

const USAGE: &str = "用法: idoris [serve] | idoris key issue --spec-stdin";

pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(Command::Serve);
    };
    if first == "serve" && args.next().is_none() {
        return Ok(Command::Serve);
    }
    if first == "key"
        && args.next().is_some_and(|arg| arg == "issue")
        && args.next().is_some_and(|arg| arg == "--spec-stdin")
        && args.next().is_none()
    {
        return Ok(Command::Key(KeyCommand::Issue));
    }
    Err(USAGE.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn parses_serve_and_exact_key_issue_form() {
        assert_eq!(parse_args([]).unwrap(), Command::Serve);
        assert_eq!(
            parse_args([OsString::from("serve")]).unwrap(),
            Command::Serve
        );
        assert_eq!(
            parse_args([
                OsString::from("key"),
                OsString::from("issue"),
                OsString::from("--spec-stdin"),
            ])
            .unwrap(),
            Command::Key(KeyCommand::Issue)
        );
        for args in [
            vec![OsString::from("nope")],
            vec![OsString::from("serve"), OsString::from("extra")],
            vec![OsString::from("key"), OsString::from("issue")],
            vec![
                OsString::from("key"),
                OsString::from("issue"),
                OsString::from("--secret"),
            ],
        ] {
            assert_eq!(parse_args(args).unwrap_err(), USAGE);
        }
    }
}
