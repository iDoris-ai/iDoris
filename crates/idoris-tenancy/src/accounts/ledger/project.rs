//! 账户可用额的**投影**：把分录和额度、授信、限额读出来，填成 S0 的 [`AncestorView`]。
//!
//! 这里没有任何「余额字段」可读：账户有多少钱、花了多少、在途多少，都是对 `ledger_entries` 求和：
//!
//! | `AncestorView` 的字段 | 怎么得到 |
//! |---|---|
//! | `balance`（自有资金） | 有充值分录才有：`Σ funds − Σ (held + spent)`（所有时间）。没有充值过 = `None`。例外：有过授信、又有过花费类额度的充值账户走下面「授信的旧债与新额度分开记」的口径，自有资金并进 `ceiling_remaining` 的净头寸，这里是 `None` |
//! | `ceiling_remaining`（授权额度剩余） | 流入这个账户的、此刻有效的**花费类**额度（免费额度、预付、组织配额，不含授信）之和，减去这些额度**窗口内**（从最早的有效起点算起）的 `Σ (held + spent)`。曾经有过额度、现在一个都不生效（比如上个月的免费额度过期了）= `Some(0)`，**不是** `None`。例外：有过授信的账户（没充值的，或者充了值又有过花费类额度的）走下面「授信的旧债与新额度分开记」的口径 |
//! | `credit_limit` | 此刻有效的**授信额度**（`allocations` 里 purpose = `credit_line`、窗口内、没被撤销的）之和，没有 = 0 |
//! | `rate_windows` | 配了分钟 / 小时上限才有：`Σ (held + spent)` 里 `window_at` 落在最近 60 秒 / 3600 秒（滑动窗口，不是日历桶：任意一个窗口长度的区间里都不会超过上限）的部分 |
//! | `in_flight` | 配了在途上限才有：当前在途 = `Σ held`；过期的预留已经有 `expire` 分录冲掉 |
//! | `subsidy_pool_remaining` | 只有补贴池账户：当前账期的总上限（`ledger_pool_caps` 最新一条，没有 = 0）减去该账期池账户上的 `Σ (held + spent)` |
//!
//! **失败即关闭**：除了平台根和补贴池，一个账户既没有充值也从没有过额度，它的 `ceiling_remaining` 是 `Some(0)`
//! （不是 `None`）。S0 把 `None` 解释成「没配置、不设限」，对一个会花钱的账户这是最危险的默认值：
//! 新开的组织、新加的成员什么都没配置就能无限花平台的钱。所以默认是什么都不允许，要先发额度或充值。
//!
//! **授信的旧债与新额度分开记**（owner 2026-11-15 拍板，方案 C；PR #600 第三轮复审 M1 / L1；issue #601 把它延伸到充过值的账户）。
//! 授信是「有担保的负值」，它限制的是**累计**透支，不是单笔。**有过授信额度**的账户（授信此刻有效，或者已经关掉、过期、被撤销）
//! 走这套账：账本把额度、自有资金、授信债务三本分开记出来。
//!
//! * 没充值的账户：一直走这套账（没有余额可读，账只能记出来）。
//! * 充过值的账户：**有过花费类额度**才走。没有任何额度的充值账户，没有额度能覆盖花费，能花的钱就是 S0 的 `余额 + 授信`
//!   （`余额 = 充值 − 全期已花`；债务超过授信额度时 S0 的余额更负，这套账按授信额度算，两边能花的都是 0），视图仍是 `balance`。
//! * 从没有过授信额度的账户（充了值的也一样）不走：没有授信就没有「授信旧债」，额度窗口口径（没充值）和 S0 的
//!   `min(余额, 额度剩余)`（充了值，额度是上限）都不变。
//!
//! 三本账的记法：
//!
//! * 它名下每一笔花费类额度是一个「桶」：只覆盖**自己窗口内、自己存在之后**的花费。窗口是额度自己声明的有效期
//!   `[valid_from, valid_until)`（左闭右开，按花费的归属时间判断）；「存在之后」看**提交顺序**：额度创建那一刻账本里最后一条
//!   分录的号记在 `ledger_allocation_marks`（迁移 0004），分录号大于它的花费才是它存在之后的。迟发的、起点倒填的额度不会回头替
//!   更早的花费还债；
//! * 一笔花费先用**自己归属时刻**生效的桶里**先到期**的。花费有两种：预留本身（这个预留在这个账户上的预留、结算、释放、过期分录，
//!   归属时刻是预留创建的时刻，先后看预留**第一条**分录的号）；和结算那一刻才发生的花费（迟到的结算整笔，没迟到的结算只有超出预留的
//!   部分 overage，分录种类 `late_settle`，归属时刻是结算的时刻，先后看这次结算自己的号）。超出预留的部分不是预留的一部分：
//!   结算时算「担保人能吸收多少」读的是结算那一刻的额度，这部分就记在结算那一刻、用结算自己的提交序号，吸收过的额度不会再被花一次
//!   （Codex 复审 2026-11-22 High 1）。额度之外的部分 `U` 先用自有资金（全部充值：充值不过期，所以不分先后，只看总数），
//!   用完才记成授信债务。所以同一个请求先用免费额度，再用自有资金，最后才动授信；
//! * **授信债务 = `max(全期 U 之和 − 全部充值, 0)`**（已花加在途预留）。它不因新额度的窗口而重置，不因额度到期（哪怕一分没花）
//!   而勾销，也不因授信被重设、关掉再开而勾销。债务只因上级充值而减少：充值先还债，还清之后多出来的才是自有资金剩余
//!   `max(全部充值 − 全期 U 之和, 0)`。额度覆盖的花费不扣自有资金，所以补钱不会让能花的变少（补 X，授信至多恢复 X，额度一分不动）；
//! * 被撤销的额度此刻不再给任何人授权（不计入额度剩余），但撤销**之前**它覆盖过的花费照样是它覆盖的（L1：预付额度 1000
//!   下已花 900，撤销后再开 100 授信，授信可用是 100，不是 -800）。撤销提交之后才提交的花费不再被它覆盖（撤销同样记一个分录号）。
//!
//! **先后只看提交顺序，不看毫秒**（PR #600 第四轮复审 Medium 3）。额度的创建时间、撤销时间、花费的归属时间都是调用方在**进事务
//! 之前**取的毫秒，HTTP 请求取时之后还要等工作线程和写锁，排队能让「先提交」和「时间更早」对不上，同一毫秒更分不出先后。
//! 拿毫秒比先后，后来才存在的额度会追溯偿还它存在之前的授信债务（债务凭空少一块，额度没人用、到期了授信还是空出来），
//! 已撤销的额度会回头覆盖撤销之后才提交的花费。分录号（AUTOINCREMENT）在串行的写事务里追加，大小就是提交的先后。
//!
//! **每一笔花费按它自己记下的归属时刻判断，不看读的是哪一刻**（Codex 复审 2026-11-22 High 2）。读钟同样是调用方进事务之前取的：
//! 排队等写锁的请求读到的钟可以比前面已经提交的花费更早。读钟只决定回放**之后**哪些额度此刻还能花（窗口包含读钟、没被撤销、
//! 剩余的量）；它不能把已经提交的花费压到更早的时刻去重新找额度——压过去的花费会落进更早的、甚至已经撤销的额度的窗口里，被算成
//! 它覆盖的，自有资金或授信就凭空回来了。同理回放要看得见读钟**之后**才开始的额度（归属时刻晚于读钟的花费要用它们），不能只加载
//! 读钟之前已经开始的。
//!
//! 视图里记成净头寸：`ceiling_remaining = 额度剩余 + 自有资金剩余 − min(债务, 授信额度)`（`balance` 是 `None`，自有资金已经
//! 并在里面；没充值的账户自有资金剩余是 0），授信额度原样放在 `credit_limit`。S0 的 `可用额度 + 授信` 因此恰好是
//! `额度剩余 + 自有资金剩余 + 授信可用`（授信可用 = 授信额度 − 债务，不为负）：授信 200 已用光，月初发 10 的免费额度，这个月能花 10，
//! 授信可用仍是 0；10 花完或月底作废，债务仍是 200。债务超过授信额度的部分（授信被调小，或 overage 之后）不再往额度里扣：
//! 它再也花不出去，新额度不该被它吃掉。授信此刻失效的账户授信额度是 0、没有授信可用，但欠下的债还在，
//! 所以同样走这条路：新额度照常可花（不被旧债吃掉），授信再开时债务照样从全期分录里算出来，不会因为授信关过一阵就忘掉；
//! 宽限那边另看一眼这笔债（[`owes_credit`]）：欠着授信的钱不给宽限（净头寸可能不是负的：新额度比欠的债大）。
//!
//! 准入告警 `credit_used` 要通知担保人「有人动了授信」：S0 只看净头寸，旧债未清时哪怕这一笔完全落在新额度里，净头寸也是负的，
//! `CreditFloor` 照样是 `PassUsingCredit`。所以走这套账的账户，这一笔没超出 [`Books::spendable_without_credit`]（额度剩余加自有资金
//! 剩余）就不记告警（`admit.rs` 的 `credit_users`）；动了授信的，`amount` 仍是准入时的净头寸口径（准入之后授信已占用的总额，
//! 含已经欠下的），和没有额度的充值账户一样。
//! 既没充值、也没有授信、没有任何额度的账户仍是 `Some(0)`：什么都不允许，用不着算债务。
//!
//! 所有求和都在调用方已经开着的写事务里读，所以和同一事务里随后写下的预留之间没有缝。
//! SQLite 的 `SUM` 溢出会报错（不是回绕），那个错误原样往上走：准入失败即关闭。

