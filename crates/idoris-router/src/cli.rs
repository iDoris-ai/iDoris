//! Minimal command-line surface for the packaged `idoris` binary.

use std::ffi::OsString;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Serve(ServeOptions),
    Admin(AdminCommand),
    Key(KeyCommand),
    SetupOmlx(SetupOmlxOptions),
}

/// `idoris setup omlx` — offline probe + components-dir generator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupOmlxOptions {
    pub endpoint: String,
    pub out_dir: std::path::PathBuf,
    /// `(role, model_id)` in command-line order; roles are unique.
    pub bindings: Vec<(String, String)>,
    pub load_mode: idoris_contracts::load_policy::LoadMode,
    pub api_key_stdin: bool,
    pub force: bool,
}

fn parse_setup_omlx(args: &[OsString]) -> Result<SetupOmlxOptions, String> {
    use idoris_contracts::load_policy::LoadMode;
    let (mut endpoint, mut out_dir, mut load_mode) = (None, None, None);
    let (mut bindings, mut api_key_stdin, mut force) = (Vec::new(), false, false);
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let flag = flag.to_str().ok_or_else(|| USAGE.to_string())?;
        match flag {
            "--api-key-stdin" if !api_key_stdin => api_key_stdin = true,
            "--force" if !force => force = true,
            "--endpoint" | "--out-dir" | "--bind" | "--load-mode" => {
                let value = it.next().ok_or_else(|| USAGE.to_string())?;
                match flag {
                    "--endpoint" if endpoint.is_none() => {
                        endpoint = Some(value.to_str().ok_or(USAGE)?.to_string())
                    }
                    "--out-dir" if out_dir.is_none() => out_dir = Some(value.into()),
                    "--load-mode" if load_mode.is_none() => {
                        load_mode = Some(match value.to_str() {
                            Some("on_demand") => LoadMode::OnDemand,
                            Some("resident") => LoadMode::Resident,
                            _ => return Err(USAGE.to_string()),
                        })
                    }
                    "--bind" => {
                        let (role, model) = value
                            .to_str()
                            .and_then(|v| v.split_once('='))
                            .filter(|(r, m)| !r.is_empty() && !m.is_empty())
                            .ok_or_else(|| USAGE.to_string())?;
                        if bindings.iter().any(|(r, _): &(String, String)| r == role) {
                            return Err(USAGE.to_string());
                        }
                        bindings.push((role.to_string(), model.to_string()));
                    }
                    _ => return Err(USAGE.to_string()),
                }
            }
            _ => return Err(USAGE.to_string()),
        }
    }
    Ok(SetupOmlxOptions {
        endpoint: endpoint.ok_or_else(|| USAGE.to_string())?,
        out_dir: out_dir.ok_or_else(|| USAGE.to_string())?,
        bindings,
        load_mode: load_mode.unwrap_or(LoadMode::OnDemand),
        api_key_stdin,
        force,
    })
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCommand {
    Issue,
}

const USAGE: &str = "用法: idoris [serve [--admin-token-stdin]] | idoris admin <status|backends|models|roles|runtimes> --token-stdin | idoris key issue --spec-stdin | idoris setup omlx --endpoint <url> --out-dir <dir> [--bind <role>=<model>]... [--load-mode on_demand|resident] [--api-key-stdin] [--force]";

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
        [key, issue, option] if key == "key" && issue == "issue" && option == "--spec-stdin" => {
            Ok(Command::Key(KeyCommand::Issue))
        }
        [setup, omlx, rest @ ..] if setup == "setup" && omlx == "omlx" => {
            parse_setup_omlx(rest).map(Command::SetupOmlx)
        }
        _ => Err(USAGE.to_string()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn parses_serve_admin_and_exact_key_issue_forms() {
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
        assert_eq!(
            parse_args([
                OsString::from("key"),
                OsString::from("issue"),
                OsString::from("--spec-stdin")
            ])
            .unwrap(),
            Command::Key(KeyCommand::Issue)
        );
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
            vec![OsString::from("key"), OsString::from("issue")],
            vec![
                OsString::from("key"),
                OsString::from("issue"),
                OsString::from("--secret"),
            ],
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

    fn setup(args: &[&str]) -> Result<SetupOmlxOptions, String> {
        let mut all = vec![OsString::from("setup"), OsString::from("omlx")];
        all.extend(args.iter().map(OsString::from));
        match parse_args(all)? {
            Command::SetupOmlx(options) => Ok(options),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_setup_omlx_flags_and_rejects_bad_forms() {
        use idoris_contracts::load_policy::LoadMode;
        let ok = setup(&[
            "--endpoint",
            "http://127.0.0.1:8088",
            "--out-dir",
            "o",
            "--bind",
            "daily=a=b",
            "--bind",
            "fast=m",
            "--load-mode",
            "resident",
            "--api-key-stdin",
            "--force",
        ])
        .unwrap();
        assert_eq!(ok.endpoint, "http://127.0.0.1:8088");
        assert_eq!(
            ok.bindings,
            [("daily".into(), "a=b".into()), ("fast".into(), "m".into())]
        );
        assert_eq!(ok.load_mode, LoadMode::Resident);
        assert!(ok.api_key_stdin && ok.force);
        let min = setup(&["--endpoint", "e", "--out-dir", "o"]).unwrap();
        assert_eq!(min.load_mode, LoadMode::OnDemand);
        assert!(min.bindings.is_empty() && !min.api_key_stdin && !min.force);
        for bad in [
            vec!["--out-dir", "o"],
            vec!["--endpoint", "e"],
            vec!["--endpoint", "e", "--out-dir", "o", "--nope"],
            vec![
                "--endpoint",
                "e",
                "--out-dir",
                "o",
                "--bind",
                "daily=a",
                "--bind",
                "daily=b",
            ],
            vec!["--endpoint", "e", "--out-dir", "o", "--bind", "daily"],
            vec!["--endpoint", "e", "--out-dir", "o", "--bind", "=m"],
            vec!["--endpoint", "e", "--endpoint", "f", "--out-dir", "o"],
            vec![
                "--endpoint",
                "e",
                "--out-dir",
                "o",
                "--load-mode",
                "evict_to_load",
            ],
            vec![
                "--endpoint",
                "e",
                "--out-dir",
                "o",
                "--api-key-stdin",
                "--api-key-stdin",
            ],
            vec!["--endpoint", "e", "--out-dir"],
            vec!["--endpoint", "e", "--out-dir", "o", "--api-key", "secret"],
        ] {
            assert_eq!(setup(&bad).unwrap_err(), USAGE, "{bad:?}");
        }
    }
}
