//! 账户可用额的**投影**：把分录和额度、授信、限额读出来，填成 S0 的 [`AncestorView`]。
//!
//! 这里没有任何「余额字段」可读：账户有多少钱、花了多少、在途多少，都是对 `ledger_entries` 求和：
//!
//! | `AncestorView` 的字段 | 怎么得到 |
//! |---|---|
//! | `balance`（自有资金） | 有充值分录才有：`Σ funds − Σ (held + spent)`（所有时间）。没有充值过 = `None` |
//! | `ceiling_remaining`（授权额度剩余） | 流入这个账户的、此刻有效的**花费类**额度（免费额度、预付、组织配额，不含授信）之和，减去这些额度**窗口内**（从最早的有效起点算起）的 `Σ (held + spent)`。曾经有过额度、现在一个都不生效（比如上个月的免费额度过期了）= `Some(0)`，**不是** `None`。例外：没充值、有过授信的账户走下面「授信的旧债与新额度分开记」的口径 |
//! | `credit_limit` | 此刻有效的**授信额度**（`allocations` 里 purpose = `credit_line`、窗口内、没被撤销的）之和，没有 = 0 |
//! | `rate_windows` | 配了分钟 / 小时上限才有：`Σ (held + spent)` 里 `window_at` 落在最近 60 秒 / 3600 秒（滑动窗口，不是日历桶：任意一个窗口长度的区间里都不会超过上限）的部分 |
//! | `in_flight` | 配了在途上限才有：当前在途 = `Σ held`；过期的预留已经有 `expire` 分录冲掉 |
//! | `subsidy_pool_remaining` | 只有补贴池账户：当前账期的总上限（`ledger_pool_caps` 最新一条，没有 = 0）减去该账期池账户上的 `Σ (held + spent)` |
//!
//! **失败即关闭**：除了平台根和补贴池，一个账户既没有充值也从没有过额度，它的 `ceiling_remaining` 是 `Some(0)`
//! （不是 `None`）。S0 把 `None` 解释成「没配置、不设限」，对一个会花钱的账户这是最危险的默认值：
//! 新开的组织、新加的成员什么都没配置就能无限花平台的钱。所以默认是什么都不允许，要先发额度或充值。
//!
//! **授信的旧债与新额度分开记**（owner 2026-11-15 拍板，方案 C；PR #600 第三轮复审 M1 / L1）。授信是「有担保的负值」，
//! 它限制的是**累计**透支，不是单笔。有充值的账户，余额 `funds − Σ (held + spent)` 本来就是全期求和，带着已欠的钱。
//! **没充值、有过授信**的账户（经控制面开通授信的成员；授信此刻有效，或者已经关掉、过期、被撤销）没有这个余额，
//! 账本替它把「先额度后授信」的账记出来：
//!
//! * 它名下每一笔花费类额度是一个「桶」：只覆盖**自己窗口内、自己存在之后**的花费，覆盖区间
//!   `[max(生效起点, 创建时刻), min(到期, 撤销时刻))`（左闭右开）。迟发的、起点倒填的额度不会回头替更早的花费还债；
//! * 一笔花费（预留、结算，按预留创建的时刻归属）先用此刻生效的桶里**先到期**的，用完才记成授信债务。所以同一个请求
//!   先用免费额度，免费额度用完才动授信；
//! * **授信债务 = 全期累计里没被额度覆盖的部分**（已花加在途预留）。它不因新额度的窗口而重置，不因额度到期（哪怕一分没花）
//!   而勾销，也不因授信被重设、关掉再开而勾销。债务只因上级充值而减少：有了充值，账户就按上面有充值的口径记账，余额带着全部已花；
//! * 被撤销的额度此刻不再给任何人授权（不计入额度剩余），但撤销**之前**它覆盖过的花费照样是它覆盖的（L1：预付额度 1000
//!   下已花 900，撤销后再开 100 授信，授信可用是 100，不是 -800）。撤销那一刻起的花费不再被它覆盖。
//!
//! 视图里记成净头寸：`ceiling_remaining = 额度剩余 − min(债务, 授信额度)`，授信额度原样放在 `credit_limit`。S0 的
//! `可用额度 + 授信` 因此恰好是 `额度剩余 + 授信可用`（授信可用 = 授信额度 − 债务，不为负）：授信 200 已用光，月初发 10 的
//! 免费额度，这个月能花 10，授信可用仍是 0；10 花完或月底作废，债务仍是 200。债务超过授信额度的部分（授信被调小，或 overage
//! 之后）不再往额度里扣：它再也花不出去，新额度不该被它吃掉。授信此刻失效的账户授信额度是 0、没有授信可用，但欠下的债还在，
//! 所以同样走这条路：新额度照常可花（不被旧债吃掉），授信再开时债务照样从全期分录里算出来，不会因为授信关过一阵就忘掉；
//! 宽限那边另看一眼这笔债（[`owes_credit`]）：欠着授信的钱不给宽限。从没有过授信的账户不走这条路，仍是上面的额度窗口口径。
//!
//! 准入告警 `credit_used` 的 `amount` 是准入时的净头寸口径（和有充值的账户一样：欠额含已经欠下的）。
//! 没有授信、也没有任何额度的账户仍是 `Some(0)`：什么都不允许，用不着算债务。
//!
//! 所有求和都在调用方已经开着的写事务里读，所以和同一事务里随后写下的预留之间没有缝。
//! SQLite 的 `SUM` 溢出会报错（不是回绕），那个错误原样往上走：准入失败即关闭。

