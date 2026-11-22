//! 「授信的旧债与新额度分开记」（方案 C）的两个**独立于被测代码**的模型，给没充值（`ledger_credit_books.rs`）和充过值
//! （`ledger_credit_books_funded.rs`）两套测试共用。
//!
//! * [`model`]：读原始表（`allocations`、`allocation_revocations`、`ledger_allocation_marks`、`ledger_entries`），把「额度创建」
//!   「额度撤销」「一笔花费」当成同一条时间线上的事件，按提交先后逐件回放，再把充值加进来；不经过被测的投影函数。
//!   给并发测试的采样者线程用（竞争期间只能读库）。
//! * [`Sim`]：**不读库**的事件模型。只吃测试自己做过的事（充值、发额度、撤销额度、预留、释放、结算），每个事件拿一个全局递增的号
//!   （调用顺序 = 提交顺序），期望数字从这些事件和业务规则直接推出来：
//!   预留是创建时刻上的一笔花费；结算时预留之内的部分留在原处，超出预留的部分是结算那一刻的一笔**新**花费。
//!   随机历史用它预测账本该有的样子，这样「超出预留的部分记在哪」「读钟早于已提交花费时怎么判」这两件事，期望从来不是账本自己
//!   的分录或被测实现算出来的。
//!
//! 两个模型和被测实现共用的只有一条业务规则本身（回放顺序：一笔花费先用**自己归属时刻**窗口包含它的、它之前创建且没被撤销的额度里
//! 先到期的，额度之外的部分先用自有资金，用完才是授信债务）；规则里的每一个输入（归属时刻、提交先后、读钟）都各自取得。
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::Duration;

/// 一个账户上额度、自有资金、授信债务三本账（没充值的账户 `funds` 是 0）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Books {
    /// 此刻还能花的额度：没被撤销、此刻生效的额度，扣掉它们已经覆盖的花费之后的剩余。
    pub allowance_left: i64,
    /// 已花（含在途预留）里被额度覆盖的部分。
    pub covered: i64,
    /// 已花（含在途预留）里没被额度覆盖的部分：由自有资金和授信承担。
    pub uncovered: i64,
    /// 全部充值。
    pub funds: i64,
    /// 自有资金剩余：`max(充值 - 没被额度覆盖的花费, 0)`。
    pub funds_left: i64,
    /// 授信债务：`max(没被额度覆盖的花费 - 充值, 0)`。
    pub credit_debt: i64,
    /// 已花加在途：全部花费的净额之和。
    pub spent: i64,
}

impl Books {
    /// 不动授信还能花的钱。
    pub fn without_credit(&self) -> i64 {
        self.allowance_left + self.funds_left
    }

    /// 授信额度为 `credit` 时一共还能花的钱：额度剩余 + 自有资金剩余 + 授信可用。
    pub fn room(&self, credit: i64) -> i64 {
        self.without_credit() + credit - self.credit_debt.min(credit)
    }

    /// 视图里 `ceiling_remaining` 应该是的净头寸：`额度剩余 + 自有资金剩余 - min(债务, 授信额度)`。
    pub fn net(&self, credit: i64) -> i64 {
        self.without_credit() - self.credit_debt.min(credit)
    }
}

/// 回放用的一笔额度。
struct ReplayBucket {
    from: i64,
    until: i64,
    left: i64,
    revoked: bool,
}

/// 回放用的一笔花费：归属时刻、净额。
struct ReplaySpend {
    at: i64,
    amount: i64,
}