use idoris_compute_contracts::account::{Account, AccountKind};
use idoris_compute_contracts::admission::{
    AncestorView, InFlightView, RateWindowKind, RateWindowView,
};
use idoris_compute_contracts::money::{Money, MoneyError};
use rusqlite::{Connection, OptionalExtension};
use std::collections::{BTreeSet, HashMap};

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

/// 这个账户此刻是不是还欠着授信的钱：走「分开记」的账户（`books` 是 `Some`），并且累计授信债务（含在途预留）大于 0。
///
/// 给宽限用：宽限是「额度用完之后的最后一单」，不是给欠着钱的账户续命。授信此刻有效、又没有新额度时，债务已经带在可用额度里
/// （为负），S0 的 `check_grace` 自己就拒绝；但新额度比欠的债大时净头寸不是负的，授信关掉、过期之后授信额度是 0，债务不再往
/// 额度里扣（新额度不被旧债吃掉），可用额度也可能恰好是 0，所以宽限要单独看一眼这笔债。不走「分开记」的账户（从没有过授信，
/// 或者充了值却没有过额度）没有这条路：它们的透支体现在余额里，S0 自己会拒绝。
pub(super) fn owes_credit(books: Option<&Books>) -> bool {
    books.is_some_and(|books| books.credit_debt > 0)
}

