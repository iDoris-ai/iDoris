//! 准入：沿付款账户链**原子地**检查五项并占住预留（设计稿 §5）。
//!
//! 分两步，调用方在**同一个** `BEGIN IMMEDIATE` 事务里依次做（`AccountStore::admit_attempt`）：
//!
//! 1. [`decide`]：扫过期的预留、释放这个请求之前失败的候选占着的预留、核对凭证与账户链还有效、
//!    把整条链的投影读出来、跑 S0 的 `evaluate_admission`、决定放行 / 降级 / 宽限 / 拒绝；
//! 2. [`apply`]：写预留行和沿链的 `reserve` 分录（以及宽限记录、告警）。
//!
//! 检查和写入在同一个事务里，而 SQLite 的 `BEGIN IMMEDIATE` 一开始就拿写锁，所以两个线程 / 进程同时准入
//! 同一棵树，第二个一定看得到第一个已经提交的预留，不可能都通过检查再都写进去（这是账本注释里反复写的
//! LiteLLM #32614 那一类超发）。`mutation-test-hooks` 特性下有一个钩子可以在两步之间提交，用来证明
//! 并发测试抓得住这一类退化（见 `hooks.rs`）。
//!
//! # 阶梯（设计稿 §5 体验分级）
//!
//! 1. 全额预留五项都过：**放行**（`Allow`），没有原因码。
//! 2. 只有「钱不够」（`Funds` / `CreditFloor`）不过：先用 S0 的 `fit_max_tokens` 把 `max_tokens` 压到放得下的最大值：
//!    **降级**（`Degrade`，`budget_degraded`），发给上游的 `max_tokens` 就是压低后的值。
//! 3. 压低也放不下：`allow_grace = false` 时返回 `budget_exhausted`（这个候选放不下；调用方会去试下一个更便宜的候选）。
//!    `allow_grace = true`（调用方把所有候选都试过、都放不下之后）才看**宽限**：只有付款账户自己的「钱」是唯一不过的项、
//!    本周期没用过宽限、账户没有透支，才放行（`Grace`，`grace_used`），预留额是完整的一次预留。
//! 4. 其余情况：**拒绝**，原因码按第一个阻断项（`budget_exhausted` / `rate_limited` / `in_flight_limited` /
//!    `subsidy_pool_exhausted`）。速率、在途、补贴池是平台的保护，降级和宽限都绕不过。

use idoris_compute_contracts::account::{Account, AccountStatus};
use idoris_compute_contracts::admission::{
    AdmissionDecision, AdmissionReport, Admitted, CheckKind, CheckOutcome, DegradeReason,
    GraceDenied, Reservation, check_grace, evaluate_admission,
};
use idoris_compute_contracts::ids::AccountId;
use idoris_compute_contracts::money::Unit;
use idoris_compute_contracts::pricing::Price;
use idoris_compute_contracts::reason::ReasonCode;
use idoris_compute_contracts::reserve::{ReserveRequest, fit_max_tokens, reserve};
use rusqlite::{Connection, OptionalExtension, params};

use super::error::{LedgerError, add};
use super::holds::{
    NewAlert, NewHold, hold_id_of, insert_alert, insert_hold_and_reserve, load_hold,
    release_earlier_attempts, sweep_expired_in,
};
use super::period::period_of;
use super::project::{
    SPENDABLE_PURPOSES, available_of, load_config, not_revoked_clause, project_chain,
};
use super::types::{AdmitInput, AlertKind, MODEL_COMPONENT};
use crate::accounts::rows::load_ancestors;

/// `decide` 的结果：决定本身，加上 `apply` 要写的东西（拒绝时没有）。
pub(super) struct AdmitPlan {
    pub decision: AdmissionDecision,
    write: Option<HoldWrite>,
}

struct HoldWrite {
    chain: Vec<Account>,
    unit: Unit,
    reserved: i64,
    period: String,
    expires_at_ms: i64,
    grace: Option<GraceWrite>,
    /// 靠授信放行的账户：（链上的位置，透支额）。要通知担保人。
    credit_used: Vec<(usize, i64)>,
}

