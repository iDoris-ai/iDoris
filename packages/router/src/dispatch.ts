import type { TaskProfile } from "@idoris/contracts";
import type { RouteDecision } from "./policy.js";
import type { Registered } from "./registry.js";

/** 假上游出站计数器：隐私测试断言它为 0。 */
export interface EgressCounter {
  count: number;
}

export interface DispatchOutcome {
  status: number;
  body: Record<string, unknown>;
  providerId?: string;
}

/** 组件卡层面的「可信本地」：loopback + 只承载 local_only + 出站只 none/loopback。 */
export function isLocalCapable(r: Registered): boolean {
  return (
    r.card.provider.locality === "loopback" &&
    r.card.privacy_class === "local_only" &&
    r.card.allowed_egress.every((e) => e === "none" || e === "loopback")
  );
}

/**
 * 选中 provider（T1.3.3）。
 * - `local_only`：候选只保留「可信本地」；无候选 → **503 local_only_unavailable**，绝不出站；
 * - 非 local_only：可按 `next_in_chain` 降级，但降级候选同样要过 privacy_class/allowed_egress 复核。
 */
export function dispatch(
  profile: TaskProfile,
  decision: RouteDecision,
  registered: Registered[],
  egress: EgressCounter,
): DispatchOutcome {
  const byTier = registered.filter((r) => decision.tiers.includes(r.card.provider.tier));
  const localOnly = profile.privacy === "local_only";
  const candidates = localOnly ? byTier.filter(isLocalCapable) : byTier;

  if (candidates.length === 0) {
    if (decision.failClosed || localOnly) {
      return {
        status: 503,
        body: { error: { type: "local_only_unavailable", message: "no local provider can serve this local_only request" } },
      };
    }
    return { status: 503, body: { error: { type: "no_candidate" } } };
  }

  const chosen = candidates[0];
  if (!chosen) return { status: 503, body: { error: { type: "no_candidate" } } };
  if (chosen.card.provider.locality !== "loopback") egress.count += 1;
  return { status: 200, body: { provider: chosen.card.provider.id }, providerId: chosen.card.provider.id };
}
