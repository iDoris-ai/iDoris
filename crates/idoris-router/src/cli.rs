//! Minimal command-line surface for the packaged `idoris` binary.

use std::ffi::OsString;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Serve(ServeOptions),
    Admin(AdminCommand),
    Key(KeyCommand),
    SetupOmlx(SetupOmlxOptions),
    /// `idoris recommend --role chat --json [--allocated-budget-bytes <u64>]`（接口规范 §2.9）。
    /// 只读本机文件系统；不开数据库、不取实例锁、不读任何密钥。
    Recommend(RecommendOptions),
    /// `idoris models local --json`（接口规范 §2.9）。运行条件同 `recommend`。
    ModelsLocal,
    /// Local operator commands of the compute-service control plane (S2); organization and cloud forms only.
    Control(ControlCommand),
    /// `idoris cloud pair|status|unpair`: the device side of the cloud-account pairing (S7, personal form).
    Cloud(crate::cloud_tier::cli::CloudCommand),
}

/// `idoris control …` — local operator commands. They run with the authority of whoever owns the
/// database file and are the only way to create the platform root and the first administrative credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlCommand {
    /// `idoris control bootstrap --spec-stdin`: accounts, allocations and credentials from a JSON spec.
    Bootstrap,
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

/// `role` 保持原样交给运行期判断：不支持的角色要给出明确的一行说明，而不是笼统的用法提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecommendOptions {
    pub role: String,
    /// `--allocated-budget-bytes`；缺省时由调用方退到环境变量 `IDORIS_ALLOCATED_BUDGET_BYTES`。
    pub allocated_budget_bytes: Option<u64>,
}

