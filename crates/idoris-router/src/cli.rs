//! Minimal command-line surface for the packaged `idoris` binary.

use std::ffi::OsString;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Serve(ServeOptions),
    Admin(AdminCommand),
    Key(KeyCommand),
    SetupOmlx(SetupOptions),
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

/// `idoris setup <engine>` 的选项（目前由 `setup omlx` 使用）：探测或托管一个本地推理引擎，生成组件目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupOptions {
    pub target: SetupTarget,
    pub out_dir: std::path::PathBuf,
    /// `(role, model_id)` in command-line order; roles are unique.
    pub bindings: Vec<(String, String)>,
    pub load_mode: idoris_contracts::load_policy::LoadMode,
    pub api_key_stdin: bool,
    pub force: bool,
}

/// 配置的引擎从哪里来（规范 §2.7.8）。三种来源互斥，解析时就排除了不合法的组合，
/// 所以后面的流程里没有「又给了端点又要自动探测」这种状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupTarget {
    /// `--endpoint <url>`：采纳这个端点上已有的引擎。`probe == false` 是 `--no-probe`：不连引擎。
    Endpoint { url: String, probe: bool },
    /// `--auto`：探测候选端口（只发匿名请求，不接受 `--api-key-stdin`），找到就采纳；`managed` 有值（同时给了
    /// `--managed`）时，没找到才托管。
    Auto { managed: Option<ManagedOptions> },
    /// 只给了 `--managed`：由 iDoris 托管（不连引擎，`--no-probe` 隐含）。
    Managed(ManagedOptions),
}

/// `--managed` 的参数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ManagedOptions {
    /// `--managed-port`；`None` 时在私有端口范围里挑一个此刻空闲的。
    pub port: Option<u16>,
}

