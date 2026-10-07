//! Minimal command-line surface for the packaged `idoris` binary.

use std::ffi::OsString;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Serve(ServeOptions),
    Admin(AdminCommand),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServeOptions {
    pub admin_token_stdin: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminCommand {
    Status(AdminOptions),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdminOptions {
    pub token_stdin: bool,
}

const USAGE: &str =
    "用法: idoris [serve [--admin-token-stdin]] | idoris admin status --token-stdin";

pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let args = args.into_iter().collect::<Vec<_>>();
    match args.as_slice() {
        [] => Ok(Command::Serve(ServeOptions::default())),
        [command] if command == "serve" => Ok(Command::Serve(ServeOptions::default())),
        [command, option] if command == "serve" && option == "--admin-token-stdin" => {
            Ok(Command::Serve(ServeOptions {
                admin_token_stdin: true,
            }))
        }
        [admin, status, option]
            if admin == "admin" && status == "status" && option == "--token-stdin" =>
        {
            Ok(Command::Admin(AdminCommand::Status(AdminOptions {
                token_stdin: true,
            })))
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
        assert_eq!(
            parse_args([
                OsString::from("admin"),
                OsString::from("status"),
                OsString::from("--token-stdin")
            ])
            .unwrap(),
            Command::Admin(AdminCommand::Status(AdminOptions { token_stdin: true }))
        );
        for args in [
            vec![OsString::from("nope")],
            vec![OsString::from("serve"), OsString::from("extra")],
            vec![OsString::from("--admin-token-stdin")],
            vec![OsString::from("admin")],
            vec![OsString::from("admin"), OsString::from("status")],
            vec![
                OsString::from("admin"),
                OsString::from("status"),
                OsString::from("extra"),
            ],
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
