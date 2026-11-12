//! `idoris cloud pair | status | unpair`：个人形态的云账号配对（设备端）。
//!
//! ```text
//! $ export IDORIS_CLOUD_TIER_CONFIG=~/Library/Application\ Support/iDoris/cloud-tier.yaml
//! $ echo 'ABCD-EFGH-JKMN' | idoris cloud pair [--label "Jason's MacBook"]   # 配对码从 stdin 读（一行），不进 argv、不进环境变量
//! $ idoris cloud status                                                    # 是否已配对、账号绑定（成员关系 id）、凭证 id 前缀；绝不显示密钥
//! $ idoris cloud unpair                                                    # 云端撤销 + 删除本机凭证
//! ```
//!
//! 这些命令**只读写设备凭证文件**（`<state_dir>/device.json`，见 [`super::credential`]），不打开 SQLite 库、不碰 Admin、
//! 不碰本地数据密钥。stdout 是 JSON（机器可读，没有任何密钥）；给人看的说明和警告写 stderr。
//!
//! # 退出码
//!
//! | 码 | 含义 |
//! |---|---|
//! | 0 | 成功（`status` 能读出状态；`unpair` 云端确认已撤销或本来就没有配对） |
//! | 1 | 失败（配对码被拒、云端不可达、本机写不进去、配置有问题……）；stderr 有原因 |
//! | 2 | `unpair` 部分完成：**本机凭证已删除，但云端没有确认撤销**（不可达、报错）。云端那张凭证可能仍然有效，需要在控制面撤销（stderr 有凭证 id 前缀） |

use std::io::{BufRead, Read, Write};
use std::path::Path;

use serde_json::json;

use super::config::{CONFIG_ENV, CloudTierConfig, CredentialSourceConfig};
use super::credential::{
    CredentialProvider, DeviceRecord, DeviceStore, NodeEnvProvider, NodeFileProvider,
};
use super::pairing::{DEFAULT_DEVICE_LABEL, PairingClient, PairingError, validate_device_label};

/// 配对码输入的最大长度：码是 12 个字符加连字符，`pairing_codes::normalize` 的上限是 64。
const MAX_CODE_INPUT: usize = 64;

/// `idoris cloud …` 的子命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudCommand {
    Pair { label: Option<String> },
    Status,
    Unpair,
}

/// 读环境变量 [`CONFIG_ENV`] 指向的配置，并取数据库路径（只用来定位设备凭证目录）。
pub fn load_from_env() -> Result<(CloudTierConfig, std::path::PathBuf), String> {
    let path = crate::config::optional_env_path(CONFIG_ENV)?.ok_or_else(|| {
        format!(
            "云端档没有配置：请设置 {CONFIG_ENV} 指向云端档配置文件（它写明云端地址；没有它就没有云端档）"
        )
    })?;
    let config = CloudTierConfig::load(&path)?;
    let db = crate::storage::process_db_path()?;
    Ok((config, db))
}

/// 从命令行参数解析 `idoris cloud …`（`args` 是 `cloud` 之后的部分）。
pub fn parse(args: &[String]) -> Result<CloudCommand, String> {
    const USAGE: &str =
        "idoris cloud pair [--label <设备名>] | idoris cloud status | idoris cloud unpair";
    match args {
        [command] if command == "status" => Ok(CloudCommand::Status),
        [command] if command == "unpair" => Ok(CloudCommand::Unpair),
        [command] if command == "pair" => Ok(CloudCommand::Pair { label: None }),
        [command, flag, label] if command == "pair" && flag == "--label" => {
            Ok(CloudCommand::Pair {
                label: Some(label.clone()),
            })
        }
        _ => Err(USAGE.to_string()),
    }
}

fn paired_device_only(config: &CloudTierConfig, what: &str) -> Result<(), String> {
    match config.credential {
        CredentialSourceConfig::PairedDevice => Ok(()),
        _ => Err(format!(
            "{what}只用于个人形态的设备配对（credential.source: paired_device）；\
             这份配置用的是 Node 凭证（组织形态），由运维用云控制面签发，不需要配对"
        )),
    }
}

