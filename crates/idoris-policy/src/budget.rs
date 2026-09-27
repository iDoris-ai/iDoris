//! 只读预算查询接口。
//!
//! 移植自 `packages/tenancy/src/budget.ts` 的"过滤"语义：预算阶段只**淘汰**
//! 付费候选，真正的原子 reserve/charge 属于另一个 crate（本 crate 不做 IO，
//! 也就不可能原子地改任何东西）。

use idoris_contracts::tenant::BudgetScope;

/// 某个租户当前的预算快照，供决策管道只读查询。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetSnapshot {
    pub limit_minor: i64,
    pub spent_minor: i64,
    pub scope: BudgetScope,
}

impl BudgetSnapshot {
    /// 对齐 `budget.ts` 的 `over` 判定：`spent_minor >= limit_minor`。
    pub fn is_over(&self) -> bool {
        self.spent_minor >= self.limit_minor
    }

    /// 剩余额度；超支时为 `<= 0`。
    pub fn balance_minor(&self) -> i64 {
        self.limit_minor - self.spent_minor
    }
}

/// 只读预算查询接口。真正的原子 reserve/charge 由另一个 crate 负责。
pub trait BudgetView {
    /// 返回 `tenant_id` 当前的预算快照；`None` 表示没有租户预算上下文（例如
    /// personal 部署），此时预算阶段直接放行，不做任何闸门（对齐 `budget.ts`
    /// 里 `ctx === undefined` 的分支）。
    fn snapshot(&self, tenant_id: &str) -> Option<BudgetSnapshot>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_over_matches_spent_greater_or_equal_limit() {
        let under = BudgetSnapshot {
            limit_minor: 1000,
            spent_minor: 999,
            scope: BudgetScope::PaidOnly,
        };
        assert!(!under.is_over());
        let at_limit = BudgetSnapshot {
            limit_minor: 1000,
            spent_minor: 1000,
            scope: BudgetScope::PaidOnly,
        };
        assert!(at_limit.is_over());
        let over = BudgetSnapshot {
            limit_minor: 1000,
            spent_minor: 1001,
            scope: BudgetScope::PaidOnly,
        };
        assert!(over.is_over());
    }

    #[test]
    fn balance_minor_can_go_negative_when_over_budget() {
        let over = BudgetSnapshot {
            limit_minor: 1000,
            spent_minor: 1200,
            scope: BudgetScope::All,
        };
        assert_eq!(over.balance_minor(), -200);
    }
}
