import type { TaskProfile } from "@idoris/contracts";
import { effectiveServedLocality } from "./locality.js";
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

/**
 * 组件卡层面的「可信本地」：loopback + 只承载 local_only + 出站只 none/loopback。
 *
 * **必须用 `effectiveServedLocality`，不能直接读 `card.provider.locality`**
 * （PR #46 复审 H1，真实复现）：后者只是卡自己声明的值，对 `spawn_cli`/订阅类
 * provider 这类"实际推理在别处发生"的卡不可信——直接读它会让这类卡只要声明
 * `locality: loopback` 就能通过 local_only 门禁，实际却把请求转发到了外部。
 */
export function isLocalCapable(r: Registered): boolean {
  return (
    effectiveServedLocality(r.card) === "loopback" &&
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
  // 同样必须用 effectiveServedLocality：否则一张 spawn_cli/订阅类卡声明
  // locality: loopback 时，真实出站会被计数器漏记（跟 isLocalCapable 是同一类问题）。
  if (effectiveServedLocality(chosen.card) !== "loopback") egress.count += 1;
  return { status: 200, body: { provider: chosen.card.provider.id }, providerId: chosen.card.provider.id };
}