/// 从 stdin 读一行配对码。不回显、不记录；超长、空、非 UTF-8 都是错误。
fn read_code(stdin: &mut dyn BufRead) -> Result<String, String> {
    let mut line = Vec::new();
    // 多读一个字节：能分辨「刚好上限」和「超长」
    let mut limited = Read::take(&mut *stdin, MAX_CODE_INPUT as u64 + 2);
    limited
        .read_until(b'\n', &mut line)
        .map_err(|_| "无法从 stdin 读取配对码".to_string())?;
    let text =
        std::str::from_utf8(&line).map_err(|_| "stdin 里的配对码不是有效的文本".to_string())?;
    let code = text.trim();
    if code.is_empty() {
        return Err(
            "没有从 stdin 读到配对码（用法：echo '配对码' | idoris cloud pair）".to_string(),
        );
    }
    if code.len() > MAX_CODE_INPUT {
        return Err("配对码太长，不是有效的配对码".to_string());
    }
    Ok(code.to_string())
}

/// 一次 CLI 调用共用的上下文：配置、数据库路径（只用来定位设备凭证目录）、HTTP 客户端、「现在」。
pub struct CliContext<'a> {
    pub config: &'a CloudTierConfig,
    pub db_path: &'a Path,
    pub http: reqwest::Client,
    pub now_ms: i64,
}