fn parse_setup(args: &[OsString]) -> Result<SetupOptions, String> {
    use idoris_contracts::load_policy::LoadMode;
    let (mut endpoint, mut out_dir, mut load_mode) = (None, None, None);
    let (mut bindings, mut api_key_stdin, mut force) = (Vec::new(), false, false);
    let (mut auto, mut managed, mut no_probe, mut managed_port) = (false, false, false, None);
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let flag = flag.to_str().ok_or_else(|| USAGE.to_string())?;
        match flag {
            "--api-key-stdin" if !api_key_stdin => api_key_stdin = true,
            "--force" if !force => force = true,
            "--auto" if !auto => auto = true,
            "--managed" if !managed => managed = true,
            "--no-probe" if !no_probe => no_probe = true,
            "--endpoint" | "--out-dir" | "--bind" | "--load-mode" | "--managed-port" => {
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
                    "--managed-port" if managed_port.is_none() => {
                        managed_port = Some(parse_port(value.to_str().ok_or(USAGE)?)?)
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
    let managed_options = ManagedOptions { port: managed_port };
    // 三种来源恰好选一种：`--endpoint`、`--auto`（可再加 `--managed` 作为没找到时的退路）、单独的 `--managed`。
    // `--endpoint` 与 `--managed` 同时给出没有意义（托管的端点取自 `--managed-port`，规范 §2.7.8），不静默取其一。
    let target = match (endpoint, auto, managed) {
        (Some(url), false, false) => SetupTarget::Endpoint {
            url,
            probe: !no_probe,
        },
        (None, true, _) if !no_probe => SetupTarget::Auto {
            managed: managed.then_some(managed_options),
        },
        (None, false, true) => SetupTarget::Managed(managed_options),
        _ => return Err(USAGE.to_string()),
    };
    // `--managed-port` 只对 `--managed` 有意义。
    if managed_port.is_some() && !managed {
        return Err(USAGE.to_string());
    }
    // key 只给一个确定的端点：`--auto` 探测的端口是猜的，端口属主校验（#619）落地之前不把 key 发给它们；
    // 不探测的运行用不上 key。两种情形给了 key 都是用法错误（不读 stdin 再悄悄丢掉）。
    let key_allowed = matches!(&target, SetupTarget::Endpoint { probe: true, .. });
    if api_key_stdin && !key_allowed {
        return Err(USAGE.to_string());
    }
    Ok(SetupOptions {
        target,
        out_dir: out_dir.ok_or_else(|| USAGE.to_string())?,
        bindings,
        load_mode: load_mode.unwrap_or(LoadMode::OnDemand),
        api_key_stdin,
        force,
    })
}

/// 端口：十进制数字，1 到 65535（与 `--allocated-budget-bytes` 同样不接受符号、空白、小数）。
fn parse_port(value: &str) -> Result<u16, String> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(USAGE.to_string());
    }
    match value.parse::<u16>() {
        Ok(port) if port != 0 => Ok(port),
        _ => Err(USAGE.to_string()),
    }
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

const USAGE: &str = "用法: idoris [serve [--admin-token-stdin | --launcher-stdin]] | idoris admin <status|backends|models|roles|runtimes> --token-stdin | idoris key issue --spec-stdin | idoris key list | idoris key revoke <key_id> | idoris control bootstrap --spec-stdin | idoris cloud pair [--label <设备名>] | idoris cloud status | idoris cloud unpair | idoris recommend --role chat --json [--allocated-budget-bytes <u64>] | idoris models local --json | idoris setup omlx --out-dir <dir> (--endpoint <url> [--api-key-stdin] | --auto | --managed) [--managed-port <n>] [--bind <role>=<model>]... [--load-mode on_demand|resident] [--force] [--no-probe]";

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
            parse_setup(rest).map(Command::SetupOmlx)
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

    fn setup(args: &[&str]) -> Result<SetupOptions, String> {
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
        assert_eq!(
            ok.target,
            SetupTarget::Endpoint {
                url: "http://127.0.0.1:8088".into(),
                probe: true
            }
        );
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

    fn target(args: &[&str]) -> SetupTarget {
        let mut all = vec!["--out-dir", "o"];
        all.extend_from_slice(args);
        setup(&all).unwrap().target
    }

    fn managed(port: Option<u16>) -> ManagedOptions {
        ManagedOptions { port }
    }

    /// 规范 §2.7.8：`[--endpoint <url> | --auto] [--managed] [--managed-port <n>] … [--no-probe]`。
    #[test]
    fn parses_the_three_setup_targets_and_their_modifiers() {
        assert_eq!(
            target(&["--endpoint", "e"]),
            SetupTarget::Endpoint {
                url: "e".into(),
                probe: true
            }
        );
        assert_eq!(
            target(&["--endpoint", "e", "--no-probe"]),
            SetupTarget::Endpoint {
                url: "e".into(),
                probe: false
            }
        );
        assert_eq!(
            target(&["--auto"]),
            SetupTarget::Auto { managed: None },
            "没有 --managed：没找到就报错，不托管"
        );
        assert_eq!(
            target(&["--auto", "--managed"]),
            SetupTarget::Auto {
                managed: Some(managed(None))
            }
        );
        assert_eq!(
            target(&["--managed", "--auto", "--managed-port", "19731"]),
            SetupTarget::Auto {
                managed: Some(managed(Some(19731)))
            },
            "旗标顺序不限"
        );
        assert_eq!(target(&["--managed"]), SetupTarget::Managed(managed(None)));
        assert_eq!(
            target(&["--managed", "--managed-port", "65535"]),
            SetupTarget::Managed(managed(Some(65535)))
        );
        // `--no-probe` 对 `--managed` 是隐含的：明说也行。
        assert_eq!(
            target(&["--managed", "--no-probe"]),
            SetupTarget::Managed(managed(None))
        );
        // 其余选项与目标无关。
        let both = setup(&["--auto", "--out-dir", "o", "--force", "--bind", "daily=m"]).unwrap();
        assert!(both.force && !both.api_key_stdin);
        assert_eq!(both.bindings, [("daily".into(), "m".into())]);
        // key 只配一个确定的、要探测的端点。
        assert!(
            setup(&["--endpoint", "e", "--out-dir", "o", "--api-key-stdin"])
                .unwrap()
                .api_key_stdin
        );
    }

    /// 每一种不合法的组合都是用法错误，不是静默取其一。
    #[test]
    fn rejects_contradictory_setup_targets_and_modifiers() {
        for bad in [
            // 没有来源。
            vec!["--out-dir", "o", "--no-probe"],
            vec!["--out-dir", "o", "--managed-port", "19731"],
            // 来源互斥：`--endpoint` 与 `--auto`；`--endpoint` 与 `--managed`（托管的端点取自 `--managed-port`）。
            vec!["--out-dir", "o", "--endpoint", "e", "--auto"],
            vec!["--out-dir", "o", "--auto", "--endpoint", "e"],
            vec!["--out-dir", "o", "--endpoint", "e", "--managed"],
            vec!["--out-dir", "o", "--endpoint", "e", "--auto", "--managed"],
            // `--no-probe` 与 `--auto`（自动探测就是探测）。
            vec!["--out-dir", "o", "--auto", "--no-probe"],
            vec!["--out-dir", "o", "--auto", "--managed", "--no-probe"],
            // `--managed-port` 只跟 `--managed`。
            vec![
                "--out-dir",
                "o",
                "--endpoint",
                "e",
                "--managed-port",
                "19731",
            ],
            vec!["--out-dir", "o", "--auto", "--managed-port", "19731"],
            // key 只配一个确定的、要探测的端点：`--auto`（端口是猜的，#619 之前不发 key）、托管、不探测都不行。
            vec!["--out-dir", "o", "--auto", "--api-key-stdin"],
            vec!["--out-dir", "o", "--api-key-stdin", "--auto", "--managed"],
            vec!["--out-dir", "o", "--managed", "--api-key-stdin"],
            vec![
                "--out-dir",
                "o",
                "--endpoint",
                "e",
                "--no-probe",
                "--api-key-stdin",
            ],
            // 重复的旗标。
            vec!["--out-dir", "o", "--auto", "--auto"],
            vec!["--out-dir", "o", "--managed", "--managed"],
            vec![
                "--out-dir",
                "o",
                "--endpoint",
                "e",
                "--no-probe",
                "--no-probe",
            ],
            vec![
                "--out-dir",
                "o",
                "--managed",
                "--managed-port",
                "19731",
                "--managed-port",
                "19732",
            ],
        ] {
            assert_eq!(setup(&bad).unwrap_err(), USAGE, "{bad:?}");
        }
    }

    #[test]
    fn the_managed_port_is_a_plain_decimal_between_1_and_65535() {
        for good in ["1", "1024", "19000", "65535"] {
            assert_eq!(
                target(&["--managed", "--managed-port", good]),
                SetupTarget::Managed(managed(Some(good.parse().unwrap()))),
                "{good}"
            );
        }
        for bad in [
            "", "0", "65536", "100000", "-1", "+1", " 1", "1 ", "1.0", "0x10", "1e3", "八八",
        ] {
            assert_eq!(
                setup(&["--out-dir", "o", "--managed", "--managed-port", bad]).unwrap_err(),
                USAGE,
                "{bad:?}"
            );
        }
        // 值缺失。
        assert_eq!(
            setup(&["--out-dir", "o", "--managed", "--managed-port"]).unwrap_err(),
            USAGE
        );
    }

    #[test]
    fn the_usage_text_lists_the_new_setup_flags() {
        for flag in ["--auto", "--managed", "--managed-port", "--no-probe"] {
            assert!(USAGE.contains(flag), "{flag}");
        }
    }
}