/// 一个走「分开记」的账户上，额度、自有资金、授信债务三本账（口径见模块文档「授信的旧债与新额度分开记」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Books {
    /// 此刻还能花的额度：没被撤销、此刻生效的花费类额度，扣掉它们已经覆盖的花费之后的剩余。
    allowance_left: i64,
    /// 自有资金剩余：`max(全部充值 − 没被额度覆盖的花费, 0)`（没充值的账户是 0）。
    funds_left: i64,
    /// 累计授信债务：`max(没被额度覆盖的花费 − 全部充值, 0)`，花费含在途预留。
    credit_debt: i64,
}

impl Books {
    /// 不动授信还能花的钱：额度剩余加自有资金剩余。
    pub(super) fn spendable_without_credit(&self) -> Result<i64, LedgerError> {
        add(self.allowance_left, self.funds_left)
    }

    /// 视图里的净头寸：`额度剩余 + 自有资金剩余 − min(债务, 授信额度)`。债务超过授信额度的部分不再往里扣：
    /// 它再也花不出去，新额度和自有资金不该被它吃掉。
    fn net_position(&self, credit_limit: i64) -> Result<i64, LedgerError> {
        sub(
            self.spendable_without_credit()?,
            self.credit_debt.min(credit_limit),
        )
    }
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

/// 按归属时刻找候选额度的索引：额度的窗口 `[from, until)` 把时间轴切成若干基本区间，每个基本区间里「窗口包含它」的额度是固定的一小撮
/// （通常是月度免费额度加几笔预付，个位数），按回放要的次序（先到期的、同时到期的先创建的）排好。一笔花费二分找到它的基本区间，只看这一撮，
/// 不用把全部（历史上的几十到上百笔）额度从头扫一遍。
///
/// 建索引 `O(B log B + Σ k)`（`B` 是额度个数，`k` 是同一时刻同时生效的额度个数），一次投影建一次。`k` 异常大（一个账户同时有成千上万笔
/// 重叠生效的额度）时快照总量会爆炸，超过 [`WindowIndex::MAX_ENTRIES`] 就不建，回放退回原来的线性扫描（结果一样，只是慢）。
struct WindowIndex {
    /// 基本区间的起点（升序、去重）：第 `k` 个基本区间是 `[starts[k], starts[k + 1])`，最后一个到无穷。
    starts: Vec<i64>,
    /// 第 `k` 个基本区间里窗口包含它的额度（`buckets` 的下标），按（到期时间，创建先后）排好。
    candidates: Vec<Vec<usize>>,
}

impl WindowIndex {
    /// 快照总量上限（下标个数）：每个下标 8 字节，100 万个是 8 MB。
    const MAX_ENTRIES: usize = 1_000_000;

    /// 给 `buckets` 建索引；快照总量超过 `limit` 返回 `None`（调用方退回线性扫描）。
    fn build(buckets: &[Bucket], limit: usize) -> Option<Self> {
        let n = buckets.len();
        let mut boundaries: Vec<i64> = buckets
            .iter()
            .flat_map(|bucket| [bucket.from, bucket.until])
            .collect();
        boundaries.sort_unstable();
        boundaries.dedup();
        let mut by_from: Vec<usize> = (0..n).collect();
        by_from.sort_by_key(|index| buckets[*index].from);
        let mut by_until: Vec<usize> = (0..n).collect();
        by_until.sort_by_key(|index| buckets[*index].until);
        // 此刻窗口包含当前边界的额度，按（到期时间，创建先后）排好：和「先到期的先用，同时到期的先创建的先用」同一个次序
        let mut active: BTreeSet<(i64, usize)> = BTreeSet::new();
        let (mut next_start, mut next_end) = (0_usize, 0_usize);
        let mut total = 0_usize;
        let mut candidates = Vec::with_capacity(boundaries.len());
        for boundary in &boundaries {
            while next_start < n && buckets[by_from[next_start]].from <= *boundary {
                let index = by_from[next_start];
                next_start += 1;
                // 左闭右开：`until <= boundary` 的窗口在这个边界上已经结束，不放进来
                if buckets[index].until > *boundary {
                    active.insert((buckets[index].until, index));
                }
            }
            while next_end < n && buckets[by_until[next_end]].until <= *boundary {
                let index = by_until[next_end];
                next_end += 1;
                active.remove(&(buckets[index].until, index));
            }
            total += active.len();
            if total > limit {
                return None;
            }
            candidates.push(active.iter().map(|(_, index)| *index).collect());
        }
        Some(Self {
            starts: boundaries,
            candidates,
        })
    }

