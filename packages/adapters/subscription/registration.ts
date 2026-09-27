/**
 * T1.4.1/T1.4.2 订阅 provider 的**注册门禁**（fail-closed）。
 *
 * 注册订阅 relay 需要同时满足：
 *   1) deploy_mode 为 personal（tenant / community / city 一律拒绝注册并让启动非 0）；
 *   2) IDORIS_ENABLE_SUBSCRIPTION=1（默认关闭，best-effort）；
 *   3) 显式声明沙箱档（resolveSubscriptionSandbox）。
 *
 * 这里只放**纯决策**（不 spawn 任何东西），便于 registry / factory / 测试共用同一份语义。
 */
import type { ComponentCard } from "@idoris/contracts";
import { resolveSubscriptionSandbox, SandboxProfileError, type SandboxProfile } from "./sandbox.js";
import { SubscriptionRelay } from "./relay.js";

export const SUBSCRIPTION_PROVIDER_ID = "subscription";

/** 非 personal 的部署模式：多租户/社区/城市端绝不转发个人订阅。 */
export const NON_PERSONAL_DEPLOY_MODES = ["tenant", "community", "city"] as const;
export type NonPersonalDeployMode = (typeof NON_PERSONAL_DEPLOY_MODES)[number];

export function isSubscriptionProviderId(providerId: string): boolean {
  return providerId === SUBSCRIPTION_PROVIDER_ID;
}

/** 读 IDORIS_DEPLOY_MODE；空/未设 = personal（本机单用户默认）。 */
export function deployModeFromEnv(env: NodeJS.ProcessEnv = process.env): string {
  const raw = (env.IDORIS_DEPLOY_MODE ?? "").trim().toLowerCase();
  return raw === "" ? "personal" : raw;
}

export function isPersonalDeployMode(env: NodeJS.ProcessEnv = process.env): boolean {
  return deployModeFromEnv(env) === "personal";
}

export type RegistrationAction = "register" | "skip" | "refuse";

export interface RegistrationDecision {
  action: RegistrationAction;
  reason: string;
  sandbox?: SandboxProfile;
}

/**
 * 订阅 provider 的注册决策。
 * - 非 personal 部署 → **refuse**（硬拒，启动非 0）；
 * - IDORIS_DISABLE_SUBSCRIPTION=1 → skip（显式关闭，用于证明核心不依赖能力①）；
 * - 未显式 enable → skip（默认关闭，不是失败）；
 * - enable 但沙箱档缺失/未知 → **refuse**（明确要求了却没沙箱，绝不无沙箱硬跑）。
 */
export function decideSubscriptionRegistration(env: NodeJS.ProcessEnv = process.env): RegistrationDecision {
  const mode = deployModeFromEnv(env);
  if (mode !== "personal") {
    return {
      action: "refuse",
      reason: "deploy_mode=" + mode + " forbids the subscription provider (personal only); use your own API (capability 2) or local models (capability 3)",
    };
  }
  if (env.IDORIS_DISABLE_SUBSCRIPTION === "1") {
    return { action: "skip", reason: "IDORIS_DISABLE_SUBSCRIPTION=1 (subscription disabled explicitly)" };
  }
  if (env.IDORIS_ENABLE_SUBSCRIPTION !== "1") {
    return {
      action: "skip",
      reason: "subscription relay is fail-closed by default; set IDORIS_ENABLE_SUBSCRIPTION=1 and IDORIS_SUBSCRIPTION_SANDBOX",
    };
  }
  try {
    const sandbox = resolveSubscriptionSandbox(env);
    return { action: "register", reason: "personal deploy mode + explicit enable + sandbox profile " + sandbox.id, sandbox };
  } catch (err) {
    if (err instanceof SandboxProfileError) {
      return { action: "refuse", reason: err.message };
    }
    throw err;
  }
}

export interface SubscriptionRegistrationOptions {
  env?: NodeJS.ProcessEnv;
  command?: string;
  timeoutMs?: number;
  maxOutputBytes?: number;
  cwd?: string;
}

/**
 * 构造订阅 relay。**唯一入口**：没有显式沙箱档一律抛错，调用方无法绕过。
 * registry 在调用本函数前已按 deploy_mode/enable 做过 skip/refuse 决策；
 * 这里再复核一次，作为 factory 被直接调用时的兜底。
 */
export function createSubscriptionBackend(
  _card: ComponentCard,
  opts: SubscriptionRegistrationOptions = {},
): SubscriptionRelay {
  const env = opts.env ?? process.env;
  const decision = decideSubscriptionRegistration(env);
  if (decision.action !== "register" || decision.sandbox === undefined) {
    throw new SubscriptionRegistrationError(
      decision.action === "refuse" ? "SUBSCRIPTION_REFUSED" : "SUBSCRIPTION_NOT_ENABLED",
      decision.reason,
    );
  }
  return new SubscriptionRelay({
    sandbox: decision.sandbox,
    env,
    ...(opts.command === undefined ? {} : { command: opts.command }),
    ...(opts.timeoutMs === undefined ? {} : { timeoutMs: opts.timeoutMs }),
    ...(opts.maxOutputBytes === undefined ? {} : { maxOutputBytes: opts.maxOutputBytes }),
    ...(opts.cwd === undefined ? {} : { cwd: opts.cwd }),
  });
}

export class SubscriptionRegistrationError extends Error {
  constructor(
    readonly code: "SUBSCRIPTION_REFUSED" | "SUBSCRIPTION_NOT_ENABLED",
    message: string,
  ) {
    super(message);
    this.name = "SubscriptionRegistrationError";
  }
}