use idoris_compute_contracts::account::{Account, AccountKind};
use idoris_compute_contracts::admission::{
    AncestorView, InFlightView, RateWindowKind, RateWindowView,
};
use idoris_compute_contracts::ids::AccountId;
use idoris_compute_contracts::money::{Money, MoneyError};
use rusqlite::{Connection, OptionalExtension};

use super::error::{LedgerError, add, sub};
use super::period::{period_bounds, period_of};
use super::types::LedgerConfig;

/// 分钟窗口的长度。
pub(super) const MINUTE_MS: i64 = 60_000;
/// 小时窗口的长度。
pub(super) const HOUR_MS: i64 = 3_600_000;

/// 花费类额度的用途（授信不算：它是透支下限，不是可花的额度）。
pub(super) const SPENDABLE_PURPOSES: &str = "('free_tier', 'prepaid', 'org_quota')";

/// S2 的额度撤销（「关授信」）：被撤销的额度不再给任何人授权。撤销记在 S2 的侧表 `allocation_revocations`
/// 里（只追加，`allocations` 本身不动），所以**读 `allocations` 的 SQL 都要带上这个子句**——和 S2 的读函数是同一条规矩。
///
/// 这张表在 S2 合入之前不存在，所以按表在不在拼子句：在，就排除有撤销行的额度；不在，什么都不排除。
/// 这样 S2 合入之后撤销立刻对准入生效，不需要再回来改账本。子句只能和 `FROM allocations` 一起用。
pub(super) fn not_revoked_clause(conn: &Connection) -> Result<&'static str, LedgerError> {
    Ok(if revocations_present(conn)? {
        " AND NOT EXISTS (SELECT 1 FROM allocation_revocations r WHERE r.allocation_id = allocations.allocation_id)"
    } else {
        ""
    })
}

/// 额度被撤销的时刻（`allocation_revocations.revoked_at_ms`；没被撤销是 `NULL`）：授信的旧债要知道「撤销之前」覆盖过什么。
/// 和 [`not_revoked_clause`] 同一条规矩：表不在就没有撤销，表达式是字面量 `NULL`。只能和 `FROM allocations` 一起用。
fn revoked_at_expr(conn: &Connection) -> Result<&'static str, LedgerError> {
    Ok(if revocations_present(conn)? {
        "(SELECT r.revoked_at_ms FROM allocation_revocations r WHERE r.allocation_id = allocations.allocation_id)"
    } else {
        "NULL"
    })
}