/// `idoris cloud pair`。
pub async fn run_pair(
    ctx: CliContext<'_>,
    label: Option<&str>,
    stdin: &mut dyn BufRead,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<u8, String> {
    let CliContext {
        config,
        db_path,
        http,
        now_ms,
    } = ctx;
    paired_device_only(config, "idoris cloud pair ")?;
    let label = label.unwrap_or(DEFAULT_DEVICE_LABEL);
    validate_device_label(label)?;
    let store = DeviceStore::new(config.state_dir_for(db_path));

    // 已经配对（或者有一个不可用的凭证文件）：不兑换。配对码是一次性的，不能在已经有凭证的机器上烧掉一个。
    match store.read() {
        Ok(Some(existing)) => {
            let summary = existing.summary();
            return Err(format!(
                "这台机器已经配对了（成员关系 {}，凭证 {}…）：先运行 idoris cloud unpair，再配对新的账号",
                summary.membership, summary.credential_id_prefix
            ));
        }
        Ok(None) => {}
        Err(error) => {
            return Err(format!(
                "本机已经有一个设备凭证文件但它不可用（{error}）：请先运行 idoris cloud unpair（会删除它），再重新配对"
            ));
        }
    }
    // 兑换之前确认能写：兑换是一次性的
    store.preflight().map_err(|error| {
        format!(
            "设备凭证目录 \"{}\" 不能写入（{error}）：没有兑换配对码，请先解决再重试",
            store.dir().display()
        )
    })?;
    let code = read_code(stdin)?;

    let client = PairingClient::new(http, config.control_url.clone());
    let redeemed = client
        .redeem(&code, label)
        .await
        .map_err(|error| format!("配对失败：{error}"))?;
    let record = DeviceRecord {
        origin: config.origin(),
        credential_id: redeemed.credential_id.clone(),
        membership: redeemed.membership.clone(),
        pairing_id: redeemed.pairing_id.clone(),
        paired_at_ms: now_ms,
        secret: redeemed.secret.clone(),
    };
    if let Err(error) = store.write(&record) {
        // 兑换成功但写不下：立刻用手里的凭证把云端那张撤销，免得它成了没人持有的孤儿
        let revoked = client.unpair(&redeemed.secret).await;
        let tail = match revoked {
            Ok(()) | Err(PairingError::CredentialNotAccepted) => {
                "云端那张凭证已经撤销，没有留下孤儿凭证；请生成新的配对码重试".to_string()
            }
            Err(other) => format!(
                "云端那张凭证（{}…）**没有**能撤销（{other}）：请在云账号的控制面撤销它，并生成新的配对码重试",
                record.summary().credential_id_prefix
            ),
        };
        return Err(format!("兑换成功但本机无法保存凭证（{error}）。{tail}"));
    }
    let summary = record.summary();
    let _ = writeln!(
        stderr,
        "已配对：云账号绑定 {}，凭证 {}…（密钥只保存在本机的 0600 文件里，不会显示）",
        summary.membership, summary.credential_id_prefix
    );
    let out = json!({
        "paired": true,
        "membership": summary.membership,
        "credential_id_prefix": summary.credential_id_prefix,
        "origin": summary.origin,
        "paired_at_ms": summary.paired_at_ms,
    });
    writeln!(stdout, "{out}").map_err(|_| "无法写 stdout".to_string())?;
    Ok(0)
}

/// `idoris cloud status`：只读，不联网。
pub fn run_status(
    config: &CloudTierConfig,
    db_path: &Path,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<u8, String> {
    let (value, code) = match &config.credential {
        CredentialSourceConfig::PairedDevice => {
            let store = DeviceStore::new(config.state_dir_for(db_path));
            match store.read() {
                Ok(Some(record)) => {
                    let summary = record.summary();
                    let same_origin = summary.origin == config.origin();
                    (
                        json!({
                            "enabled": true,
                            "credential_source": "paired_device",
                            "paired": true,
                            "membership": summary.membership,
                            "credential_id_prefix": summary.credential_id_prefix,
                            "origin": summary.origin,
                            "paired_at_ms": summary.paired_at_ms,
                            "origin_matches_config": same_origin,
                        }),
                        0,
                    )
                }
                Ok(None) => (
                    json!({"enabled": true, "credential_source": "paired_device", "paired": false}),
                    0,
                ),
                Err(error) => {
                    let _ = writeln!(stderr, "设备凭证不可用：{error}");
                    (
                        json!({
                            "enabled": true,
                            "credential_source": "paired_device",
                            "paired": false,
                            "problem": error.code(),
                        }),
                        1,
                    )
                }
            }
        }
        CredentialSourceConfig::NodeFile(path) => node_status(
            "node_file",
            &NodeFileProvider::new(path.clone(), None),
            stderr,
        ),
        CredentialSourceConfig::NodeEnv(name) => node_status(
            "node_env",
            &NodeEnvProvider::new(std::env::var(name).ok(), None),
            stderr,
        ),
    };
    writeln!(stdout, "{value}").map_err(|_| "无法写 stdout".to_string())?;
    Ok(code)
}

fn node_status(
    source: &'static str,
    provider: &dyn CredentialProvider,
    stderr: &mut dyn Write,
) -> (serde_json::Value, u8) {
    match provider.load() {
        Ok(_) => (
            json!({"enabled": true, "credential_source": source, "credential_usable": true}),
            0,
        ),
        Err(error) => {
            let _ = writeln!(stderr, "Node 凭证不可用：{error}");
            (
                json!({
                    "enabled": true,
                    "credential_source": source,
                    "credential_usable": false,
                    "problem": error.code(),
                }),
                1,
            )
        }
    }
}

/// `idoris cloud unpair`：云端撤销 + 删除本机凭证。**本机凭证无论如何都会删除**。
pub async fn run_unpair(
    ctx: CliContext<'_>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<u8, String> {
    let CliContext {
        config,
        db_path,
        http,
        ..
    } = ctx;
    paired_device_only(config, "idoris cloud unpair ")?;
    let store = DeviceStore::new(config.state_dir_for(db_path));
    let (record, unreadable) = match store.read() {
        Ok(Some(record)) => (Some(record), None),
        Ok(None) => {
            let _ = writeln!(stderr, "这台机器没有配对，什么都没做。");
            writeln!(
                stdout,
                "{}",
                json!({"unpaired": false, "reason": "not_paired"})
            )
            .map_err(|_| "无法写 stdout".to_string())?;
            return Ok(0);
        }
        Err(error) => (None, Some(error)),
    };

    // 云端那一步：只在凭证可用、并且和配置的云端是同一个源时才做——凭证只会发往签发它的源。
    let mut cloud_confirmed = false;
    let mut cloud_note: Option<String> = None;
    let mut id_prefix: Option<String> = None;
    match (&record, unreadable) {
        (Some(record), _) => {
            id_prefix = Some(record.summary().credential_id_prefix);
            if record.origin != config.origin() {
                cloud_note = Some(
                    "配置里的云端地址和这张凭证签发的地址不一致，凭证没有被发出去".to_string(),
                );
            } else {
                let client = PairingClient::new(http, config.control_url.clone());
                match client.unpair(&record.secret).await {
                    Ok(()) => cloud_confirmed = true,
                    Err(PairingError::CredentialNotAccepted) => {
                        cloud_confirmed = true;
                        let _ = writeln!(
                            stderr,
                            "云端已经不认这张凭证（之前就被撤销或已过期），本机凭证现在删除。"
                        );
                    }
                    Err(error) => cloud_note = Some(error.to_string()),
                }
            }
        }
        (None, Some(error)) => {
            cloud_note = Some(format!(
                "本机的设备凭证文件不可用（{error}），读不出密钥，无法通知云端"
            ));
        }
        (None, None) => {}
    }

    // 本机：无论云端结果如何都删除
    let removed = store.remove();
    match removed {
        Ok(_) => {}
        Err(error) => {
            return Err(format!(
                "本机设备凭证文件没有删掉（{error}）：{}请手动删除 \"{}\"",
                if cloud_confirmed {
                    "云端已经撤销，但"
                } else {
                    "云端也没有确认撤销，"
                },
                store.path().display()
            ));
        }
    }

    let prefix = id_prefix.clone().unwrap_or_else(|| "?".to_string());
    if cloud_confirmed {
        writeln!(
            stdout,
            "{}",
            json!({"unpaired": true, "cloud_revoked": true, "credential_id_prefix": id_prefix})
        )
        .map_err(|_| "无法写 stdout".to_string())?;
        let _ = writeln!(stderr, "已解绑：云端已撤销这张凭证，本机凭证已删除。");
        return Ok(0);
    }
    let note = cloud_note.unwrap_or_else(|| "未知原因".to_string());
    let _ = writeln!(
        stderr,
        "本机凭证已删除，但云端没有确认撤销（{note}）。\n\
         云端那张凭证（{prefix}…）可能仍然有效：请在云账号的控制面撤销它\
         （POST /control/v1/credentials/<凭证 id>/status，status 设为 revoked），或让云账号的管理员撤销。"
    );
    writeln!(
        stdout,
        "{}",
        json!({
            "unpaired": true,
            "cloud_revoked": false,
            "credential_id_prefix": id_prefix,
            "local_credential_deleted": true,
            "reason": cloud_note_code(&note),
        })
    )
    .map_err(|_| "无法写 stdout".to_string())?;
    Ok(2)
}

/// 给 stdout 的机器可读原因：不把人话塞进 JSON，只留一个稳定码。
fn cloud_note_code(note: &str) -> &'static str {
    if note.contains("没有被发出去") {
        "origin_mismatch"
    } else if note.contains("不可用") {
        "credential_file_unusable"
    } else {
        "cloud_not_confirmed"
    }
}

/// `idoris cloud …` 的入口（给 `bin/idoris.rs`）。返回退出码；`Err` 是一行错误（退出码 1）。
pub async fn run(command: CloudCommand) -> Result<u8, String> {
    let (config, db_path) = load_from_env()?;
    let http = idoris_upstream::http_client().map_err(|_| "无法初始化 HTTP 客户端".to_string())?;
    let now_ms = {
        use crate::cloud::Clock;
        crate::cloud::SystemClock.now().0
    };
    let stdout = &mut std::io::stdout().lock();
    let stderr = &mut std::io::stderr().lock();
    let ctx = CliContext {
        config: &config,
        db_path: &db_path,
        http,
        now_ms,
    };
    match command {
        CloudCommand::Pair { label } => {
            let stdin = &mut std::io::stdin().lock();
            run_pair(ctx, label.as_deref(), stdin, stdout, stderr).await
        }
        CloudCommand::Status => run_status(&config, &db_path, stdout, stderr),
        CloudCommand::Unpair => run_unpair(ctx, stdout, stderr).await,
    }
}
