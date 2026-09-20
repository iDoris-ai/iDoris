import { componentCardSchema } from "./component-card.js";
import type { ComponentCard } from "./component-card.js";

/** 组件卡必须存在的强制策略字段（06 §10.2）。缺一即不注册。 */
export const MANDATORY_POLICY_FIELDS = [
  "privacy_class",
  "allowed_egress",
  "fallback_policy",
  "fail_closed",
  "version_pin",
] as const;

export type PolicyErrorCode =
  | "MISSING_POLICY_FIELD"
  | "INVALID_SCHEMA"
  | "LOCAL_ONLY_REQUIRES_FAIL_CLOSED"
  | "PRIVACY_CLASS_MISMATCH"
  | "LOCAL_TIER_CANNOT_BE_REMOTE_LOCALITY"
  | "LOCAL_TIER_REQUIRES_LOAD_POLICY"
  | "LOAD_POLICY_MODE_KEEPALIVE_MISMATCH"
  | "EGRESS_NONE_MUST_BE_EXCLUSIVE"
  | "LOCAL_ONLY_CANNOT_ALLOW_INTERNET"
  | "LOCAL_ONLY_EGRESS_MUST_BE_LOOPBACK";

/** 组件卡策略校验失败。带类型（code + path），调用方按 code 判定。 */
export class ComponentCardPolicyError extends Error {
  constructor(
    readonly code: PolicyErrorCode,
    readonly path: string,
    message: string,
  ) {
    super(message);
    this.name = "ComponentCardPolicyError";
  }
}

/**
 * 组件卡策略校验器（T1.1.3）。
 *
 * 「协议兼容 ≠ 路由安全」：结构 schema 过 ≠ 可注册。除结构校验外强制：
 * - 强制策略字段存在；
 * - privacy_class=local_only ⇒ fail_closed=true；
 * - 卡与 provider 的 privacy_class 一致（provider=local_only ⇒ 卡也 local_only）；
 * - provider.tier=local 且 provider.locality=remote 非法；
 * - provider.tier=local（能力③）必须有 load_policy；
 * - LoadPolicy mode 与 keepalive 自洽（resident ⇒ pinned=true，其余 ⇒ 不得 pinned）；
 * - allowed_egress 含 none 时必须独占；
 * - privacy_class=local_only 时出站只能是 none/loopback（含 internet 或 lan 即拒）。
 */
export function validateComponentCard(input: unknown): ComponentCard {
  if (typeof input !== "object" || input === null || Array.isArray(input)) {
    throw new ComponentCardPolicyError("INVALID_SCHEMA", "", "component card must be an object");
  }
  const record = input as Record<string, unknown>;
  for (const field of MANDATORY_POLICY_FIELDS) {
    if (!(field in record) || record[field] === undefined) {
      throw new ComponentCardPolicyError("MISSING_POLICY_FIELD", field, "missing mandatory policy field: " + field);
    }
  }
  const parsed = componentCardSchema.safeParse(input);
  if (!parsed.success) {
    throw new ComponentCardPolicyError(
      "INVALID_SCHEMA",
      parsed.error.issues.map((i) => i.path.join(".")).join(","),
      "component card schema validation failed",
    );
  }
  const card: ComponentCard = parsed.data;

  if (card.privacy_class === "local_only" && card.fail_closed !== true) {
    throw new ComponentCardPolicyError("LOCAL_ONLY_REQUIRES_FAIL_CLOSED", "fail_closed", "privacy_class=local_only requires fail_closed=true");
  }
  if (card.provider.privacy_class === "local_only" && card.privacy_class !== "local_only") {
    throw new ComponentCardPolicyError("PRIVACY_CLASS_MISMATCH", "privacy_class", "provider.privacy_class=local_only forbids a broader card privacy_class");
  }
  if (card.provider.tier === "local" && card.provider.locality === "remote") {
    throw new ComponentCardPolicyError("LOCAL_TIER_CANNOT_BE_REMOTE_LOCALITY", "provider.locality", "provider.tier=local cannot have provider.locality=remote");
  }
  if (card.provider.tier === "local" && card.load_policy === undefined) {
    throw new ComponentCardPolicyError("LOCAL_TIER_REQUIRES_LOAD_POLICY", "load_policy", "a tier=local (capability 3) component must declare load_policy");
  }
  if (card.load_policy) {
    const pinned = (card.load_policy.keepalive as { pinned?: boolean }).pinned;
    const resident = card.load_policy.mode === "resident";
    if (resident !== (pinned === true)) {
      throw new ComponentCardPolicyError("LOAD_POLICY_MODE_KEEPALIVE_MISMATCH", "load_policy.keepalive", "mode=resident requires pinned=true; other modes must not pin");
    }
  }
  if (card.allowed_egress.includes("none") && card.allowed_egress.length !== 1) {
    throw new ComponentCardPolicyError("EGRESS_NONE_MUST_BE_EXCLUSIVE", "allowed_egress", "none means no egress and must be the only entry");
  }
  if (card.privacy_class === "local_only") {
    if (card.allowed_egress.includes("internet")) {
      throw new ComponentCardPolicyError("LOCAL_ONLY_CANNOT_ALLOW_INTERNET", "allowed_egress", "privacy_class=local_only cannot allow internet egress");
    }
    if (card.allowed_egress.some((e) => e !== "none" && e !== "loopback")) {
      throw new ComponentCardPolicyError("LOCAL_ONLY_EGRESS_MUST_BE_LOOPBACK", "allowed_egress", "privacy_class=local_only may only egress to none/loopback");
    }
  }
  return card;
}