fn parse_recommend(args: &[OsString]) -> Result<RecommendOptions, String> {
    let (mut role, mut budget, mut json) = (None, None, false);
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.to_str().ok_or_else(|| USAGE.to_string())? {
            "--json" if !json => json = true,
            "--role" if role.is_none() => {
                let value = it.next().and_then(|v| v.to_str()).ok_or(USAGE)?;
                role = Some(value.to_string());
            }
            "--allocated-budget-bytes" if budget.is_none() => {
                let value = it.next().and_then(|v| v.to_str()).ok_or(USAGE)?;
                // 与 IDORIS_ALLOCATED_BUDGET_BYTES 同口径：无符号整数字节，不接受符号、空白、小数。
                if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(USAGE.to_string());
                }
                budget = Some(value.parse::<u64>().map_err(|_| USAGE.to_string())?);
            }
            _ => return Err(USAGE.to_string()),
        }
    }
    match (role, json) {
        (Some(role), true) => Ok(RecommendOptions {
            role,
            allocated_budget_bytes: budget,
        }),
        _ => Err(USAGE.to_string()),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServeOptions {
    /// `--admin-token-stdin`：stdin 上一帧 65 字节（64 位小写 hex 加 LF）的 Admin token。v0.4.0 起的既有契约，不变。
    pub admin_token_stdin: bool,
    /// `--launcher-stdin`：stdin 上一行 UTF-8 JSON 帧（接口规范 §2.7.1，修订 1.6），带 Admin token 与发现文件、
    /// 登记、引擎等启动输入。与 `--admin-token-stdin` 互斥：同时给出是用法错误。
    pub launcher_stdin: bool,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyCommand {
    Issue,
    List,
    Revoke(String),
}

const USAGE: &str = "用法: idoris [serve [--admin-token-stdin | --launcher-stdin]] | idoris admin <status|backends|models|roles|runtimes> --token-stdin | idoris key issue --spec-stdin | idoris key list | idoris key revoke <key_id> | idoris control bootstrap --spec-stdin | idoris cloud pair [--label <设备名>] | idoris cloud status | idoris cloud unpair | idoris recommend --role chat --json [--allocated-budget-bytes <u64>] | idoris models local --json | idoris setup omlx --endpoint <url> --out-dir <dir> [--bind <role>=<model>]... [--load-mode on_demand|resident] [--api-key-stdin] [--force]";

pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let args = args.into_iter().collect::<Vec<_>>();
    match args.as_slice() {
        [] => Ok(Command::Serve(ServeOptions::default())),
        [command] if command == "serve" => Ok(Command::Serve(ServeOptions::default())),
        [command, option] if command == "serve" && option == "--admin-token-stdin" => {
            Ok(Command::Serve(ServeOptions {
                admin_token_stdin: true,
                ..ServeOptions::default()
            }))
        }
        // `--launcher-stdin` 与 `--admin-token-stdin` 互斥：两个 flag 同时出现（任何顺序、重复）没有匹配的分支，
        // 落到最后的 `_` 成为用法错误，退出码 1。
        [command, option] if command == "serve" && option == "--launcher-stdin" => {
            Ok(Command::Serve(ServeOptions {
                launcher_stdin: true,
                ..ServeOptions::default()
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
        [key, list] if key == "key" && list == "list" => Ok(Command::Key(KeyCommand::List)),
        [key, revoke, key_id] if key == "key" && revoke == "revoke" => key_id
            .to_str()
            .filter(|id| crate::key_manage::valid_key_id(id))
            .map(|id| Command::Key(KeyCommand::Revoke(id.to_string())))
            .ok_or_else(|| USAGE.to_string()),
        [control, bootstrap, option]
            if control == "control" && bootstrap == "bootstrap" && option == "--spec-stdin" =>
        {
            Ok(Command::Control(ControlCommand::Bootstrap))
        }
        [recommend, rest @ ..] if recommend == "recommend" => {
            parse_recommend(rest).map(Command::Recommend)
        }
        [models, local, option] if models == "models" && local == "local" && option == "--json" => {
            Ok(Command::ModelsLocal)
        }
        [setup, omlx, rest @ ..] if setup == "setup" && omlx == "omlx" => {
            parse_setup_omlx(rest).map(Command::SetupOmlx)
        }
        [cloud, rest @ ..] if cloud == "cloud" => {
            let rest = rest
                .iter()
                .map(|arg| arg.to_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| USAGE.to_string())?;
            crate::cloud_tier::cli::parse(&rest)
                .map(Command::Cloud)
                .map_err(|_| USAGE.to_string())
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
                admin_token_stdin: true,
                ..ServeOptions::default()
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
        assert_eq!(
            parse_args([OsString::from("key"), OsString::from("list")]).unwrap(),
            Command::Key(KeyCommand::List)
        );
        let id = format!("vk_{}", "a".repeat(32));
        assert_eq!(
            parse_args(["key", "revoke", id.as_str()].map(OsString::from)).unwrap(),
            Command::Key(KeyCommand::Revoke(id.clone()))
        );
        for bad in [
            vec!["key", "list", "extra"],
            vec!["key", "revoke"],
            vec!["key", "revoke", "vk_short"],
            vec!["key", "revoke", "../etc"],
            vec!["key", "revoke", "VK_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"],
            vec!["key", "revoke", id.as_str(), "extra"],
        ] {
            assert_eq!(
                parse_args(bad.into_iter().map(OsString::from)).unwrap_err(),
                USAGE
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

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(OsString::from))
    }

    #[test]
    fn parses_serve_launcher_stdin_and_keeps_the_legacy_token_flag_unchanged() {
        assert_eq!(
            parse(&["serve", "--launcher-stdin"]).unwrap(),
            Command::Serve(ServeOptions {
                admin_token_stdin: false,
                launcher_stdin: true
            })
        );
        // 旧 flag 一字不变。
        assert_eq!(
            parse(&["serve", "--admin-token-stdin"]).unwrap(),
            Command::Serve(ServeOptions {
                admin_token_stdin: true,
                launcher_stdin: false
            })
        );
        assert_eq!(
            parse(&["serve"]).unwrap(),
            Command::Serve(ServeOptions::default())
        );
    }

    /// 接口规范 §2.7.1：两个 stdin flag 互斥，同时给出是用法错误（退出码 1），不是静默取其一。
    #[test]
    fn launcher_stdin_and_admin_token_stdin_are_mutually_exclusive() {
        for args in [
            vec!["serve", "--launcher-stdin", "--admin-token-stdin"],
            vec!["serve", "--admin-token-stdin", "--launcher-stdin"],
            vec!["serve", "--launcher-stdin", "--launcher-stdin"],
            vec!["serve", "--admin-token-stdin", "--admin-token-stdin"],
            vec!["serve", "--launcher-stdin", "extra"],
            vec!["--launcher-stdin"],
            vec!["serve", "--launcher-stdin=1"],
            vec!["serve", "--launcher-stdin", "--discovery-file", "/tmp/x"],
        ] {
            assert_eq!(parse(&args).unwrap_err(), USAGE, "{args:?}");
        }
        assert!(USAGE.contains("--launcher-stdin"));
    }

    #[test]
    fn parses_recommend_and_models_local_forms() {
        assert_eq!(
            parse(&["recommend", "--role", "chat", "--json"]).unwrap(),
            Command::Recommend(RecommendOptions {
                role: "chat".into(),
                allocated_budget_bytes: None
            })
        );
        // 旗标顺序不限；额度可选。
        assert_eq!(
            parse(&[
                "recommend",
                "--allocated-budget-bytes",
                "18446744073709551615",
                "--json",
                "--role",
                "chat"
            ])
            .unwrap(),
            Command::Recommend(RecommendOptions {
                role: "chat".into(),
                allocated_budget_bytes: Some(u64::MAX)
            })
        );
        // 角色值原样保留，由运行期给出专门的错误说明。
        assert_eq!(
            parse(&["recommend", "--role", "vision", "--json"]).unwrap(),
            Command::Recommend(RecommendOptions {
                role: "vision".into(),
                allocated_budget_bytes: None
            })
        );
        assert_eq!(
            parse(&["models", "local", "--json"]).unwrap(),
            Command::ModelsLocal
        );
        for bad in [
            vec!["recommend"],
            vec!["recommend", "--json"],
            vec!["recommend", "--role", "chat"],
            vec!["recommend", "--role", "--json"],
            vec!["recommend", "--role", "chat", "--json", "--json"],
            vec!["recommend", "--role", "chat", "--role", "chat", "--json"],
            vec!["recommend", "--role", "chat", "--json", "extra"],
            vec![
                "recommend",
                "--role",
                "chat",
                "--json",
                "--allocated-budget-bytes",
            ],
            vec![
                "recommend",
                "--role",
                "chat",
                "--json",
                "--allocated-budget-bytes",
                "-1",
            ],
            vec![
                "recommend",
                "--role",
                "chat",
                "--json",
                "--allocated-budget-bytes",
                "+1",
            ],
            vec![
                "recommend",
                "--role",
                "chat",
                "--json",
                "--allocated-budget-bytes",
                " 1",
            ],
            vec![
                "recommend",
                "--role",
                "chat",
                "--json",
                "--allocated-budget-bytes",
                "18446744073709551616",
            ],
            vec![
                "recommend",
                "--role",
                "chat",
                "--json",
                "--allocated-budget-bytes",
                "1",
                "--allocated-budget-bytes",
                "2",
            ],
            vec!["models"],
            vec!["models", "local"],
            vec!["models", "local", "--json", "extra"],
            vec!["models", "remote", "--json"],
        ] {
            assert_eq!(parse(&bad).unwrap_err(), USAGE, "{bad:?}");
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
