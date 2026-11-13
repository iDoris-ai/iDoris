//! `idoris cloud pair | status | unpair`：个人形态的云账号配对（设备端）。
//!
//! ```text
//! $ export IDORIS_CLOUD_TIER_CONFIG=~/Library/Application\ Support/iDoris/cloud-tier.yaml
//! $ echo 'ABCD-EFGH-JKMN' | idoris cloud pair [--label "Jason's MacBook"]   # 配对码从 stdin 读（一行），不进 argv、不进环境变量
//! $ idoris cloud status                                                    # 是否已配对、账号绑定（成员关系 id）、凭证 id 前缀；绝不显示密钥
//! $ idoris cloud unpair                                                    # 云端撤销 + 删除本机凭证
//! ```
//!
//! 这些命令读写设备凭证文件（`<state_dir>/device.json`，见 [`super::credential`]），不碰 Admin、不改本地数据密钥；
//! `unpair` 发凭证之前**只读地**查一下本机的虚拟 key 库，确认它不是本机的本地数据密钥（不创建库、不建表）。
//! `pair` / `unpair` 的整个流程在状态目录的跨进程锁（`<state_dir>/.lock`）里，两个进程不会互相穿插；安装不覆盖已有的凭证，
//! 兑换成功却装不上时（写不下，或者兑换期间别的进程装了一张）立刻撤销新兑换的那张。
//! stdout 是 JSON（机器可读，没有任何密钥）；给人看的说明和警告写 stderr。
//!
//! # 退出码
//!
//! | 码 | 含义 |
//! |---|---|
//! | 0 | 成功（`status` 能读出状态；`unpair` 云端确认已撤销或本来就没有配对） |
//! | 1 | 失败（配对码被拒、云端不可达、本机写不进去、配置有问题……）；stderr 有原因 |
//! | 2 | `unpair` 部分完成：**本机凭证已删除，但云端没有确认撤销**（不可达、报错；或者文件里是本机的本地数据密钥，没有发送：`reason: local_key_refused`）。云端那张凭证可能仍然有效，需要在控制面撤销（stderr 有凭证 id 前缀） |

use std::io::{BufRead, Read, Write};
use std::path::Path;

use serde_json::json;

use super::config::{CONFIG_ENV, CloudTierConfig, CredentialSourceConfig};
use super::credential::{
    CredentialError, CredentialProvider, DeviceRecord, DeviceStore, LocalSecretProbe,
    NodeEnvProvider, NodeFileProvider, StateLock,
};
use super::pairing::{DEFAULT_DEVICE_LABEL, PairingClient, PairingError, validate_device_label};

/// 配对码输入的最大长度：码是 12 个字符加连字符，`pairing_codes::normalize` 的上限是 64。
const MAX_CODE_INPUT: usize = 64;

/// 另一个 `pair` / `unpair` 占着状态目录的锁时，最多等多久再放弃（异步地等，不卡运行时）。
/// 不是排队：并发地配对 / 解绑同一台机器本来就不是正常用法，等一小会儿只是为了让紧挨着的两个命令别互相误伤。
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
const LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// 拿状态目录的跨进程锁（[`DeviceStore::try_lock`]）。配对和解绑的**整个流程**都在它里面：「有没有配对」的检查、
/// 兑换、安装是一个整体，不能被另一个进程的同一个流程穿插（审查 r4 Medium#4）。
/// 别人占着时短暂地等（异步地等），等不到返回 [`CredentialError::Busy`]。
/// `create_dir` 为假（解绑）且目录不存在时返回 `None`：没有目录就没有配对，不用锁。
async fn lock_state_dir(
    store: &DeviceStore,
    create_dir: bool,
) -> Result<Option<StateLock>, CredentialError> {
    let deadline = tokio::time::Instant::now() + LOCK_WAIT;
    loop {
        match store.try_lock(create_dir) {
            Err(CredentialError::Busy) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(LOCK_POLL).await;
            }
            other => return other,
        }
    }
}

fn lock_busy_message(store: &DeviceStore) -> String {
    format!(
        "另一个 idoris cloud pair / unpair 正在这台机器上运行（设备凭证目录 \"{}\" 的锁被占着）：\
         等它结束后重试。这个命令什么都没做（没有兑换配对码，也没有动凭证）",
        store.dir().display()
    )
}

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