/// 事件按提交先后逐件回放出三本账。`events` 里是 `(提交序号, 种类, 下标)`，种类 0 = 一笔花费，1 = 额度创建，2 = 额度撤销；
/// 同一个序号上花费排在额度事件之前（创建序号 c 的额度只覆盖序号大于 c 的花费，撤销序号 r 的额度仍然覆盖序号不大于 r 的花费）。
fn replay(
    mut events: Vec<(i64, u8, usize)>,
    mut buckets: Vec<ReplayBucket>,
    spends: &[ReplaySpend],
    funds: i64,
    now: i64,
) -> Books {
    events.sort_unstable();
    let mut known: Vec<usize> = Vec::new();
    let (mut covered, mut uncovered, mut spent) = (0_i64, 0_i64, 0_i64);
    for (_, kind, index) in events {
        match kind {
            // 额度创建：此后的花费才可能用它
            1 => known.push(index),
            // 额度撤销：此后的花费不再用它，剩下的额度作废
            2 => buckets[index].revoked = true,
            _ => {
                let ReplaySpend { at, amount } = spends[index];
                spent += amount;
                let mut rest = amount;
                let mut usable: Vec<usize> = known
                    .iter()
                    .copied()
                    .filter(|i| {
                        !buckets[*i].revoked && buckets[*i].from <= at && at < buckets[*i].until
                    })
                    .collect();
                // 先到期的先用；同时到期的先创建的先用
                usable.sort_by_key(|i| (buckets[*i].until, *i));
                for i in usable {
                    let take = rest.min(buckets[i].left);
                    buckets[i].left -= take;
                    rest -= take;
                    covered += take;
                }
                uncovered += rest;
            }
        }
    }
    let allowance_left = buckets
        .iter()
        .filter(|bucket| !bucket.revoked && bucket.from <= now && now < bucket.until)
        .map(|bucket| bucket.left)
        .sum();
    Books {
        allowance_left,
        covered,
        uncovered,
        funds,
        funds_left: (funds - uncovered).max(0),
        credit_debt: (uncovered - funds).max(0),
        spent,
    }
}

/// 一个预留的记录（状态、预留额、实际消费、宽限下限）按手算规则推出的两笔花费：（预留本身，结算那一刻才发生的花费）。
///
/// 预留本身是预留创建时刻上的一笔（在途 = 预留额；结算之后 = 预留之内的部分 `min(实际, 预留) - 宽限承担`；放掉 / 过期 = 0；迟到的结算 = 0）；
/// 超出预留的部分是结算那一刻的另一笔（没迟到的结算 `max(实际 - 预留, 0)`；迟到的结算整笔 `实际 - 宽限承担`）。宽限里平台承担的部分
/// （预留之内、超出放行时下限的那一块）不记到付款账户链上。分录是旧格式、中间形态还是新格式，这里是同一回事。
fn pieces_of(
    state: &str,
    late: bool,
    reserved: i64,
    charged: i64,
    floor: Option<i64>,
) -> (i64, i64) {
    let within = charged.min(reserved);
    let absorbed = floor.map_or(0, |floor| (within - floor).max(0));
    match (state, late) {
        ("open", _) => (reserved, 0),
        ("settled", false) => (within - absorbed, (charged - reserved).max(0)),
        ("settled", true) => (0, charged - absorbed),
        // 放掉了、过期了（还没有迟到的结算）：不占任何额度
        _ => (0, 0),
    }
}

/// 读原始表（`ledger_holds` 和这个账户上有分录的预留），一个账户上每个预留按手算规则（[`pieces_of`]）推出的花费：（归属时刻，净额）。
/// **不看分录的 `window_at_ms`**：预留本身在预留创建的时刻，结算那一刻才发生的花费在结算的时刻（预留记录里的 `closed_at_ms`）。
/// 窗口求和（额度窗口、速率窗口、补贴池的账期）的期望从这里来，旧格式、中间形态、新格式的分录读出来必须是同一份。
pub fn attributed_spends(db_path: &Path, account: &str) -> Vec<(i64, i64)> {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.busy_timeout(Duration::from_secs(10)).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT h.state, h.late, h.reserved_micros, COALESCE(h.charged_micros, 0), h.created_at_ms,
                    COALESCE(h.closed_at_ms, 0), h.grace_floor_micros
               FROM ledger_holds h
              WHERE EXISTS (SELECT 1 FROM ledger_entries e WHERE e.hold_id = h.hold_id AND e.account_id = ?1)
              ORDER BY h.hold_id",
        )
        .unwrap();
    let rows = stmt
        .query_map([account], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? == 1,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<i64>>(6)?,
            ))
        })
        .unwrap();
    let mut spends = Vec::new();
    for row in rows {
        let (state, late, reserved, charged, created_at, closed_at, floor) = row.unwrap();
        let (reservation, settled_later) = pieces_of(&state, late, reserved, charged, floor);
        if reservation != 0 {
            spends.push((created_at, reservation));
        }
        if settled_later != 0 {
            spends.push((closed_at, settled_later));
        }
    }
    spends
}

/// [`attributed_spends`] 里归属时刻满足 `window` 的花费之和。
pub fn spent_where(db_path: &Path, account: &str, window: impl Fn(i64) -> bool) -> i64 {
    attributed_spends(db_path, account)
        .into_iter()
        .filter(|(at, _)| window(*at))
        .map(|(_, amount)| amount)
        .sum()
}

