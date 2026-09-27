import type { TenantContext } from "@idoris/contracts";

/** 假「计费调用」计数器：超预算被拒时它必须是 0。 */
export interface BillingCounter {
  count: number;
}

export interface PricedCandidate {
  id: string;
  costMinor: number;
}

export interface BudgetDecision {
  status: 200 | 402;
  body: Record<string, unknown>;
  /** 通过预算闸门的候选（超预算 + all 时为空）。 */
  allowed: string[];
}

/**
 * 预算闸门（T1.5.2）——**终态拒绝，不是降级**。
 * - 位置：隐私判定之后、意图匹配之前（由调用方按此顺序调用）；
 * - `scope=paid_only`（默认）：只闸 `cost>0` 的候选，本地零成本模型不受影响；
 * - `scope=all`：超预算一律拒绝；
 * - 402 错误体明确带 `budget_exceeded`，与 5xx 故障区分。
 */
export function checkBudget(ctx: TenantContext | undefined, candidates: PricedCandidate[]): BudgetDecision {
  if (ctx === undefined) {
    return { status: 200, body: {}, allowed: candidates.map((c) => c.id) };
  }
  const over = ctx.budget.spent_minor >= ctx.budget.limit_minor;
  if (!over) {
    return { status: 200, body: {}, allowed: candidates.map((c) => c.id) };
  }
  const rejected = (): BudgetDecision => ({
    status: 402,
    body: {
      error: {
        type: "budget_exceeded",
        message: "tenant budget exhausted: this is a billing decision, not a provider failure",
        scope: ctx.budget.scope,
      },
    },
    allowed: [],
  });

  if (ctx.budget.scope === "all") return rejected();
  const allowed = candidates.filter((c) => c.costMinor === 0).map((c) => c.id);
  if (allowed.length === 0) return rejected();
  return { status: 200, body: {}, allowed };
}

/** 计费调用时调用方自增；被闸门拒绝的路径不该走到这里。 */
export function charge(billing: BillingCounter): void {
  billing.count += 1;
}