/// 一次 CLI 调用共用的上下文：配置、数据库路径（定位设备凭证目录；`unpair` 的本地密钥识别也用它）、HTTP 客户端、「现在」，
/// 以及本地密钥探针。
///
/// `probe` 是**必填**的：所有会把凭证发往云端的入口都要先问「这是不是本机的本地数据密钥」（审查 r4 High#2：解绑以前直接发
/// `record.secret`，没有经过它）。生产里是 [`super::VirtualKeyFileProbe`]（只读地查本机虚拟 key 库，库不可用按「是」处理）。
pub struct CliContext<'a> {
    pub config: &'a CloudTierConfig,
    pub db_path: &'a Path,
    pub http: reqwest::Client,
    pub now_ms: i64,
    pub probe: &'a dyn LocalSecretProbe,
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
        ..
    } = ctx;
    paired_device_only(config, "idoris cloud pair ")?;
    let label = label.unwrap_or(DEFAULT_DEVICE_LABEL);
    validate_device_label(label)?;
    let store = DeviceStore::new(config.state_dir_for(db_path));
    // 整个配对流程（检查、预检、兑换、安装）在状态目录的跨进程锁里：两个进程各兑换一个账号的配对码，
    // 不能都先读到「还没有配对」、再互相覆盖（以前先装的那张云凭证仍然有效、本机却丢了它）。
    let _lock = match lock_state_dir(&store, true).await {
        Ok(lock) => lock,
        Err(CredentialError::Busy) => return Err(lock_busy_message(&store)),
        // 目录建不出来、锁文件写不了：和预检一样，兑换之前就停下（配对码是一次性的）
        Err(error) => {
            return Err(format!(
                "设备凭证目录 \"{}\" 不能写入（{error}）：没有兑换配对码，请先解决再重试",
                store.dir().display()
            ));
        }
    };

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
    if let Err(error) = store.install(&record) {
        // 兑换成功但装不上（写不下，或者兑换期间别的进程已经装了一张——安装不覆盖已有的凭证）：
        // 立刻用手里的新凭证把云端那张撤销，免得它成了没人持有的孤儿
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
        let what = match error {
            CredentialError::AlreadyPaired => {
                "兑换成功，但兑换期间这台机器上已经出现了一张设备凭证（别的进程装的），没有覆盖它"
                    .to_string()
            }
            other => format!("兑换成功但本机无法保存凭证（{other}）"),
        };
        return Err(format!("{what}。{tail}"));
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

/// 解绑时云端没有确认撤销的原因：稳定码给 stdout（机器可读，不塞人话），人话给 stderr。
enum CloudNotConfirmed {
    /// 配置里的云端地址和这张凭证签发的地址不一致：凭证只会发往签发它的源。
    OriginMismatch,
    /// 设备凭证文件不可用（权限、格式……）：读不出密钥。
    FileUnusable(CredentialError),
    /// 凭证文件里的密钥是本机的本地数据密钥（有效、已撤销、已过期都算），或者本机密钥库读不出来、判不了（按「是」处理）：
    /// **没有发出去**。
    LocalKey,
    /// 发了，云端没有确认（不可达、报错）。
    Cloud(PairingError),
}

impl CloudNotConfirmed {
    fn code(&self) -> &'static str {
        match self {
            CloudNotConfirmed::OriginMismatch => "origin_mismatch",
            CloudNotConfirmed::FileUnusable(_) => "credential_file_unusable",
            CloudNotConfirmed::LocalKey => "local_key_refused",
            CloudNotConfirmed::Cloud(_) => "cloud_not_confirmed",
        }
    }

    fn text(&self) -> String {
        match self {
            CloudNotConfirmed::OriginMismatch => {
                "配置里的云端地址和这张凭证签发的地址不一致，凭证没有被发出去".to_string()
            }
            CloudNotConfirmed::FileUnusable(error) => {
                format!("本机的设备凭证文件不可用（{error}），读不出密钥，无法通知云端")
            }
            CloudNotConfirmed::LocalKey => {
                "无法确认这个文件里的密钥不是本机的本地数据密钥（本地密钥永远不发给云端），没有发出去"
                    .to_string()
            }
            CloudNotConfirmed::Cloud(error) => error.to_string(),
        }
    }
}

