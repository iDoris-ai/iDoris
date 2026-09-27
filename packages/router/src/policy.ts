import { readFileSync } from "node:fs";
import { type RoutingPolicy, type TaskProfile, routingPolicySchema } from "@idoris/contracts";
import { parse } from "yaml";

type Tier = NonNullable<RoutingPolicy["routing_policy"]["default"]["tiers"]>[number];

export interface RouteDecision {
  tiers: Tier[];
  failClosed: boolean;
  capability?: string;
  load?: string;
  matchedRule: number | "default";
}

export function loadRoutingPolicy(path: string): RoutingPolicy {
  const raw = parse(readFileSync(path, "utf8")) as unknown;
  return routingPolicySchema.parse(raw);
}

type Condition = RoutingPolicy["routing_policy"]["rules"][number]["if"];

function matches(cond: Condition, profile: TaskProfile): boolean {
  if (cond.privacy !== undefined && cond.privacy !== profile.privacy) return false;
  if (cond.intent !== undefined && cond.intent !== profile.intent) return false;
  if (cond.complexity !== undefined && cond.complexity !== profile.complexity) return false;
  if (cond.capabilities !== undefined) {
    const need = cond.capabilities as readonly string[];
    const have = profile.capabilities as readonly string[];
    if (!need.every((c) => have.includes(c))) return false;
  }
  return true;
}

/**
 * 路由决策（T1.3.2）。**顺序即语义**：隐私判定在最前 ——
 * 先按 privacy 收窄允许的 tier，再套规则的 tiers。规则说 remote 时，
 * local_only 请求的最终 tier 集合会是空（宁可无候选也不外泄）。
 */
export function decide(policy: RoutingPolicy, profile: TaskProfile): RouteDecision {
  const allowed: readonly Tier[] = profile.privacy === "local_only" ? ["local", "lora"] : ["local", "lora", "remote"];
  const rules = policy.routing_policy.rules;
  let matchedRule: number | "default" = "default";
  let then = policy.routing_policy.default;
  for (let i = 0; i < rules.length; i += 1) {
    const rule = rules[i];
    if (rule && matches(rule.if, profile)) {
      matchedRule = i;
      then = rule.then;
      break;
    }
  }
  const requested = (then.tiers ?? ["local"]) as Tier[];
  const tiers = requested.filter((t) => allowed.includes(t));
  // 不信任 policy：local_only 一律 fail-closed（二次校验）。
  const failClosed = profile.privacy === "local_only" ? true : (then.fail_closed ?? false);
  return {
    tiers,
    failClosed,
    ...(then.capability === undefined ? {} : { capability: then.capability }),
    ...(then.load === undefined ? {} : { load: then.load }),
    matchedRule,
  };
}
