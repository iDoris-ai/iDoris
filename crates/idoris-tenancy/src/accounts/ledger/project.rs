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
//! * 它名下每一笔花费类额度是一个「桶」：只覆盖**自己窗口内、自己存在之后**的花费。窗口是额度自己声明的有效期
//!   `[valid_from, valid_until)`（左闭右开，按花费的归属时间判断）；「存在之后」看**提交顺序**：额度创建那一刻账本里最后一条
//!   分录的号记在 `ledger_allocation_marks`（迁移 0004），分录号大于它的花费才是它存在之后的。迟发的、起点倒填的额度不会回头替
//!   更早的花费还债；
//! * 一笔花费（一个预留在这个账户上的全部分录：预留、结算、释放、过期；先后看它**第一条**分录的号，迟到的结算是另一笔新的花费）
//!   先用此刻生效的桶里**先到期**的，用完才记成授信债务。所以同一个请求先用免费额度，免费额度用完才动授信；
//! * **授信债务 = 全期累计里没被额度覆盖的部分**（已花加在途预留）。它不因新额度的窗口而重置，不因额度到期（哪怕一分没花）
//!   而勾销，也不因授信被重设、关掉再开而勾销。债务只因上级充值而减少：有了充值，账户就按上面有充值的口径记账，余额带着全部已花；
//! * 被撤销的额度此刻不再给任何人授权（不计入额度剩余），但撤销**之前**它覆盖过的花费照样是它覆盖的（L1：预付额度 1000
//!   下已花 900，撤销后再开 100 授信，授信可用是 100，不是 -800）。撤销提交之后才提交的花费不再被它覆盖（撤销同样记一个分录号）。
//!
//! **先后只看提交顺序，不看毫秒**（PR #600 第四轮复审 Medium 3）。额度的创建时间、撤销时间、花费的归属时间都是调用方在**进事务
//! 之前**取的毫秒，HTTP 请求取时之后还要等工作线程和写锁，排队能让「先提交」和「时间更早」对不上，同一毫秒更分不出先后。
//! 拿毫秒比先后，后来才存在的额度会追溯偿还它存在之前的授信债务（债务凭空少一块，额度没人用、到期了授信还是空出来），
//! 已撤销的额度会回头覆盖撤销之后才提交的花费。分录号（AUTOINCREMENT）在串行的写事务里追加，大小就是提交的先后。
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
    Ok(separate_books(conn, id, now_ms)?.credit_debt > 0)
}

/// 一个没充值、有过授信的账户上，「先额度后授信」的账（口径见模块文档「授信的旧债与新额度分开记」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SeparateBooks {
    /// 此刻还能花的额度：没被撤销、此刻生效的花费类额度，扣掉它们已经覆盖的花费之后的剩余。
    allowance_left: i64,
    /// 累计授信债务：全期已花（含在途预留）里没被额度覆盖的部分。
    credit_debt: i64,
}

/// 一笔花费类额度在回放里的样子：只覆盖窗口 `[from, until)` 里归属的、它存在期间提交的花费，总共不超过 `capacity`。
#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// 额度自己声明的有效期（毫秒，左闭右开）：按花费的归属时间判断，不是哪个事务取的时间。
    from: i64,
    until: i64,
    capacity: i64,
    /// 额度创建时账本里最后一条分录的号（`ledger_allocation_marks`，迁移 0004）：分录号大于它的花费是它存在之后才提交的，
    /// 只有这些可能被它覆盖。迟发的、起点倒填的额度不会回头替它存在之前的花费还债。
    created_after: i64,
    /// 被撤销时同样的号（没被撤销 = `None`）：分录号大于它的花费是撤销提交之后才有的，不再被它覆盖；
    /// 撤销之前它覆盖过的花费照样是它覆盖的。被撤销的额度此刻不再给任何人授权（不计入额度剩余）。
    revoked_after: Option<i64>,
}

impl Bucket {
    /// 这笔花费（归属时刻 `at`，第一条分录的号是 `first_entry`）能不能用这笔额度。
    fn covers(&self, at: i64, first_entry: i64) -> bool {
        self.from <= at
            && at < self.until
            && self.created_after < first_entry
            && self.revoked_after.is_none_or(|after| first_entry <= after)
    }
}

/// 一笔花费：一个预留在这个账户上、同一个归属时刻上的全部分录（预留、结算、释放、过期）的净额。
#[derive(Debug, Clone, Copy)]
struct Spend {
    /// 归属时刻（毫秒）：预留创建的时刻；迟到的结算记在结算的时刻。
    at: i64,
    /// 这笔花费第一条分录的号：先后看它，不看毫秒。
    first_entry: i64,
    amount: i64,
}

