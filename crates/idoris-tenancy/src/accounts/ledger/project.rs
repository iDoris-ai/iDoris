//! 账户可用额的**投影**：把分录和额度、授信、限额读出来，填成 S0 的 [`AncestorView`]。
//!
//! 这里没有任何「余额字段」可读：账户有多少钱、花了多少、在途多少，都是对 `ledger_entries` 求和：
//!
//! | `AncestorView` 的字段 | 怎么得到 |
//! |---|---|
//! | `balance`（自有资金） | 有充值分录才有：`Σ funds − Σ (held + spent)`（所有时间）。没有充值过 = `None` |
//! | `ceiling_remaining`（授权额度剩余） | 流入这个账户的、此刻有效的**花费类**额度（免费额度、预付、组织配额，不含授信）之和，减去这些额度**窗口内**（从最早的有效起点算起）的 `Σ (held + spent)`。曾经有过额度、现在一个都不生效（比如上个月的免费额度过期了）= `Some(0)`，**不是** `None` |
//! | `credit_limit` | 此刻有效的**授信额度**（`allocations` 里 purpose = `credit_line`、窗口内、没被撤销的）之和，没有 = 0 |
//! | `rate_windows` | 配了分钟 / 小时上限才有：`Σ (held + spent)` 里 `window_at` 落在最近 60 秒 / 3600 秒（滑动窗口，不是日历桶：任意一个窗口长度的区间里都不会超过上限）的部分 |
//! | `in_flight` | 配了在途上限才有：当前在途 = `Σ held`；过期的预留已经有 `expire` 分录冲掉 |
//! | `subsidy_pool_remaining` | 只有补贴池账户：当前账期的总上限（`ledger_pool_caps` 最新一条，没有 = 0）减去该账期池账户上的 `Σ (held + spent)` |
//!
//! **失败即关闭**：除了平台根和补贴池，一个账户既没有充值也从没有过额度，它的 `ceiling_remaining` 是 `Some(0)`
//! （不是 `None`）。S0 把 `None` 解释成「没配置、不设限」，对一个会花钱的账户这是最危险的默认值：
//! 新开的组织、新加的成员什么都没配置就能无限花平台的钱。所以默认是什么都不允许，要先发额度或充值。
//!
//! **仅授信账户的累计债务**（PR #600 第二轮评审 High）：授信是「有担保的负值」，它限制的是**累计**透支，不是单笔。
//! 有充值的账户，余额 `funds − Σ (held + spent)` 本来就带着已欠的钱；有此刻生效的花费类额度的账户，`ceiling_remaining`
//! 减去了窗口内的消耗。但**没充值、此刻也没有任何生效的花费类额度、只有一笔授信**的账户（经控制面开通授信的成员）
//! 这两个数都不记它的消耗：旧代码给它填固定的 `Some(0)`，于是不管它已经花了多少、还有多少在途，每次准入看到的都是
//! 「0 + 授信」的全额余量，授信变成了单笔上限，重复、并发的请求都能一直透支下去。现在这种账户的 `ceiling_remaining`
//! 是 `-债务`，债务 = 它自己账上从「最后一次有别的钱覆盖它」起累计的 `Σ (held + spent)`（已花 + 在途预留）：
//! 最后一次覆盖 = 它名下没被撤销的花费类额度里最晚到期的那个的到期时刻（从没有过 = 从头算起）。授信窗口、授信是不是被
//! 「设置」替换过都不影响债务：重发一遍授信不会把已经欠下的勾销（和有充值的账户一样，债务只会被还掉，不会被忘掉）。
//! 授信已经失效（关闭、过期、被撤销）的账户仍是 `Some(0)`：什么都不允许，用不着算债务。
//!
//! 所有求和都在调用方已经开着的写事务里读，所以和同一事务里随后写下的预留之间没有缝。
//! SQLite 的 `SUM` 溢出会报错（不是回绕），那个错误原样往上走：准入失败即关闭。

use idoris_compute_contracts::account::{Account, AccountKind};
use idoris_compute_contracts::admission::{
    AncestorView, InFlightView, RateWindowKind, RateWindowView,
};
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
    let present: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'allocation_revocations')",
        [],
        |row| row.get(0),
    )?;
    Ok(if present {
        " AND NOT EXISTS (SELECT 1 FROM allocation_revocations r WHERE r.allocation_id = allocations.allocation_id)"
    } else {
        ""
    })
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

/// 一个仅靠授信花钱的账户（没充值、此刻也没有生效的花费类额度）的**累计债务**：它自己账上从「最后一次有别的钱覆盖它」
/// 起的 `Σ (held + spent)`，也就是已花加在途预留。
///
/// 「最后一次覆盖」= 它名下没被撤销的花费类额度里已经到期的那些，最晚的到期时刻（`valid_until <= now`；此刻生效的不在
/// 这里：有生效额度的账户不走这条路）。从没有过额度 = 从头算起。被撤销的额度不算覆盖过任何东西（撤销之后它不再给任何人
/// 授权，和 `not_revoked_clause` 同一条规矩），所以保守：它名下花掉的钱也算债务，只会让授信余量更小，不会更大。
/// 债务与授信自己的窗口无关：授信被「设置」替换、关掉又重开，都不会把已经欠下的勾销。
fn credit_debt_micros(
    conn: &Connection,
    account: &str,
    now_ms: i64,
    not_revoked: &str,
) -> Result<i64, LedgerError> {
    let covered_until: i64 = conn.query_row(
        &format!(
            "SELECT COALESCE(MAX(valid_until_ms), 0) FROM allocations
              WHERE child_id = ?1 AND purpose IN {SPENDABLE_PURPOSES}
                AND valid_until_ms <= ?2{not_revoked}"
        ),
        rusqlite::params![account, now_ms],
        |row| row.get(0),
    )?;
    let committed: i64 = conn.query_row(
        "SELECT COALESCE(SUM(held_micros + spent_micros), 0) FROM ledger_entries
          WHERE account_id = ?1 AND window_at_ms >= ?2",
        rusqlite::params![account, covered_until],
        |row| row.get(0),
    )?;
    // 同一个预留的在途、已花分录落在同一个时刻上，净额不会是负的；万一读出负数，按 0 算（不会借此多出授信余量）
    Ok(committed.max(0))
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
        // 仅授信账户（没充值、此刻也没有生效的花费类额度）：授信是它唯一的钱，所以授信限制的是它的累计债务。
        // 它的 `ceiling_remaining` 记成 `-债务`，`available + 授信` 就是还能再欠多少；已花和在途预留都在债务里，
        // 重复请求、并发请求每一次都看得到前面欠下的（见模块文档）。授信失效的账户保持 `Some(0)`：什么都不允许。
        if spends_money && !sums.has_funds && active == 0 && credit_limit > 0 {
            let debt = credit_debt_micros(conn, id, now_ms, not_revoked)?;
            ceiling_remaining = Some(money(sub(0, debt)?));
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