    /// 归属时刻 `at` 上窗口包含它的额度（`buckets` 的下标），按回放要的次序排好。
    fn at(&self, at: i64) -> &[usize] {
        match self.starts.partition_point(|start| *start <= at) {
            0 => &[],
            position => &self.candidates[position - 1],
        }
    }
}

/// 一笔花费：一个预留在这个账户上、同一个归属时刻上、同一类的全部分录的净额。
///
/// 一个预留最多有两笔花费，**分得开，哪怕归属时刻是同一毫秒**：
///
/// * 预留本身（`reserve` / `settle` / `release` / `expire`，归属时刻是预留创建的时刻）：预留额，结算之后是预留之内的部分
///   （放掉、过期了就是 0）；先后看预留自己的提交序号；
/// * 结算那一刻才发生的花费（`late_settle`，归属时刻是结算的那一刻）：迟到的结算整笔，没迟到的结算只有超出预留的部分
///   （overage）；先后看这次结算自己的提交序号。
///
/// 超出预留的部分是结算时才发生的新花费，不是预留的一部分：它要用的额度是**结算那一刻**生效、结算提交之前已经存在的额度，
/// 和结算时算「担保人能吸收多少」读的是同一份额度（读多少、吸收多少、记多少用同一个时刻和同一个提交序号），
/// 否则同一块额度会被吸收一次、之后又被花一次。
#[derive(Debug, Clone, Copy)]
struct Spend {
    /// 归属时刻（毫秒）：预留创建的时刻，或者结算的时刻（见上）。
    at: i64,
    /// 这笔花费第一条分录的号：先后看它，不看毫秒。
    first_entry: i64,
    amount: i64,
}

/// 这个账户名下**所有**花费类额度（额度 0 的不要），按创建先后排好，不管起点在读的时刻之前还是之后。
///
/// 回放的是全期的花费，每一笔按它**自己的**归属时刻去找覆盖它的额度：一笔归属时刻晚于读的那一刻的花费（读钟取得早、排队等写锁的
/// 请求之后才提交的，或者调用方给了较早的时间）要找到读钟之后才开始的额度，找不到就会被算成自有资金或授信债务。
/// 哪些额度**此刻**还能花，是回放之后另算的（[`separate_books`] 末尾：窗口包含 `now`、没被撤销）。
fn load_buckets(conn: &Connection, account: &str) -> Result<Vec<Bucket>, LedgerError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT a.allocation_id, a.valid_from_ms, a.valid_until_ms, a.ceiling_micros,
                (SELECT m.after_entry FROM ledger_allocation_marks m
                  WHERE m.allocation_id = a.allocation_id AND m.event = 'created'),
                EXISTS (SELECT 1 FROM allocation_revocations r WHERE r.allocation_id = a.allocation_id),
                (SELECT m.after_entry FROM ledger_allocation_marks m
                  WHERE m.allocation_id = a.allocation_id AND m.event = 'revoked')
           FROM allocations a
          WHERE a.child_id = ?1 AND a.purpose IN {SPENDABLE_PURPOSES}
          ORDER BY a.rowid"
    ))?;
    let rows = stmt.query_map([account], |row| {
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

/// 一个预留在这个账户上的分录归成的几笔花费，加上识别旧格式要的线索（见 [`load_spends`]）。
#[derive(Debug, Default)]
struct HoldRows {
    /// 预留本身（`reserve` / `settle` / `release` / `expire`）：第一条分录的号、归属时刻、净额。
    first: i64,
    at: i64,
    amount: i64,
    seen: bool,
    /// 结算那一刻才发生的花费（`late_settle`）。
    late_first: i64,
    late_at: i64,
    late_amount: i64,
    late_seen: bool,
    /// 没迟到的结算那一条 `settle`：它的分录号、`at_ms`（结算时刻），和预留自己的 `charged - reserved`。
    settle: Option<(i64, i64, i64)>,
}

/// 这个账户上的每一笔花费，按第一条分录的号（提交先后）排好，净额为 0 的（放掉了、过期了）不要。
///
/// 一个预留最多有两笔分得开的花费（[`Spend`]）：预留本身，和结算那一刻才发生的花费（`late_settle`）。分类看分录的种类，不看归属时刻
/// 是不是同一毫秒：预留和它的 overage 可以在同一毫秒（结算和预留取到了同一个毫秒），合成一笔就会让 overage 用上预留创建时的提交
/// 序号，够不着结算之前才发的额度。
///
/// **旧格式的分录要按同一个口径解释，但一行都不改**（Codex 复审 7c5a03fe High 1）。v0.5.0（和这个口径之前的 main）把没迟到的结算里
/// 超出预留的部分和预留之内的部分一起记在预留创建时刻的一条 `settle` 里，没有 `late_settle`；现在的结算把超出的部分另记一条
/// `late_settle`。所以一个没迟到、已结算的预留，`charged - reserved` 的正差（超出预留的部分）里**还留在 `settle` 里**的那一块
/// （正差减去已经另记在 `late_settle` 里的）要从预留本身那笔花费里分出来，成为单独的一笔：归属时刻是这条 `settle` 自己的 `at_ms`
/// （结算时刻），提交序号是这条 `settle` 自己的分录号，和新格式的 `late_settle` 一回事（同一个事务里，序号和别的花费、额度的创建 /
/// 撤销序号比出来的先后一样）。没有这一步，旧库里超出预留的部分会一直被当成预留之内的、记在预留创建的时刻，够不着结算时才生效的
/// 额度：担保人吸收过一次的额度又被下一个请求花一次。不迁移，不改写历史：只是读的时候多看一眼预留表里的 `charged - reserved`
/// （只有 `settle` 那一行才查）。
///
/// 一遍顺序扫描（走 `(account_id, window_at_ms)` 索引，不排序），在内存里按预留聚合：比让 SQLite 先按 (预留, 归属时刻, 种类)
/// 分组、再按第一条分录的号排序（两次排序）快。
fn load_spends(conn: &Connection, account: &str) -> Result<Vec<Spend>, LedgerError> {
    let mut stmt = conn.prepare(
        "SELECT e.entry_id, e.hold_id, e.kind, e.window_at_ms, e.at_ms, e.held_micros + e.spent_micros,
                CASE WHEN e.kind = 'settle' THEN
                     (SELECT h.charged_micros - h.reserved_micros FROM ledger_holds h
                       WHERE h.hold_id = e.hold_id AND h.state = 'settled' AND h.late = 0)
                END
           FROM ledger_entries e
          WHERE e.account_id = ?1 AND e.hold_id IS NOT NULL
            AND e.kind IN ('reserve', 'settle', 'release', 'expire', 'late_settle')",
    )?;
    let mut rows = stmt.query([account])?;
    let mut slots: HashMap<String, usize> = HashMap::new();
    let mut holds: Vec<HoldRows> = Vec::new();
    let corrupt = |what: &str, hold: &str| {
        LedgerError::Corrupt(format!(
            "the entries of hold {hold} on account {account} {what}"
        ))
    };
    while let Some(row) = rows.next()? {
        let entry: i64 = row.get(0)?;
        let hold = row
            .get_ref(1)?
            .as_str()
            .map_err(|e| LedgerError::Corrupt(format!("ledger entry {entry} hold id: {e}")))?;
        let kind = row
            .get_ref(2)?
            .as_str()
            .map_err(|e| LedgerError::Corrupt(format!("ledger entry {entry} kind: {e}")))?;
        let window_at: i64 = row.get(3)?;
        let at_ms: i64 = row.get(4)?;
        let amount: i64 = row.get(5)?;
        let slot = match slots.get(hold) {
            Some(slot) => *slot,
            None => {
                slots.insert(hold.to_owned(), holds.len());
                holds.push(HoldRows::default());
                holds.len() - 1
            }
        };
        let rows_of_hold = &mut holds[slot];
        if kind == "late_settle" {
            if rows_of_hold.late_seen && rows_of_hold.late_at != window_at {
                return Err(corrupt(
                    "carry two attribution moments for the settlement-time spend",
                    hold,
                ));
            }
            if !rows_of_hold.late_seen || entry < rows_of_hold.late_first {
                rows_of_hold.late_first = entry;
            }
            rows_of_hold.late_at = window_at;
            rows_of_hold.late_seen = true;
            rows_of_hold.late_amount = add(rows_of_hold.late_amount, amount)?;
        } else {
            if rows_of_hold.seen && rows_of_hold.at != window_at {
                return Err(corrupt(
                    "carry two attribution moments for the reservation",
                    hold,
                ));
            }
            if !rows_of_hold.seen || entry < rows_of_hold.first {
                rows_of_hold.first = entry;
            }
            rows_of_hold.at = window_at;
            rows_of_hold.seen = true;
            rows_of_hold.amount = add(rows_of_hold.amount, amount)?;
            if kind == "settle" {
                let over: Option<i64> = row.get(6)?;
                rows_of_hold.settle = Some((entry, at_ms, over.unwrap_or(0)));
            }
        }
    }
    let mut spends = Vec::with_capacity(holds.len() + holds.len() / 8);
    for hold in holds {
        let mut reservation = hold.amount;
        if let Some((settle_entry, settle_at, over)) = hold.settle
            && over > 0
        {
            // 还留在 `settle` 里的超出部分：`charged - reserved` 减去已经另记在 `late_settle` 里的；不超过这条 `settle` 之后
            // 预留本身剩下的净额（万一分录被改坏了，不会借此凭空多出一笔花费）
            let inside = sub(over, hold.late_amount)?.clamp(0, reservation.max(0));
            if inside > 0 {
                reservation = sub(reservation, inside)?;
                spends.push(Spend {
                    at: settle_at,
                    first_entry: settle_entry,
                    amount: inside,
                });
            }
        }
        if reservation != 0 {
            spends.push(Spend {
                at: hold.at,
                first_entry: hold.first,
                amount: reservation,
            });
        }
        if hold.late_amount != 0 {
            spends.push(Spend {
                at: hold.late_at,
                first_entry: hold.late_first,
                amount: hold.late_amount,
            });
        }
    }
    spends.sort_unstable_by_key(|spend| spend.first_entry);
    Ok(spends)
}

/// 回放三本账：这个账户全期的花费按**提交先后**（第一条分录的号）一笔一笔来，每一笔先用此刻能用的额度里**先到期**的
/// （同一时刻到期的按创建先后），用不完的额度留给后面；额度之外的部分 `U` 累加起来，先用自有资金 `funds`（全部充值），
/// 用完才是授信债务。
///
/// 一笔花费能用哪些额度，只看这笔花费和额度自己的记录：额度的窗口包含它的归属时刻、额度是在它**之前**创建的、
/// 没有在它之前被撤销（口径见模块文档）。所以后来才创建的额度动不了它存在之前的任何一笔花费，往账本里追加新的额度或撤销，
/// 都不会改变之前那些花费已经得到的覆盖（只有结算、释放、过期会改一笔花费的净额）。
/// 自有资金不参与哪一笔花费用哪一笔额度：充值不过期、不分先后，只有总数 `funds`，所以「额度之外的部分」累加之后一次扣掉。
///
/// **每一笔花费都按它自己记下的归属时刻判断，不看读的是哪一刻**（`now_ms` 只决定回放之后哪些额度此刻还能花）：读钟是调用方在进事务之前
/// 取的，排队等写锁的请求读到的钟可以比前面已经提交的花费更早。如果把一笔已经提交的花费按读钟「压」到更早的时刻，它就可能落进
/// 一笔更早的、甚至已经撤销的额度的窗口里，被算成这笔额度覆盖的，自有资金或授信就凭空回来了（造钱）。
///
/// **代价**（Codex 复审 7c5a03fe，Medium；设计稿 S3-24）：回放要读这个账户的**全部**历史分录，在调用方的写事务里，所以单次投影的耗时随账户
/// 历史**线性**增长，没有压缩。找额度这一步用 [`WindowIndex`]（每笔花费只看窗口包含它的那一小撮额度），已经不是大头：10 万笔花费 × 120 笔
/// 额度实测，找额度约 3%，其余是读分录本身（`ledger_projection_perf.rs`）。账户没有任何花费类额度时不回放，已花（含在途）就是额度之外的部分。
fn separate_books(
    conn: &Connection,
    account: &str,
    now_ms: i64,
    funds: i64,
    committed: i64,
) -> Result<Books, LedgerError> {
    let buckets = load_buckets(conn, account)?;
    let (uncovered, left) = if buckets.is_empty() {
        // 没有任何额度能覆盖花费：已花（含在途）全部是额度之外的部分，不用读花费。每个预留的净额都不为负，所以和逐笔累加一样
        (committed.max(0), Vec::new())
    } else {
        replay_spends(
            &buckets,
            &load_spends(conn, account)?,
            WindowIndex::MAX_ENTRIES,
        )?
    };
    let mut allowance_left = 0_i64;
    for (index, bucket) in buckets.iter().enumerate() {
        if bucket.revoked_after.is_none() && bucket.from <= now_ms && now_ms < bucket.until {
            allowance_left = add(allowance_left, left[index])?;
        }
    }
    Ok(Books {
        allowance_left,
        funds_left: sub(funds, uncovered)?.max(0),
        credit_debt: sub(uncovered, funds)?.max(0),
    })
}

/// 回放的核心（不碰数据库）：`spends`（已经按提交先后排好）一笔一笔找额度。返回（没被额度覆盖的部分之和，每笔额度回放之后的剩余）。
/// `index_limit` 是 [`WindowIndex`] 的快照总量上限：超过就退回线性扫描全部额度（结果一样，单测对过）。
fn replay_spends(
    buckets: &[Bucket],
    spends: &[Spend],
    index_limit: usize,
) -> Result<(i64, Vec<i64>), LedgerError> {
    let mut left: Vec<i64> = buckets.iter().map(|bucket| bucket.capacity).collect();
    // 先到期的先用。排序只看额度自己的到期时间（稳定排序：同一时刻到期的保持创建先后），不看之后才发生的撤销，
    // 所以一笔花费得到的覆盖不会因为日后撤销了某笔额度而换一笔额度来覆盖。
    let mut order: Vec<usize> = (0..buckets.len()).collect();
    order.sort_by_key(|index| buckets[*index].until);
    let index = WindowIndex::build(buckets, index_limit);
    let mut uncovered = 0_i64;
    for spend in spends {
        // 万一读出负数（分录被改坏了），按 0 算：不会借此多出额度覆盖
        let mut rest = spend.amount.max(0);
        let candidates: &[usize] = match &index {
            Some(index) => index.at(spend.at),
            None => &order,
        };
        for bucket in candidates {
            if rest == 0 {
                break;
            }
            if buckets[*bucket].covers(spend.at, spend.first_entry) {
                let take = rest.min(left[*bucket]);
                left[*bucket] -= take;
                rest -= take;
            }
        }
        uncovered = add(uncovered, rest)?;
    }
    Ok((uncovered, left))
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
    Ok(project_chain_with_books(conn, chain, now_ms, config)?.0)
}

/// 同 [`project_chain`]，另外把走「分开记」的账户的三本账也交出来（顺序与 `chain` 一致，不走的是 `None`）：准入要用它判断
/// 这一笔有没有真的动授信（告警）、账户欠不欠授信的钱（宽限），S0 的视图里只有净头寸，看不出来。
pub(super) fn project_chain_with_books(
    conn: &Connection,
    chain: &[Account],
    now_ms: i64,
    config: &LedgerConfig,
) -> Result<(Vec<AncestorView>, Vec<Option<Books>>), LedgerError> {
    let period = period_of(now_ms, &config.billing_timezone)?;
    let (period_start, period_end) = period_bounds(period, &config.billing_timezone)?;
    let not_revoked = not_revoked_clause(conn)?;
    let mut views = Vec::with_capacity(chain.len());
    let mut all_books = Vec::with_capacity(chain.len());
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

        let mut balance = if sums.has_funds {
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
        // 有过授信的账户：授信限制的是它的累计债务，旧债和新额度分开记（见模块文档）。没充值的一直走；充过值的要有过花费类额度才走
        // （没有额度时能花的钱就是 S0 的 `余额 + 授信`，视图保持 `balance`）。从没有过授信的账户不走。
        // `ceiling_remaining` 记成净头寸 `额度剩余 + 自有资金剩余 - min(债务, 授信额度)`，自有资金并在里面，所以 `balance` 是 `None`：
        // S0 的 `可用额度 + 授信额度` 就是 `额度剩余 + 自有资金剩余 + 授信可用`，同一笔花费先用额度，再用自有资金，最后才动授信；
        // 新额度照常可花，但不抵旧债；债务超过授信额度的部分不再往额度里扣。已花和在途预留都在债务里，重复请求、并发请求每一次
        // 都看得到前面欠下的。授信此刻失效（关掉、过期、被撤销）的账户授信额度是 0，没有授信可用，但欠下的债还在，所以也走这条路：
        // 新额度照常可花，不被旧债吃掉（回退到上面的额度窗口，窗口里更早的债会被算成这笔额度的消耗）。
        let books = if spends_money
            && (credit_limit > 0 || ever_had_credit_line(conn, id)?)
            && (!sums.has_funds || ever_had_allocation)
        {
            Some(separate_books(
                conn,
                id,
                now_ms,
                sums.funds,
                sums.committed,
            )?)
        } else {
            None
        };
        if let Some(books) = &books {
            balance = None;
            ceiling_remaining = Some(money(books.net_position(credit_limit)?));
        }
        all_books.push(books);

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
    Ok((views, all_books))
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

#[cfg(test)]
mod tests {
    //! 按归属时刻找候选额度的索引（[`WindowIndex`]）：每个时刻的候选额度必须恰好是「窗口包含它」的那些，次序是回放要的（先到期的，
    //! 同时到期的先创建的）。期望数字手算；另外和暴力扫描对一遍随机的窗口和时刻。
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn bucket(from: i64, until: i64) -> Bucket {
        Bucket {
            from,
            until,
            capacity: 1,
            created_after: 0,
            revoked_after: None,
        }
    }

    /// 暴力扫描：窗口包含 `at` 的额度，按（到期时间，创建先后）排好。
    fn brute_force(buckets: &[Bucket], at: i64) -> Vec<usize> {
        let mut found: Vec<usize> = (0..buckets.len())
            .filter(|index| buckets[*index].from <= at && at < buckets[*index].until)
            .collect();
        found.sort_by_key(|index| (buckets[*index].until, *index));
        found
    }

    #[test]
    fn each_moment_finds_exactly_the_windows_that_contain_it_in_replay_order() {
        // 0: [0, 10)  1: [5, 20)  2: [10, 15)  3: [20, 30)  4: [5, 20)（和 1 同时到期，后创建）
        let buckets = [
            bucket(0, 10),
            bucket(5, 20),
            bucket(10, 15),
            bucket(20, 30),
            bucket(5, 20),
        ];
        let index = WindowIndex::build(&buckets, WindowIndex::MAX_ENTRIES)
            .expect("a handful of windows is indexed");
        let expected: [(i64, &[usize]); 14] = [
            (-1, &[]),
            (0, &[0]),
            (4, &[0]),
            // 5 起 0（10 到期）先于 1 和 4（20 到期，1 先创建）
            (5, &[0, 1, 4]),
            (9, &[0, 1, 4]),
            // 窗口左闭右开：10 这一刻 0 已经结束，2 刚开始；2（15 到期）先于 1、4
            (10, &[2, 1, 4]),
            (14, &[2, 1, 4]),
            (15, &[1, 4]),
            (19, &[1, 4]),
            (20, &[3]),
            (29, &[3]),
            (30, &[]),
            (1_000_000, &[]),
            (i64::MIN, &[]),
        ];
        for (at, want) in expected {
            assert_eq!(index.at(at), want, "candidates at {at}");
        }
    }

    #[test]
    fn empty_and_degenerate_windows_are_never_candidates() {
        // 空窗口 [7, 7)、倒过来的窗口 [9, 3) 永远不包含任何时刻
        let buckets = [bucket(7, 7), bucket(9, 3), bucket(1, 2)];
        let index = WindowIndex::build(&buckets, WindowIndex::MAX_ENTRIES).unwrap();
        for at in -2..12 {
            assert_eq!(
                index.at(at),
                brute_force(&buckets, at).as_slice(),
                "at {at}"
            );
        }
        assert_eq!(index.at(1), &[2]);
        assert!(
            WindowIndex::build(&[], WindowIndex::MAX_ENTRIES)
                .unwrap()
                .at(0)
                .is_empty()
        );
    }

    #[test]
    fn the_index_agrees_with_a_scan_of_every_window_on_random_windows() {
        // xorshift64*：不引入依赖，结果确定
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move |n: u64| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
        };
        let mut checked = 0_u32;
        for round in 0..300 {
            let count = 1 + usize::try_from(next(40)).unwrap();
            let buckets: Vec<Bucket> = (0..count)
                .map(|_| {
                    let from = i64::try_from(next(60)).unwrap() - 10;
                    let length = i64::try_from(next(30)).unwrap();
                    bucket(from, from + length)
                })
                .collect();
            let index = WindowIndex::build(&buckets, WindowIndex::MAX_ENTRIES).unwrap();
            for at in -15..90 {
                assert_eq!(
                    index.at(at),
                    brute_force(&buckets, at).as_slice(),
                    "round {round}, moment {at}, windows {:?}",
                    buckets
                        .iter()
                        .map(|b| (b.from, b.until))
                        .collect::<Vec<_>>()
                );
                checked += 1;
            }
        }
        assert!(checked >= 30_000);
    }

    #[test]
    fn replaying_through_the_index_gives_what_the_linear_scan_gives() {
        // 随机的额度（窗口、创建 / 撤销序号）和花费（归属时刻、序号、金额）：走索引的回放和退回线性扫描（限额 0 = 一个快照都不让建）
        // 的回放，没被覆盖的总额和每笔额度的剩余必须一模一样。线性扫描就是索引之前的算法。
        let mut state = 0xD1B5_4A32_D192_ED03_u64;
        let mut next = move |n: u64| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
        };
        for round in 0..400 {
            let count = usize::try_from(next(25)).unwrap();
            let buckets: Vec<Bucket> = (0..count)
                .map(|_| {
                    let from = i64::try_from(next(80)).unwrap();
                    let created_after = i64::try_from(next(60)).unwrap();
                    Bucket {
                        from,
                        until: from + i64::try_from(next(40)).unwrap(),
                        capacity: 1 + i64::try_from(next(150)).unwrap(),
                        created_after,
                        revoked_after: (next(3) == 0)
                            .then(|| created_after + i64::try_from(next(40)).unwrap()),
                    }
                })
                .collect();
            let mut spends: Vec<Spend> = (0..usize::try_from(next(60)).unwrap())
                .map(|_| Spend {
                    at: i64::try_from(next(130)).unwrap() - 5,
                    first_entry: 1 + i64::try_from(next(100)).unwrap(),
                    amount: i64::try_from(next(120)).unwrap(),
                })
                .collect();
            spends.sort_by_key(|spend| spend.first_entry);
            let through_index = replay_spends(&buckets, &spends, WindowIndex::MAX_ENTRIES).unwrap();
            let linear = replay_spends(&buckets, &spends, 0).unwrap();
            assert_eq!(through_index, linear, "round {round}");
        }
    }

    #[test]
    fn a_spend_is_covered_by_the_earliest_expiring_window_that_holds_it_and_the_rest_is_uncovered()
    {
        // 手算：额度 0 [0, 10) 容量 30，额度 1 [0, 20) 容量 50，额度 2 [15, 30) 容量 40，都在所有花费之前创建、没撤销。
        // 花费按提交先后：
        //   a  时刻 5   金额 40：窗口包含它的是 0、1，先到期的先用：0 出 30，剩下的 10 由 1 出 -> 0 剩 0，1 剩 40
        //   b  时刻 17  金额 60：窗口包含它的是 1、2（0 已经结束）：1（20 到期）先于 2（30 到期）：1 出 40，2 出 20 -> 1 剩 0，2 剩 20
        //   c  时刻 40  金额 7：没有额度的窗口包含它：7 没被覆盖
        //   d  时刻 3   金额 5：窗口包含它的是 0、1，都已经用光：5 没被覆盖
        let buckets = [
            Bucket {
                from: 0,
                until: 10,
                capacity: 30,
                created_after: 0,
                revoked_after: None,
            },
            Bucket {
                from: 0,
                until: 20,
                capacity: 50,
                created_after: 0,
                revoked_after: None,
            },
            Bucket {
                from: 15,
                until: 30,
                capacity: 40,
                created_after: 0,
                revoked_after: None,
            },
        ];
        let spends = [
            Spend {
                at: 5,
                first_entry: 1,
                amount: 40,
            },
            Spend {
                at: 17,
                first_entry: 2,
                amount: 60,
            },
            Spend {
                at: 40,
                first_entry: 3,
                amount: 7,
            },
            Spend {
                at: 3,
                first_entry: 4,
                amount: 5,
            },
        ];
        for limit in [WindowIndex::MAX_ENTRIES, 0] {
            let (uncovered, left) = replay_spends(&buckets, &spends, limit).unwrap();
            assert_eq!(uncovered, 7 + 5, "index limit {limit}");
            assert_eq!(left, [0, 0, 20], "index limit {limit}");
        }
    }

    #[test]
    fn an_absurd_number_of_overlapping_windows_is_not_indexed() {
        // 1 500 笔窗口各自在不同的时刻开始、同一时刻结束：第 j 个边界上有 j 笔同时生效，快照总量 ≈ 112 万，超过上限：不建，
        // 回放退回线性扫描（结果一样）
        let buckets: Vec<Bucket> = (0..1_500).map(|j| bucket(j, 1_000_000)).collect();
        assert!(WindowIndex::build(&buckets, WindowIndex::MAX_ENTRIES).is_none());
        // 同样多的窗口，但是互不重叠（月度额度那样）：每个边界上只有一笔，当然可以建
        let monthly: Vec<Bucket> = (0..1_500).map(|j| bucket(j * 10, j * 10 + 10)).collect();
        let index = WindowIndex::build(&monthly, WindowIndex::MAX_ENTRIES)
            .expect("disjoint windows are cheap to index");
        assert_eq!(index.at(0), &[0]);
        assert_eq!(index.at(14_995), &[1_499]);
        assert!(index.at(15_000).is_empty());
    }
}