/// 读原始表，按事件逐笔回放（不经过被测代码）：
///
/// * 事件按**提交先后**排：分录号是提交的先后；额度创建 / 撤销的序号 `after_entry` 是「它提交那一刻账本里最后一条分录的号」，
///   所以它排在这个号的分录之后、下一个号的分录之前；毫秒只用来判断额度自己的窗口 `[valid_from, valid_until)`；
/// * **花费从预留自己的记录（`ledger_holds`：状态、预留额、实际消费、结算时刻、宽限下限）推出来，不看分录是怎么拆的**（Codex 复审 7c5a03fe）：
///   预留本身是创建时刻上的一笔花费（在途 = 预留额；结算之后 = 预留之内的部分 `min(A, R) - 宽限承担`；放掉 / 过期 = 0；迟到的结算 = 0）；
///   超出预留的部分是结算那一刻的另一笔花费（没迟到的结算 `max(A - R, 0)`；迟到的结算整笔 `A - 宽限承担`），归属时刻是预留收尾的时刻。
///   所以旧格式（超出的部分和预留之内的部分一起记在一条 `settle` 里）和新格式（超出的部分另记一条 `late_settle`）在这里是同一回事，
///   不按分录种类分组，也不按「同一个归属时刻」合并（预留和它的 overage 可以在同一毫秒）。先后看各自的分录号：预留看 `reserve` 那条，
///   结算时刻的花费看这个账户上最后一条 `settle` / `late_settle`（同一个事务里，序号和别的事件比出来的先后一样）；
/// * 推出来的花费和分录对账：账户的 `Σ (held + spent)` 必须等于这些花费的总和（分录的数额和预留的记录对得上），否则 panic；
/// * 每笔花费按它**自己**记下的归属时刻判断，不看读的是哪一刻；所有额度都参与回放（包括读钟之后才开始的）；`now` 只决定哪些额度
///   此刻还能花；
/// * 回放时维护「此刻已经创建、没被撤销的额度」：一笔花费先用其中窗口包含它的归属时刻的额度里**先到期**的（同时到期的先创建的先用），
///   用完的部分记成「没被额度覆盖」；
/// * 额度之外的花费先用自有资金（全部充值，不分先后：充值不过期），用完才是授信债务。
pub fn model(db_path: &Path, account: &str, now: i64) -> Books {
    let mut buckets: Vec<ReplayBucket> = Vec::new();
    let mut events: Vec<(i64, u8, usize)> = Vec::new();
    let mut spends: Vec<ReplaySpend> = Vec::new();
    let funds: i64;
    {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.busy_timeout(Duration::from_secs(10)).unwrap();
        // 一个读事务里读完所有表：竞争测试里采样者线程读的时候别的线程还在写，分开读会读到不一致的快照
        conn.execute_batch("BEGIN").unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT a.valid_from_ms, a.valid_until_ms, a.ceiling_micros,
                        (SELECT m.after_entry FROM ledger_allocation_marks m
                          WHERE m.allocation_id = a.allocation_id AND m.event = 'created'),
                        (SELECT m.after_entry FROM ledger_allocation_marks m
                          WHERE m.allocation_id = a.allocation_id AND m.event = 'revoked')
                   FROM allocations a
                  WHERE a.child_id = ?1 AND a.purpose IN ('free_tier', 'prepaid', 'org_quota')
                    AND a.ceiling_micros > 0
                  ORDER BY a.rowid",
            )
            .unwrap();
        let rows = stmt
            .query_map([account], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            })
            .unwrap();
        for row in rows {
            let (from, until, cap, created_after, revoked_after) = row.unwrap();
            let index = buckets.len();
            buckets.push(ReplayBucket {
                from,
                until,
                left: cap,
                revoked: false,
            });
            events.push((
                created_after.expect("every allocation has a creation mark"),
                1,
                index,
            ));
            if let Some(after) = revoked_after {
                events.push((after, 2, index));
            }
        }
        drop(stmt);
        // 这个账户上有分录的每个预留：预留自己的记录，加上这个账户上 `reserve` 那条和最后一条 `settle` / `late_settle` 的分录号
        let mut stmt = conn
            .prepare(
                "SELECT h.hold_id, h.state, h.late, h.reserved_micros, COALESCE(h.charged_micros, 0), h.created_at_ms,
                        COALESCE(h.closed_at_ms, 0), h.grace_floor_micros,
                        MIN(CASE WHEN e.kind = 'reserve' THEN e.entry_id END),
                        MAX(CASE WHEN e.kind IN ('settle', 'late_settle') THEN e.entry_id END)
                   FROM ledger_holds h JOIN ledger_entries e ON e.hold_id = h.hold_id AND e.account_id = ?1
                  GROUP BY h.hold_id
                  ORDER BY h.hold_id",
            )
            .unwrap();
        let rows = stmt
            .query_map([account], |row| {
                Ok((
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)? == 1,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                ))
            })
            .unwrap();
        for row in rows {
            let (
                state,
                late,
                reserved,
                charged,
                created_at,
                closed_at,
                floor,
                reserve_entry,
                settle_entry,
            ) = row.unwrap();
            let (reservation, settled_later) = pieces_of(&state, late, reserved, charged, floor);
            if reservation > 0 {
                let first =
                    reserve_entry.expect("a hold with money on the books has a reserve entry");
                events.push((first, 0, spends.len()));
                spends.push(ReplaySpend {
                    at: created_at,
                    amount: reservation,
                });
            }
            if settled_later > 0 {
                let first = settle_entry.expect("a settled hold has a settle or late_settle entry");
                events.push((first, 0, spends.len()));
                spends.push(ReplaySpend {
                    at: closed_at,
                    amount: settled_later,
                });
            }
        }
        drop(stmt);
        // 对账：预留的记录推出来的花费总和，必须等于分录里这个账户的 `Σ (held + spent)`
        let on_the_books: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(held_micros + spent_micros), 0) FROM ledger_entries WHERE account_id = ?1",
                [account],
                |row| row.get(0),
            )
            .unwrap();
        let derived: i64 = spends.iter().map(|spend| spend.amount).sum();
        assert_eq!(
            derived, on_the_books,
            "the spends the hold records imply must add up to what the entries book on {account}"
        );
        funds = conn
            .query_row(
                "SELECT COALESCE(SUM(funds_micros), 0) FROM ledger_entries WHERE account_id = ?1 AND kind = 'fund'",
                [account],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute_batch("COMMIT").unwrap();
    }
    replay(events, buckets, &spends, funds, now)
}