fn revocations_present(conn: &Connection) -> Result<bool, LedgerError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'allocation_revocations')",
        [],
        |row| row.get(0),
    )?)
}

/// 读账本配置。
pub(super) fn load_config(conn: &Connection) -> Result<LedgerConfig, LedgerError> {
    let read = |key: &str| -> Result<String, LedgerError> {
        conn.query_row(
            "SELECT value FROM ledger_config WHERE key = ?1",
            [key],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| LedgerError::Corrupt(format!("ledger_config is missing {key}")))
    };
    let ttl = read("reservation_ttl_ms")?;
    let reservation_ttl_ms = ttl
        .parse::<i64>()
        .ok()
        .filter(|ms| *ms > 0)
        .ok_or_else(|| {
            LedgerError::Corrupt(format!("reservation_ttl_ms {ttl:?} is not positive"))
        })?;
    Ok(LedgerConfig {
        billing_timezone: read("billing_timezone")?,
        reservation_ttl_ms,
    })
}

/// 一个账户上分录的各种求和（一次查询）。
#[derive(Debug, Default, Clone, Copy)]
struct EntrySums {
    /// `Σ held`：当前在途。
    held: i64,
    /// `Σ (held + spent)`：所有时间。
    committed: i64,
    /// `Σ funds`：自有资金。
    funds: i64,
    has_funds: bool,
    /// 额度窗口内的 `Σ (held + spent)`。
    window: i64,
    minute: i64,
    hour: i64,
}

fn entry_sums(
    conn: &Connection,
    account: &str,
    window_from_ms: i64,
    now_ms: i64,
) -> Result<EntrySums, LedgerError> {
    let minute_from = sub(now_ms, MINUTE_MS)?;
    let hour_from = sub(now_ms, HOUR_MS)?;
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(held_micros), 0),
                COALESCE(SUM(held_micros + spent_micros), 0),
                COALESCE(SUM(funds_micros), 0),
                COALESCE(SUM(CASE WHEN kind = 'fund' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN window_at_ms >= ?2 THEN held_micros + spent_micros ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN window_at_ms > ?3 THEN held_micros + spent_micros ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN window_at_ms > ?4 THEN held_micros + spent_micros ELSE 0 END), 0)
           FROM ledger_entries WHERE account_id = ?1",
        rusqlite::params![account, window_from_ms, minute_from, hour_from],
        |row| {
            Ok(EntrySums {
                held: row.get(0)?,
                committed: row.get(1)?,
                funds: row.get(2)?,
                has_funds: row.get::<_, i64>(3)? > 0,
                window: row.get(4)?,
                minute: row.get(5)?,
                hour: row.get(6)?,
            })
        },
    )?)
}

/// 账户此刻有效的授信额度（没有 / 关闭 / 过期 / 还没生效 = 0）。
///
/// **授信的权威数据源是 `allocations`**（purpose = `credit_line`）：窗口内（`valid_from <= now < valid_until`）、
/// 没有被 S2 撤销的授信额度按子账户求和。S2 控制面开通 / 撤销授信、存储层的 `set_credit_line` / `disable_credit_line`
/// 写的都是这份记录，所以开通、有效期、撤销、准入读取始终一致（PR #600 评审 #6）。担保人就是额度的授权方（父账户）。
pub(super) fn credit_limit_micros(
    conn: &Connection,
    account: &str,
    now_ms: i64,
) -> Result<i64, LedgerError> {
    let not_revoked = not_revoked_clause(conn)?;
    Ok(conn.query_row(
        &format!(
            "SELECT COALESCE(SUM(ceiling_micros), 0) FROM allocations
              WHERE child_id = ?1 AND purpose = 'credit_line'
                AND valid_from_ms <= ?2 AND ?2 < valid_until_ms{not_revoked}"
        ),
        rusqlite::params![account, now_ms],
        |row| row.get(0),
    )?)
}

