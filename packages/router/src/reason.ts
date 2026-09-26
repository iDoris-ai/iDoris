/** 决策 reason 的四类（spec.md 审计记录）。 */
export type ReasonKind = "privacy_enforced" | "budget" | "intent_match" | "degraded";

export const REASON_KINDS: readonly ReasonKind[] = ["privacy_enforced", "budget", "intent_match", "degraded"];

export interface DecisionReason {
  kind: ReasonKind;
  detail: string;
}

export class MissingReasonError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "MissingReasonError";
  }
}

/** 造一条结构化 reason；detail 为空即拒绝（一句 routed 不合格）。 */
export function decisionReason(kind: ReasonKind, detail: string): DecisionReason {
  if (detail.trim() === "") throw new MissingReasonError("a decision reason must be non-empty");
  return { kind, detail };
}

/** 决策记录必须携带非空 reason；否则拒绝。 */
export function requireReason(reason: DecisionReason | undefined): DecisionReason {
  if (reason === undefined || reason.detail.trim() === "") {
    throw new MissingReasonError("every routing decision must carry a non-empty structured reason");
  }
  return reason;
}

/** 回传响应头的形态。 */
export function reasonHeader(reason: DecisionReason): Record<string, string> {
  return { "x-idoris-reason": reason.kind + ": " + reason.detail };
}
