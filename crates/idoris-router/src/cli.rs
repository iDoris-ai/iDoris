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
    Read(AdminResource, AdminOptions),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminResource {
    Status,
    Backends,
    Models,
    Roles,
    Runtimes,
}

impl AdminResource {
    pub fn path(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Backends => "backends",
            Self::Models => "models",
            Self::Roles => "roles",
            Self::Runtimes => "runtimes",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdminOptions {
    pub token_stdin: bool,
}

const USAGE: &str = "用法: idoris [serve [--admin-token-stdin]] | idoris admin <status|backends|models|roles|runtimes> --token-stdin";

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
        [admin, resource, option] if admin == "admin" && option == "--token-stdin" => {
            let resource = match resource.to_str() {
                Some("status") => AdminResource::Status,
                Some("backends") => AdminResource::Backends,
                Some("models") => AdminResource::Models,
                Some("roles") => AdminResource::Roles,
                Some("runtimes") => AdminResource::Runtimes,
                _ => return Err(USAGE.to_string()),
            };
            Ok(Command::Admin(AdminCommand::Read(
                resource,
                AdminOptions { token_stdin: true },
            )))
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
            Command::Admin(AdminCommand::Read(
                AdminResource::Status,
                AdminOptions { token_stdin: true }
            ))
        );
        for (resource, expected) in [
            ("backends", AdminResource::Backends),
            ("models", AdminResource::Models),
            ("roles", AdminResource::Roles),
            ("runtimes", AdminResource::Runtimes),
        ] {
            assert_eq!(
                parse_args([
                    OsString::from("admin"),
                    OsString::from(resource),
                    OsString::from("--token-stdin")
                ])
                .unwrap(),
                Command::Admin(AdminCommand::Read(
                    expected,
                    AdminOptions { token_stdin: true }
                ))
            );
        }
        assert_eq!(AdminResource::Status.path(), "status");
        assert_eq!(AdminResource::Backends.path(), "backends");
        assert_eq!(AdminResource::Models.path(), "models");
        assert_eq!(AdminResource::Roles.path(), "roles");
        assert_eq!(AdminResource::Runtimes.path(), "runtimes");
        for args in [
            vec![OsString::from("nope")],
            vec![OsString::from("serve"), OsString::from("extra")],
            vec![OsString::from("--admin-token-stdin")],
            vec![OsString::from("admin")],
            vec![OsString::from("admin"), OsString::from("status")],
            vec![
                OsString::from("admin"),
                OsString::from("models"),
                OsString::from("load"),
                OsString::from("--token-stdin"),
            ],
            vec![
                OsString::from("admin"),
                OsString::from("models"),
                OsString::from("unload"),
                OsString::from("--token-stdin"),
            ],
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