/// 这个账户有没有过授信额度（`purpose = credit_line`；被撤销的、已经到期的也算）。
fn ever_had_credit_line(conn: &Connection, account: &str) -> Result<bool, LedgerError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM allocations WHERE child_id = ?1 AND purpose = 'credit_line')",
        [account],
        |row| row.get(0),
    )?)
}

/// 这个账户此刻是不是还欠着授信的钱：没充值、有过授信，并且累计授信债务（含在途预留）大于 0。
///
/// 给宽限用：宽限是「额度用完之后的最后一单」，不是给欠着钱的账户续命。授信此刻有效时，债务已经带在可用额度里（为负），
/// S0 的 `check_grace` 自己就拒绝；授信关掉、过期之后授信额度是 0，债务不再往额度里扣（新额度不被旧债吃掉），
/// 可用额度可能恰好是 0，所以宽限要单独看一眼这笔债。有充值的、从没有过授信的账户不走这条路（它们的透支体现在余额里）。
pub(super) fn owes_credit(
    conn: &Connection,
    account: &AccountId,
    now_ms: i64,
) -> Result<bool, LedgerError> {
    let id = account.as_str();
    let funded: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM ledger_entries WHERE account_id = ?1 AND kind = 'fund')",
        [id],
        |row| row.get(0),
    )?;
    if funded || !ever_had_credit_line(conn, id)? {
        return Ok(false);
    }
    let revoked_at = revoked_at_expr(conn)?;
    Ok(separate_books(conn, id, now_ms, revoked_at)?.credit_debt > 0)
}

/// 一个没充值、有过授信的账户上，「先额度后授信」的账（口径见模块文档「授信的旧债与新额度分开记」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SeparateBooks {
    /// 此刻还能花的额度：没被撤销、此刻生效的花费类额度，扣掉它们已经覆盖的花费之后的剩余。
    allowance_left: i64,
    /// 累计授信债务：全期已花（含在途预留）里没被额度覆盖的部分。
    credit_debt: i64,
}

/// 一笔花费类额度在回放里的样子：只覆盖 `[start, end)` 里归属的花费，总共不超过 `capacity`。
#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// 覆盖的起点：生效起点和创建时刻里较晚的（迟发的额度不回头覆盖更早的花费）。创建时刻晚于 `now`（读的是过去的某一刻）
    /// 时取 `now`：和「窗口内就生效」的判断一致，不因为创建得晚而在它自己的窗口里变成无效。
    start: i64,
    /// 覆盖到的时刻（不含）：到期和撤销时刻里较早的。
    end: i64,
    capacity: i64,
    /// 被撤销的额度此刻不再给任何人授权（不计入额度剩余），但撤销之前它覆盖过的花费照样是它覆盖的。
    revoked: bool,
}

/// 回放中还活着的额度。
#[derive(Debug, Clone, Copy)]
struct Live {
    end: i64,
    left: i64,
    revoked: bool,
}

/// 这个账户名下、`now` 之前已经开始的花费类额度（空窗口、额度 0 的不要）。
fn load_buckets(
    conn: &Connection,
    account: &str,
    now_ms: i64,
    revoked_at: &str,
) -> Result<Vec<Bucket>, LedgerError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT valid_from_ms, valid_until_ms, created_at_ms, ceiling_micros, {revoked_at} FROM allocations
          WHERE child_id = ?1 AND purpose IN {SPENDABLE_PURPOSES} AND valid_from_ms <= ?2"
    ))?;
    let rows = stmt.query_map(rusqlite::params![account, now_ms], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, Option<i64>>(4)?,
        ))
    })?;
    let mut buckets = Vec::new();
    for row in rows {
        let (from, until, created, capacity, revoked_at) = row?;
        let start = from.max(created.min(now_ms));
        let end = revoked_at.map_or(until, |at| until.min(at));
        if capacity > 0 && start < end {
            buckets.push(Bucket {
                start,
                end,
                capacity,
                revoked: revoked_at.is_some(),
            });
        }
    }
    Ok(buckets)
}

