//! Minimal command-line surface for the packaged `idoris` binary.

use std::ffi::OsString;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Serve(ServeOptions),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServeOptions {
    pub admin_token_stdin: bool,
}

const USAGE: &str = "用法: idoris [serve [--admin-token-stdin]]";

pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut args = args.into_iter();
    match (args.next(), args.next(), args.next()) {
        (None, None, None) => Ok(Command::Serve(ServeOptions::default())),
        (Some(command), None, None) if command == "serve" => {
            Ok(Command::Serve(ServeOptions::default()))
        }
        (Some(command), Some(option), None)
            if command == "serve" && option == "--admin-token-stdin" =>
        {
            Ok(Command::Serve(ServeOptions {
                admin_token_stdin: true,
            }))
        }
        _ => Err(USAGE.to_string()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn serve_accepts_only_the_explicit_admin_token_source_option() {
        assert_eq!(
            parse_args([]).unwrap(),
            Command::Serve(ServeOptions::default())
        );
        assert_eq!(
            parse_args([OsString::from("serve")]).unwrap(),
            Command::Serve(ServeOptions::default())
        );
        assert_eq!(
            parse_args([
                OsString::from("serve"),
                OsString::from("--admin-token-stdin")
            ])
            .unwrap(),
            Command::Serve(ServeOptions {
                admin_token_stdin: true
            })
        );
        for args in [
            vec![OsString::from("nope")],
            vec![OsString::from("serve"), OsString::from("extra")],
            vec![OsString::from("--admin-token-stdin")],
            vec![
                OsString::from("serve"),
                OsString::from("--admin-token-stdin"),
                OsString::from("extra"),
            ],
        ] {
            assert_eq!(parse_args(args).unwrap_err(), USAGE);
        }
    }
}