// ------------------------------------------------------------------------------------------
// 不读库的事件模型
// ------------------------------------------------------------------------------------------

/// 一笔额度。
#[derive(Debug, Clone)]
struct SimBucket {
    from: i64,
    until: i64,
    cap: i64,
    /// 发出它的事件号。
    created: u64,
    /// 撤销它的事件号（没撤销是 `None`）。
    revoked: Option<u64>,
}

/// 一笔花费。
#[derive(Debug, Clone)]
struct SimSpend {
    /// 归属时刻。
    at: i64,
    /// 当前净额：预留额；结算之后是预留之内的部分；释放之后是 0。
    amount: i64,
    /// 预留额（结算时用来分出「超出预留」的部分）。
    reserved: i64,
    /// 这笔花费的事件号。
    seq: u64,
}

/// 一次没迟到的结算按手算规则应有的分摊。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimSettle {
    /// 超出预留的部分。
    pub overage: i64,
    /// 担保人承担的。
    pub sponsor: i64,
    /// 平台损失（吸收不了的，凭证因此被冻结）。
    pub loss: i64,
}

/// 只吃测试自己做过的事、不读库的事件模型（见模块文档）。事件按调用顺序拿号：号越大提交越晚，所以「额度在花费之前创建」
/// 「撤销在花费之后」直接比号，不需要分录号、也不需要毫秒。
#[derive(Debug, Clone)]
pub struct Sim {
    credit: i64,
    funds: i64,
    clock: u64,
    buckets: Vec<SimBucket>,
    spends: Vec<SimSpend>,
}