struct GraceWrite {
    floor: i64,
    shortfall: i64,
}

fn reject(code: ReasonCode) -> Result<AdmitPlan, LedgerError> {
    Ok(AdmitPlan {
        decision: AdmissionDecision::reject(code)?,
        write: None,
    })
}

/// 凭证、主体、成员关系、账户链此刻还有效吗。这是在预留事务里对付款上下文的最后一次核对：付款上下文是
/// 请求刚进来时解析的，之后凭证可能被撤销、冻结（包括被上一次结算的平台损失冻结），账户可能被冻结，
/// 这些都必须在**占预留的同一个事务里**拦住，不留「撤销了还放行一次」的缝。
///
/// 顺序与 S0 `resolve_payment_context` 一致：凭证（撤销 → 冻结 → 过期）→ 主体 → 成员关系 → 账户。
fn gate(
    conn: &Connection,
    input: &AdmitInput<'_>,
    chain: &[Account],
    now_ms: i64,
) -> Result<Option<ReasonCode>, LedgerError> {
    let credential: Option<(String, Option<i64>)> = conn
        .query_row(
            "SELECT status, expires_at_ms FROM credentials WHERE credential_id = ?1",
            [input.payment.credential().as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((status, expires_at)) = credential else {
        // 账户库里没有这张凭证：付款上下文不是账户库解析出来的，不能放行。
        return Ok(Some(ReasonCode::PaymentContextInvalid));
    };
    match status.as_str() {
        "revoked" => return Ok(Some(ReasonCode::CredentialRevoked)),
        "frozen" => return Ok(Some(ReasonCode::CredentialFrozen)),
        "active" => {}
        other => {
            return Err(LedgerError::Corrupt(format!(
                "unknown credential status {other:?}"
            )));
        }
    }
    if let Some(expires_at) = expires_at
        && now_ms >= expires_at
    {
        return Ok(Some(ReasonCode::CredentialExpired));
    }
    if let Some(principal) = input.payment.principal() {
        let status: Option<String> = conn
            .query_row(
                "SELECT status FROM principals WHERE principal_id = ?1",
                [principal.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        match status.as_deref() {
            Some("active") => {}
            Some("revoked") => return Ok(Some(ReasonCode::PrincipalRevoked)),
            _ => return Ok(Some(ReasonCode::PaymentContextInvalid)),
        }
    }
    if let Some(membership) = input.payment.membership() {
        let status: Option<String> = conn
            .query_row(
                "SELECT status FROM memberships WHERE membership_id = ?1",
                [membership.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        match status.as_deref() {
            Some("active") => {}
            Some("revoked") => return Ok(Some(ReasonCode::MembershipRevoked)),
            _ => return Ok(Some(ReasonCode::PaymentContextInvalid)),
        }
    }
    if chain
        .iter()
        .any(|account| account.status != AccountStatus::Active)
    {
        return Ok(Some(ReasonCode::AccountInactive));
    }
    Ok(None)
}

/// 链上哪些账户靠授信放行（`CreditFloor` 是 `PassUsingCredit`）。
fn credit_users(report: &AdmissionReport, chain: &[Account]) -> Vec<(usize, i64)> {
    report
        .results()
        .iter()
        .filter_map(|result| match (&result.check, &result.outcome) {
            (CheckKind::CreditFloor, CheckOutcome::PassUsingCredit { overdraft }) => chain
                .iter()
                .position(|account| account.id == result.account)
                .map(|index| (index, overdraft.micros)),
            _ => None,
        })
        .collect()
}

/// 这个账户有没有被配置过：充值过，或者有过花费类额度（不管现在还生不生效，**但被撤销的不算**）。
///
/// 被撤销的额度是管理员明确关掉的（S2 的「关授信」）：撤销之后这个账户不该再有一次免费的宽限。
fn is_provisioned(conn: &Connection, account: &AccountId) -> Result<bool, LedgerError> {
    let not_revoked = not_revoked_clause(conn)?;
    Ok(conn.query_row(
        &format!(
            "SELECT EXISTS (SELECT 1 FROM ledger_entries WHERE account_id = ?1 AND kind = 'fund')
                 OR EXISTS (SELECT 1 FROM allocations
                             WHERE child_id = ?1 AND purpose IN {SPENDABLE_PURPOSES}{not_revoked})"
        ),
        [account.as_str()],
        |row| row.get(0),
    )?)
}

/// 读：这个账户本账期已经用掉几次宽限（没作废的）。
fn active_grace_uses(
    conn: &Connection,
    account: &AccountId,
    period: &str,
) -> Result<u32, LedgerError> {
    let used: i64 = conn.query_row(
        "SELECT COUNT(*) FROM ledger_grace_uses
          WHERE account_id = ?1 AND period = ?2 AND voided_at_ms IS NULL",
        params![account.as_str(), period],
        |row| row.get(0),
    )?;
    u32::try_from(used).map_err(|_| LedgerError::Corrupt(format!("grace use count {used}")))
}

/// 第一步：决定。会写的只有「扫过期」和「释放之前失败的候选」，这两件事不管最后放行还是拒绝都该留下。
pub(super) fn decide(conn: &Connection, input: &AdmitInput<'_>) -> Result<AdmitPlan, LedgerError> {
    let now_ms = input.now.millis();
    let config = load_config(conn)?;
    sweep_expired_in(conn, now_ms)?;

    let payer = input.payment.payer();
    let unit = input.payment.unit();
    if input.price_rates.unit() != unit {
        return Err(LedgerError::Invalid(format!(
            "the price rates are in {} but the payer pays in {unit}",
            input.price_rates.unit()
        )));
    }
    let hold_id = hold_id_of(input.attempt, MODEL_COMPONENT);
    if load_hold(conn, &hold_id)?.is_some() {
        return Err(LedgerError::HoldExists(hold_id));
    }
    // 切换候选：这个请求之前失败的候选不会再是作答的那一次，用户对它们付 0。先放掉它们占的额度，
    // 新候选的检查才看得到（否则余额刚好够一次的孩子会因为上一次失败的尝试还占着额度被拒）。
    if input.attempt.index().0 > 0 {
        release_earlier_attempts(
            conn,
            input.attempt.request(),
            input.attempt.index().0,
            now_ms,
            &config,
        )?;
    }

    let chain = load_ancestors(conn, payer)?;
    let Some(payer_account) = chain.first() else {
        return reject(ReasonCode::PaymentContextInvalid);
    };
    if payer_account.unit != *unit {
        return reject(ReasonCode::PaymentContextInvalid);
    }
    if let Some(code) = gate(conn, input, &chain, now_ms)? {
        return reject(code);
    }

    let full = reserve(input.policy, &input.sizing, input.price_rates)?;
    let views = project_chain(conn, &chain, now_ms, &config)?;
    let report = evaluate_admission(&full.amount, &views)?;
    let period = period_of(now_ms, &config.billing_timezone)?;

    // 选定这次放行的预留额、发给上游的 max_tokens、决定类别。
    enum Tier {
        Allow,
        Degrade,
        Grace,
    }
    let (tier, price, effective, final_report, grace) = if report.passed() {
        (
            Tier::Allow,
            full.amount.clone(),
            full.effective_max_tokens,
            report,
            None,
        )
    } else if report.only_budget_blocked() {
        // 第 2 级：压低 max_tokens
        let fitted = match report.headroom()? {
            Some(headroom) => {
                fit_max_tokens(input.policy, &input.sizing, input.price_rates, &headroom)?
            }
            None => None,
        };
        let fitted = match fitted {
            Some(fit) => {
                let again = evaluate_admission(&fit.amount, &views)?;
                // 压低后的预留额必须让五项全过（`max_admissible` 就是精确的阈值）；万一不过就当放不下，不放行。
                again.passed().then_some((fit, again))
            }
            None => None,
        };
        match fitted {
            Some((fit, again)) => (
                Tier::Degrade,
                fit.amount,
                fit.effective_max_tokens,
                again,
                None,
            ),
            None => {
                // 第 3 级：宽限（只在调用方允许时）
                let grace_choice = if input.allow_grace {
                    try_grace(
                        conn,
                        &chain,
                        &views,
                        &report,
                        &full.amount,
                        &period.to_string(),
                    )?
                } else {
                    None
                };
                match grace_choice {
                    Some(grace) => (
                        Tier::Grace,
                        full.amount.clone(),
                        full.effective_max_tokens,
                        report,
                        Some(grace),
                    ),
                    None => return reject(ReasonCode::BudgetExhausted),
                }
            }
        }
    } else {
        let code = report
            .blocking_codes()
            .first()
            .copied()
            .unwrap_or(ReasonCode::BudgetExhausted);
        return reject(code);
    };

    // 上游成本的预留按同一个有效 max_tokens 算（挂账待核销，不占付款账户的额度）。
    let cost = reserve(
        input.policy,
        &ReserveRequest {
            requested_max_tokens: Some(effective),
            input_tokens_estimate: input.sizing.input_tokens_estimate,
        },
        input.cost_rates,
    )?;
    let admitted = Admitted {
        endpoint: input.endpoint.clone(),
        payer: payer.clone(),
        effective_max_tokens: effective,
        reservation: Reservation {
            price: price.clone(),
            cost: cost.amount,
        },
    };
    let decision = match tier {
        Tier::Allow => AdmissionDecision::Allow { admitted },
        Tier::Degrade => AdmissionDecision::Degrade {
            admitted,
            reason: DegradeReason::Budget,
        },
        Tier::Grace => AdmissionDecision::Grace { admitted },
    };
    let expires_at_ms = add(now_ms, config.reservation_ttl_ms)?;
    Ok(AdmitPlan {
        decision,
        write: Some(HoldWrite {
            credit_used: credit_users(&final_report, &chain),
            chain,
            unit: unit.clone(),
            reserved: price.micros(),
            period: period.to_string(),
            expires_at_ms,
            grace: grace.map(|(floor, shortfall)| GraceWrite { floor, shortfall }),
        }),
    })
}

/// 宽限资格。返回 `(放行时可用额度 + 授信, 没被可用额度覆盖的部分)`。
///
/// 只有这些全部成立才给：
/// - 阻断放行的失败**全部**是「钱不够」，并且**全部**在付款账户自己身上（担保人、组织、补贴池那一层不够，
///   不是宽限能解决的：那是别人的钱，或者平台的保护）；
/// - 本账期这个账户还没用过宽限（没作废的）；
/// - 账户没有透支（可用额度 >= 0），缺口不超过一次预留。后两条由 S0 的 [`check_grace`] 判定，
///   **不读 settle 的 overage**（评审 B4）。
fn try_grace(
    conn: &Connection,
    chain: &[Account],
    views: &[idoris_compute_contracts::admission::AncestorView],
    report: &AdmissionReport,
    reserve_price: &Price,
    period: &str,
) -> Result<Option<(i64, i64)>, LedgerError> {
    let (Some(payer_account), Some(payer_view)) = (chain.first(), views.first()) else {
        return Ok(None);
    };
    if !report
        .blocking_failures()
        .iter()
        .all(|failure| failure.account == payer_account.id)
    {
        return Ok(None);
    }
    let Some(available) = available_of(payer_view)? else {
        return Ok(None);
    };
    // 宽限是「额度用完之后的最后一单」：账户必须有过额度或资金。一个什么都没配置过的账户（可用额是失败即关闭
    // 填出来的 0）没有「用完」这回事，给它宽限就是让每个新账户每月白拿一次完整的预留（平台买单）。
    if !is_provisioned(conn, &payer_account.id)? {
        return Ok(None);
    }
    let used = active_grace_uses(conn, &payer_account.id, period)?;
    match check_grace(used, &available, reserve_price) {
        Ok(grant) => {
            let floor = add(available.micros, payer_view.credit_limit.micros)?;
            Ok(Some((floor, grant.shortfall.micros())))
        }
        Err(GraceDenied::Money(error)) => Err(error.into()),
        Err(
            GraceDenied::NotNeeded
            | GraceDenied::AlreadyUsed { .. }
            | GraceDenied::ExceedsOneReservation { .. },
        ) => Ok(None),
    }
}

/// 第二步：写。只有放行才有东西要写。
pub(super) fn apply(
    conn: &Connection,
    input: &AdmitInput<'_>,
    plan: &AdmitPlan,
) -> Result<(), LedgerError> {
    let Some(write) = &plan.write else {
        return Ok(());
    };
    let now_ms = input.now.millis();
    let hold_id = hold_id_of(input.attempt, MODEL_COMPONENT);
    insert_hold_and_reserve(
        conn,
        &write.chain,
        &NewHold {
            attempt: input.attempt,
            component: MODEL_COMPONENT,
            payer: input.payment.payer(),
            credential: input.payment.credential(),
            unit: &write.unit,
            reserved: write.reserved,
            period: &write.period,
            now_ms,
            expires_at_ms: write.expires_at_ms,
            grace_floor: write.grace.as_ref().map(|grace| grace.floor),
        },
    )?;
    let guarantor_of = |index: usize| write.chain.get(index + 1).map(|a| a.id.as_str());
    if let Some(grace) = &write.grace {
        conn.execute(
            "INSERT INTO ledger_grace_uses
                (account_id, period, hold_id, unit, reserve_micros, shortfall_micros, floor_micros, used_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                input.payment.payer().as_str(),
                write.period,
                hold_id,
                write.unit.to_string(),
                write.reserved,
                grace.shortfall,
                grace.floor,
                now_ms,
            ],
        )?;
        insert_alert(
            conn,
            &NewAlert {
                kind: AlertKind::GraceUsed,
                account: input.payment.payer().as_str(),
                guarantor: guarantor_of(0),
                credential: Some(input.payment.credential().as_str()),
                hold: Some(&hold_id),
                amount: grace.shortfall,
                unit: &write.unit,
                detail: "grace_round_granted",
                at_ms: now_ms,
            },
        )?;
    }
    for (index, overdraft) in &write.credit_used {
        let Some(account) = write.chain.get(*index) else {
            continue;
        };
        insert_alert(
            conn,
            &NewAlert {
                kind: AlertKind::CreditUsed,
                account: account.id.as_str(),
                guarantor: guarantor_of(*index),
                credential: Some(input.payment.credential().as_str()),
                hold: Some(&hold_id),
                amount: *overdraft,
                unit: &write.unit,
                detail: "overdraft_within_credit",
                at_ms: now_ms,
            },
        )?;
    }
    Ok(())
}

/// 给附加成本（比如安全检查）用的单项准入：五项检查沿所有祖先，通过就返回要占的链，不降级、不宽限。
pub(super) fn check_extra(
    conn: &Connection,
    payer: &AccountId,
    amount: &Price,
    now_ms: i64,
    config: &super::types::LedgerConfig,
) -> Result<(Vec<Account>, Option<ReasonCode>), LedgerError> {
    let chain = load_ancestors(conn, payer)?;
    if chain.is_empty() {
        return Ok((chain, Some(ReasonCode::PaymentContextInvalid)));
    }
    let views = project_chain(conn, &chain, now_ms, config)?;
    let report = evaluate_admission(amount, &views)?;
    if report.passed() {
        Ok((chain, None))
    } else {
        let code = report
            .blocking_codes()
            .first()
            .copied()
            .unwrap_or(ReasonCode::BudgetExhausted);
        Ok((chain, Some(code)))
    }
}