/// 归属时刻落在 `[from_ms, to_ms]`（闭区间，整数毫秒）里的 `Σ (held + spent)`。
fn spent_between(
    conn: &Connection,
    account: &str,
    from_ms: i64,
    to_ms: i64,
) -> Result<i64, LedgerError> {
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(held_micros + spent_micros), 0) FROM ledger_entries
          WHERE account_id = ?1 AND window_at_ms BETWEEN ?2 AND ?3",
        rusqlite::params![account, from_ms, to_ms],
        |row| row.get(0),
    )?)
}

/// 回放「先额度后授信」：把这个账户全期的花费按额度的起点、终点切成段，每一段里此刻生效的额度集合不变，
/// 这一段的花费（`Σ (held + spent)`，同一个预留的在途、已花落在同一个归属时刻上，净额不会是负的）先用先到期的额度，
/// 用不完的额度带进下一段，到期的额度剩下的作废，用不到额度的部分记成授信债务。
///
/// 一段里先花哪一笔、哪一笔先花，都不改变这一段用掉的总量，所以按段求和和「一笔一笔回放」得到的账是同一份
/// （测试里有一个逐笔回放的独立模型对照）。段数只和这个账户名下额度的个数有关，不和分录条数有关。
/// 归属时刻晚于 `now` 的花费算在最后一段（读的是过去的某一刻时，往后的花费照样占着额度，只会更保守）。
fn separate_books(
    conn: &Connection,
    account: &str,
    now_ms: i64,
    revoked_at: &str,
) -> Result<SeparateBooks, LedgerError> {
    let buckets = load_buckets(conn, account, now_ms, revoked_at)?;
    let mut points: Vec<i64> = buckets
        .iter()
        .flat_map(|bucket| [bucket.start, bucket.end])
        .filter(|point| *point <= now_ms)
        .collect();
    points.sort_unstable();
    points.dedup();

    let mut live: Vec<Live> = Vec::new();
    let mut credit_debt = 0_i64;
    let mut from = i64::MIN;
    for point in points
        .iter()
        .copied()
        .map(Some)
        .chain(std::iter::once(None))
    {
        // 这一段：[from, point)；最后一段没有终点
        let to = point.map_or(i64::MAX, |p| p.saturating_sub(1));
        // 万一读出负数（分录被改坏了），按 0 算：不会借此多出额度覆盖
        let mut rest = spent_between(conn, account, from, to)?.max(0);
        live.sort_by_key(|bucket| bucket.end);
        for bucket in &mut live {
            let take = rest.min(bucket.left);
            bucket.left -= take;
            rest -= take;
        }
        credit_debt = add(credit_debt, rest)?;
        if let Some(point) = point {
            // 走到边界：到期（撤销）的作废，刚开始的加入
            live.retain(|bucket| bucket.end > point);
            live.extend(
                buckets
                    .iter()
                    .filter(|bucket| bucket.start == point)
                    .map(|bucket| Live {
                        end: bucket.end,
                        left: bucket.capacity,
                        revoked: bucket.revoked,
                    }),
            );
            from = point;
        }
    }
    let mut allowance_left = 0_i64;
    for bucket in live.iter().filter(|bucket| !bucket.revoked) {
        allowance_left = add(allowance_left, bucket.left)?;
    }
    Ok(SeparateBooks {
        allowance_left,
        credit_debt,
    })
}

/// 一个账户此刻生效的限额（微单位；`None` = 不设限）。
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct LimitMicros {
    pub inflight: Option<i64>,
    pub minute: Option<i64>,
    pub hour: Option<i64>,
}