/// `idoris cloud unpair`：云端撤销 + 删除本机凭证。**本机凭证无论如何都会删除**。
///
/// 凭证发往云端之前和推理路径一样先问本地密钥探针（`ctx.probe`）：认得（本机的本地数据密钥误填进了设备凭证文件）就**不发**，
/// 只删本地文件，按「本机已删、云端未确认」处理（退出码 2，`reason: local_key_refused`）。
pub async fn run_unpair(
    ctx: CliContext<'_>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<u8, String> {
    let CliContext {
        config,
        db_path,
        http,
        probe,
        ..
    } = ctx;
    paired_device_only(config, "idoris cloud unpair ")?;
    let store = DeviceStore::new(config.state_dir_for(db_path));
    // 和配对同一把锁：解绑（读凭证、通知云端、删文件）也是一个整体，不能和一次配对穿插。没有目录就没有配对，不用锁。
    let _lock = match lock_state_dir(&store, false).await {
        Ok(lock) => lock,
        Err(CredentialError::Busy) => return Err(lock_busy_message(&store)),
        Err(error) => {
            return Err(format!(
                "无法获取设备凭证目录 \"{}\" 的锁（{error}）：这个命令什么都没做",
                store.dir().display()
            ));
        }
    };
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

    // 云端那一步：只在凭证可用、和配置的云端是同一个源、并且不是本机的本地数据密钥时才做——凭证只会发往签发它的源，
    // 本地密钥永远不发给云端。
    let mut cloud_confirmed = false;
    let mut not_confirmed: Option<CloudNotConfirmed> = None;
    let mut id_prefix: Option<String> = None;
    match (&record, unreadable) {
        (Some(record), _) => {
            id_prefix = Some(record.summary().credential_id_prefix);
            if record.origin != config.origin() {
                not_confirmed = Some(CloudNotConfirmed::OriginMismatch);
            } else if probe.is_local_secret(record.secret.expose_secret()) {
                not_confirmed = Some(CloudNotConfirmed::LocalKey);
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
                    Err(error) => not_confirmed = Some(CloudNotConfirmed::Cloud(error)),
                }
            }
        }
        (None, Some(error)) => not_confirmed = Some(CloudNotConfirmed::FileUnusable(error)),
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
    let reason = not_confirmed
        .as_ref()
        .map_or("cloud_not_confirmed", CloudNotConfirmed::code);
    if matches!(not_confirmed, Some(CloudNotConfirmed::LocalKey)) {
        // 这不是「云端那张凭证可能仍然有效」的情形：文件里根本不是云凭证。说清楚没发出去什么、什么没有被动过
        let _ = writeln!(
            stderr,
            "本机凭证文件已删除，但没有通知云端：这个文件里的密钥是本机的本地数据密钥，或者本机的密钥库读不出来、\
             无法确认它不是（本地密钥永远不发给云端），所以没有发给云端。本地数据密钥本身没有被动过\
             （它仍在本机的密钥库里，是否吊销由你决定：idoris key revoke <key_id>）。\n\
             这台机器如果真的配对过云账号，那张云凭证不在这个文件里、本机无从知道它的 id：请在云账号的控制面查看并撤销，\
             然后重新运行 idoris cloud pair。"
        );
    } else {
        let note = not_confirmed
            .as_ref()
            .map_or_else(|| "未知原因".to_string(), CloudNotConfirmed::text);
        let _ = writeln!(
            stderr,
            "本机凭证已删除，但云端没有确认撤销（{note}）。\n\
             云端那张凭证（{prefix}…）可能仍然有效：请在云账号的控制面撤销它\
             （POST /control/v1/credentials/<凭证 id>/status，status 设为 revoked），或让云账号的管理员撤销。"
        );
    }
    writeln!(
        stdout,
        "{}",
        json!({
            "unpaired": true,
            "cloud_revoked": false,
            "credential_id_prefix": id_prefix,
            "local_credential_deleted": true,
            "reason": reason,
        })
    )
    .map_err(|_| "无法写 stdout".to_string())?;
    Ok(2)
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
    // 本地密钥探针：只读地查本机虚拟 key 库（不创建文件、不建表）；库读不出来就按「是本地密钥」处理，不发
    let probe = super::VirtualKeyFileProbe::new(db_path.clone());
    let ctx = CliContext {
        config: &config,
        db_path: &db_path,
        http,
        now_ms,
        probe: &probe,
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