impl Sim {
    /// `credit`：授信额度（整个历史里不变）。
    pub fn new(credit: i64) -> Self {
        Self {
            credit,
            funds: 0,
            clock: 0,
            buckets: Vec::new(),
            spends: Vec::new(),
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// 充值（全部充值只看总数）。
    pub fn top_up(&mut self, micros: i64) {
        self.funds += micros;
    }

    /// 发一笔额度，窗口 `[from, until)`。返回它的编号（撤销时用）。
    pub fn allowance(&mut self, from: i64, until: i64, cap: i64) -> usize {
        let created = self.tick();
        self.buckets.push(SimBucket {
            from,
            until,
            cap,
            created,
            revoked: None,
        });
        self.buckets.len() - 1
    }

    /// 撤销一笔额度。
    pub fn revoke(&mut self, allowance: usize) {
        let seq = self.tick();
        self.buckets[allowance].revoked = Some(seq);
    }

    /// 在时刻 `at` 预留 `micros`。返回这笔花费的编号。
    pub fn reserve(&mut self, at: i64, micros: i64) -> usize {
        let seq = self.tick();
        self.spends.push(SimSpend {
            at,
            amount: micros,
            reserved: micros,
            seq,
        });
        self.spends.len() - 1
    }

    /// 释放：这笔预留不再占任何额度。
    pub fn release(&mut self, spend: usize) {
        self.spends[spend].amount = 0;
    }

    /// 没迟到的结算：预留之内的部分（最多预留额）留在原处，超出预留的部分是结算那一刻（`at`）的一笔新花费。返回这次结算的分摊（手算规则）：
    ///
    /// * 先把预留之内的部分记账，**再**读整条链的余量（预留还占着的时候，宽限里平台承担的那一块也占着余额，不能先读）；
    /// * 担保人能吸收 `min(超出, max(余量, 0))`（链上别的账户都很宽裕，整条链的余量就是这个账户的 `额度剩余 + 自有资金剩余 + 授信可用`）；
    /// * 吸收不了的是平台损失，并且冻结凭证；
    /// * 然后超出的部分才记成结算时刻的一笔新花费。
    pub fn settle(&mut self, spend: usize, charged: i64, at: i64) -> SimSettle {
        let reserved = self.spends[spend].reserved;
        self.spends[spend].amount = charged.min(reserved);
        let overage = (charged - reserved).max(0);
        let room = self.room(at);
        let sponsor = overage.min(room.max(0));
        if overage > 0 {
            let seq = self.tick();
            self.spends.push(SimSpend {
                at,
                amount: overage,
                reserved: 0,
                seq,
            });
        }
        SimSettle {
            overage,
            sponsor,
            loss: overage - sponsor,
        }
    }

    /// 读钟是 `now` 时的三本账：全部花费按事件顺序回放，每笔先用自己归属时刻窗口包含它的、它之前发出且没被它之前撤销的额度里
    /// 先到期的；`now` 只决定哪些额度此刻还能花（窗口包含 `now`、没被撤销）。
    pub fn books(&self, now: i64) -> Books {
        let mut left: Vec<i64> = self.buckets.iter().map(|bucket| bucket.cap).collect();
        let (mut covered, mut uncovered, mut spent) = (0_i64, 0_i64, 0_i64);
        for spend in &self.spends {
            spent += spend.amount;
            let mut usable: Vec<usize> = (0..self.buckets.len())
                .filter(|i| {
                    let bucket = &self.buckets[*i];
                    bucket.created < spend.seq
                        && bucket.from <= spend.at
                        && spend.at < bucket.until
                        && bucket.revoked.is_none_or(|revoked| spend.seq < revoked)
                })
                .collect();
            usable.sort_by_key(|i| (self.buckets[*i].until, *i));
            let mut rest = spend.amount;
            for i in usable {
                let take = rest.min(left[i]);
                left[i] -= take;
                rest -= take;
                covered += take;
            }
            uncovered += rest;
        }
        let allowance_left = self
            .buckets
            .iter()
            .enumerate()
            .filter(|(_, bucket)| {
                bucket.revoked.is_none() && bucket.from <= now && now < bucket.until
            })
            .map(|(i, _)| left[i])
            .sum();
        Books {
            allowance_left,
            covered,
            uncovered,
            funds: self.funds,
            funds_left: (self.funds - uncovered).max(0),
            credit_debt: (uncovered - self.funds).max(0),
            spent,
        }
    }

    /// 读钟是 `now` 时，一共还能花的钱。
    pub fn room(&self, now: i64) -> i64 {
        self.books(now).room(self.credit)
    }

    /// 已经提交的花费里最晚的归属时刻（没有花费是 `i64::MIN`）：读钟比它更早的请求，是排队乱序读到了旧钟。
    pub fn latest_at(&self) -> i64 {
        self.spends
            .iter()
            .map(|spend| spend.at)
            .max()
            .unwrap_or(i64::MIN)
    }
}