/// 这个账户名下、`now` 之前已经开始的花费类额度（额度 0 的不要），按创建先后排好。
fn load_buckets(conn: &Connection, account: &str, now_ms: i64) -> Result<Vec<Bucket>, LedgerError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT a.allocation_id, a.valid_from_ms, a.valid_until_ms, a.ceiling_micros,
                (SELECT m.after_entry FROM ledger_allocation_marks m
                  WHERE m.allocation_id = a.allocation_id AND m.event = 'created'),
                EXISTS (SELECT 1 FROM allocation_revocations r WHERE r.allocation_id = a.allocation_id),
                (SELECT m.after_entry FROM ledger_allocation_marks m
                  WHERE m.allocation_id = a.allocation_id AND m.event = 'revoked')
           FROM allocations a
          WHERE a.child_id = ?1 AND a.purpose IN {SPENDABLE_PURPOSES} AND a.valid_from_ms <= ?2
          ORDER BY a.rowid"
    ))?;
    let rows = stmt.query_map(rusqlite::params![account, now_ms], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, bool>(5)?,
            row.get::<_, Option<i64>>(6)?,
        ))
    })?;
    let mut buckets = Vec::new();
    for row in rows {
        let (id, from, until, capacity, created_after, revoked, revoked_after) = row?;
        // 每笔额度都由触发器记了创建序号、每笔撤销都记了撤销序号（迁移 0004）。缺了就是库被改坏了：失败即关闭，
        // 不能按「没有序号」当成「很早就存在」而让它覆盖不该覆盖的花费。
        let created_after = created_after.ok_or_else(|| {
            LedgerError::Corrupt(format!(
                "allocation {id} has no creation mark in the ledger"
            ))
        })?;
        let revoked_after = match (revoked, revoked_after) {
            (false, _) => None,
            (true, Some(after)) => Some(after),
            (true, None) => {
                return Err(LedgerError::Corrupt(format!(
                    "allocation {id} is revoked but has no revocation mark in the ledger"
                )));
            }
        };
        if capacity > 0 {
            buckets.push(Bucket {
                from,
                until,
                capacity,
                created_after,
                revoked_after,
            });
        }
    }
    Ok(buckets)
}

/// 这个账户上的每一笔花费，按第一条分录的号（提交先后）排好。同一个预留在同一个归属时刻上的在途、已花合成一笔，
/// 净额为 0 的（放掉了、过期了）不要。
fn load_spends(conn: &Connection, account: &str) -> Result<Vec<Spend>, LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT window_at_ms, MIN(entry_id), SUM(held_micros + spent_micros) FROM ledger_entries
          WHERE account_id = ?1 AND hold_id IS NOT NULL
          GROUP BY hold_id, window_at_ms
         HAVING SUM(held_micros + spent_micros) <> 0
          ORDER BY MIN(entry_id)",
    )?;
    let rows = stmt.query_map([account], |row| {
        Ok(Spend {
            at: row.get(0)?,
            first_entry: row.get(1)?,
            amount: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// 回放「先额度后授信」：这个账户全期的花费按**提交先后**（第一条分录的号）一笔一笔来，每一笔先用此刻能用的额度里**先到期**的
/// （同一时刻到期的按创建先后），用不完的额度留给后面，用不到额度的部分记成授信债务。
///
/// 一笔花费能用哪些额度，只看这笔花费和额度自己的记录：额度的窗口包含它的归属时刻、额度是在它**之前**创建的、
/// 没有在它之前被撤销（口径见模块文档）。所以后来才创建的额度动不了它存在之前的任何一笔花费，往账本里追加新的额度或撤销，
/// 都不会改变之前那些花费已经得到的覆盖（只有结算、释放、过期会改一笔花费的净额）。
/// 归属时刻晚于 `now` 的花费按 `now` 算（读的是过去的某一刻时，往后的花费照样占着此刻生效的额度，只会更保守）。
/// 回放是 `O(花费笔数 × 额度个数)`：额度个数是个位数到几十（每月一笔免费额度加预付），读花费的那一次扫描和准入已有的求和同量级。
fn separate_books(
    conn: &Connection,
    account: &str,
    now_ms: i64,
) -> Result<SeparateBooks, LedgerError> {
    let buckets = load_buckets(conn, account, now_ms)?;
    let mut left: Vec<i64> = buckets.iter().map(|bucket| bucket.capacity).collect();
    // 先到期的先用。排序只看额度自己的到期时间（`sort_by_key` 是稳定的：同一时刻到期的保持创建先后），不看之后才发生的撤销，
    // 所以一笔花费得到的覆盖不会因为日后撤销了某笔额度而换一笔额度来覆盖。
    let mut order: Vec<usize> = (0..buckets.len()).collect();
    order.sort_by_key(|index| buckets[*index].until);
    let mut credit_debt = 0_i64;
    for spend in load_spends(conn, account)? {
        let at = spend.at.min(now_ms);
        // 万一读出负数（分录被改坏了），按 0 算：不会借此多出额度覆盖
        let mut rest = spend.amount.max(0);
        for index in &order {
            if rest == 0 {
                break;
            }
            if buckets[*index].covers(at, spend.first_entry) {
                let take = rest.min(left[*index]);
                left[*index] -= take;
                rest -= take;
            }
        }
        credit_debt = add(credit_debt, rest)?;
    }
    let mut allowance_left = 0_i64;
    for (index, bucket) in buckets.iter().enumerate() {
        if bucket.revoked_after.is_none() && bucket.from <= now_ms && now_ms < bucket.until {
            allowance_left = add(allowance_left, left[index])?;
        }
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
            let books = separate_books(conn, id, now_ms)?;
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