/// 账户此刻有效的速率闸门与在途上限（最新一条设置）。
pub(super) fn limits_micros(conn: &Connection, account: &str) -> Result<LimitMicros, LedgerError> {
    Ok(conn
        .query_row(
            "SELECT inflight_cap_micros, minute_cap_micros, hour_cap_micros FROM ledger_account_limits
              WHERE account_id = ?1 ORDER BY version DESC LIMIT 1",
            [account],
            |row| {
                Ok(LimitMicros {
                    inflight: row.get(0)?,
                    minute: row.get(1)?,
                    hour: row.get(2)?,
                })
            },
        )
        .optional()?
        .unwrap_or_default())
}

/// 补贴池在某个账期的总上限（微单位）；没有设置 = 0（失败即关闭）。
pub(super) fn pool_cap_micros(
    conn: &Connection,
    pool: &str,
    period: &str,
) -> Result<i64, LedgerError> {
    Ok(conn
        .query_row(
            "SELECT cap_micros FROM ledger_pool_caps WHERE pool_id = ?1 AND period = ?2
              ORDER BY version DESC LIMIT 1",
            [pool, period],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// 为 `chain`（`[付款账户, 父, ..., 根]`）里每个账户读出 S0 的 [`AncestorView`]。顺序与 `chain` 一致。
pub(super) fn project_chain(
    conn: &Connection,
    chain: &[Account],
    now_ms: i64,
    config: &LedgerConfig,
) -> Result<Vec<AncestorView>, LedgerError> {
    let period = period_of(now_ms, &config.billing_timezone)?;
    let (period_start, period_end) = period_bounds(period, &config.billing_timezone)?;
    let not_revoked = not_revoked_clause(conn)?;
    let revoked_at = revoked_at_expr(conn)?;
    let mut views = Vec::with_capacity(chain.len());
    for account in chain {
        let unit = &account.unit;
        let money = |micros: i64| Money::new(unit.clone(), micros);
        let id = account.id.as_str();

        // 流入这个账户的、此刻有效的（窗口内、没被撤销的）花费类额度
        let (ceiling_sum, window_start, active): (i64, Option<i64>, i64) = conn.query_row(
            &format!(
                "SELECT COALESCE(SUM(ceiling_micros), 0), MIN(valid_from_ms), COUNT(*) FROM allocations
                  WHERE child_id = ?1 AND purpose IN {SPENDABLE_PURPOSES}
                    AND valid_from_ms <= ?2 AND ?2 < valid_until_ms{not_revoked}"
            ),
            rusqlite::params![id, now_ms],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        // 曾经有过额度（被撤销的也算）：现在一个都不生效就是 `Some(0)`，不是「没配置」的 `None`
        let ever_had_allocation: bool = conn.query_row(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM allocations WHERE child_id = ?1 AND purpose IN {SPENDABLE_PURPOSES})"
            ),
            [id],
            |row| row.get(0),
        )?;

        let sums = entry_sums(conn, id, window_start.unwrap_or(now_ms), now_ms)?;

        let balance = if sums.has_funds {
            Some(money(sub(sums.funds, sums.committed)?))
        } else {
            None
        };
        let mut ceiling_remaining = if ever_had_allocation {
            Some(money(if active == 0 {
                0
            } else {
                sub(ceiling_sum, sums.window)?
            }))
        } else {
            None
        };
        let spends_money = !matches!(
            account.kind,
            AccountKind::Platform | AccountKind::SubsidyPool
        );
        if balance.is_none() && ceiling_remaining.is_none() && spends_money {
            ceiling_remaining = Some(money(0));
        }
        // 此刻有效的授信额度。
        let credit_limit = credit_limit_micros(conn, id, now_ms)?;
        // 没充值、有过授信的账户：授信限制的是它的累计债务，旧债和新额度分开记（见模块文档）。
        // `ceiling_remaining` 记成净头寸 `额度剩余 - min(债务, 授信额度)`，S0 的 `可用额度 + 授信额度` 就是
        // `额度剩余 + 授信可用`：同一笔花费先用额度，额度用完才动授信；新额度照常可花，但不抵旧债；债务超过授信额度的部分
        // 不再往额度里扣。已花和在途预留都在债务里，重复请求、并发请求每一次都看得到前面欠下的。
        // 授信此刻失效（关掉、过期、被撤销）的账户授信额度是 0，没有授信可用，但欠下的债还在，所以也走这条路：
        // 新额度照常可花，不被旧债吃掉（回退到上面的额度窗口，窗口里更早的债会被算成这笔额度的消耗）。
        if spends_money && !sums.has_funds && (credit_limit > 0 || ever_had_credit_line(conn, id)?)
        {
            let books = separate_books(conn, id, now_ms, revoked_at)?;
            let debt_on_credit = books.credit_debt.min(credit_limit);
            ceiling_remaining = Some(money(sub(books.allowance_left, debt_on_credit)?));
        }

        let LimitMicros {
            inflight: inflight_cap,
            minute: minute_cap,
            hour: hour_cap,
        } = limits_micros(conn, id)?;
        let mut rate_windows = Vec::new();
        if let Some(cap) = minute_cap {
            rate_windows.push(RateWindowView {
                window: RateWindowKind::Minute,
                spent: money(sums.minute),
                cap: money(cap),
            });
        }
        if let Some(cap) = hour_cap {
            rate_windows.push(RateWindowView {
                window: RateWindowKind::Hour,
                spent: money(sums.hour),
                cap: money(cap),
            });
        }

        let subsidy_pool_remaining = if account.kind == AccountKind::SubsidyPool {
            let cap = pool_cap_micros(conn, id, &period.to_string())?;
            let consumed: i64 = conn.query_row(
                "SELECT COALESCE(SUM(held_micros + spent_micros), 0) FROM ledger_entries
                  WHERE account_id = ?1 AND window_at_ms >= ?2 AND window_at_ms < ?3",
                rusqlite::params![id, period_start, period_end],
                |row| row.get(0),
            )?;
            Some(money(sub(cap, consumed)?))
        } else {
            None
        };

        views.push(AncestorView {
            account: account.id.clone(),
            balance,
            ceiling_remaining,
            credit_limit: money(credit_limit),
            rate_windows,
            in_flight: inflight_cap.map(|cap| InFlightView {
                current: money(sums.held),
                cap: money(cap),
            }),
            subsidy_pool_remaining,
        });
    }
    Ok(views)
}

/// 一个账户自己的可用额度：自有余额与授权额度剩余里**较小**的（两者都有就两者都要满足）。
/// 都没配置（平台根、补贴池）返回 `None`。
pub(super) fn available_of(view: &AncestorView) -> Result<Option<Money>, MoneyError> {
    let mut available: Option<Money> = None;
    for money in [&view.balance, &view.ceiling_remaining]
        .into_iter()
        .flatten()
    {
        available = Some(match available.take() {
            None => money.clone(),
            Some(current) => current.try_min(money)?,
        });
    }
    Ok(available)
}

/// 整条链在「钱」这个维度上还能再吸收多少：每个有可用额度的账户的 `可用额度 + 授信` 取最小。
/// 链上没有任何账户有可用额度的限制（比如付款账户就是平台根）返回 `None`（不设限）。
///
/// 速率、在途、补贴池上限是平台的节流，不是授信，所以**不**算进来：overage 是已经发生的事实，
/// 不能因为这一分钟请求多就把它算成平台损失、冻结孩子的凭证。
pub(super) fn credit_headroom(views: &[AncestorView]) -> Result<Option<i64>, LedgerError> {
    let mut tightest: Option<i64> = None;
    for view in views {
        let Some(available) = available_of(view)? else {
            continue;
        };
        let room = add(available.micros, view.credit_limit.micros)?;
        tightest = Some(tightest.map_or(room, |current| current.min(room)));
    }
    Ok(tightest)
}
