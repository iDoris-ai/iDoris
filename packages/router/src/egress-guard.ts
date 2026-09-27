/**
 * T1.4.2 loopback + 单用户门禁（合规红线落地）。
 *
 * 两条独立约束：
 *   1) **来源**：订阅 provider 只接受 loopback 来源的请求（可选显式开启的
 *      Tailscale 私网白名单）。非 loopback 一律拒。
 *   2) **部署模式**：deploy_mode != personal（tenant / community / city）时，
 *      订阅 provider 拒绝注册 → 启动抛错 → 进程退出非 0。
 *
 * 多租户不是放开这条红线的理由：组织租户用组织自己的 API（能力②）或本地模型（能力③）。
 */
import {
  NON_PERSONAL_DEPLOY_MODES,
  deployModeFromEnv,
  isPersonalDeployMode,
  isSubscriptionProviderId,
  type NonPersonalDeployMode,
} from "@idoris/adapters";

export type EgressGuardErrorCode =
  | "SUBSCRIPTION_SOURCE_NOT_LOOPBACK"
  | "SUBSCRIPTION_FORBIDDEN_BY_DEPLOY_MODE";

export class EgressGuardError extends Error {
  constructor(
    readonly code: EgressGuardErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "EgressGuardError";
  }
}

/** 是否是 loopback 地址（含 IPv4-mapped IPv6 形式）。 */
export function isLoopbackAddress(address: string | undefined): boolean {
  if (address === undefined || address === "") return false;
  const a = address.startsWith("::ffff:") ? address.slice("::ffff:".length) : address;
  if (a === "::1" || a === "localhost") return true;
  const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(a);
  if (m === null) return false;
  return Number(m[1]) === 127;
}

/** Tailscale 私网使用 100.64.0.0/10（CGNAT）。 */
export function isTailscaleAddress(address: string | undefined): boolean {
  if (address === undefined || address === "") return false;
  const a = address.startsWith("::ffff:") ? address.slice("::ffff:".length) : address;
  const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(a);
  if (m === null) return false;
  return Number(m[1]) === 100 && Number(m[2]) >= 64 && Number(m[2]) <= 127;
}

export function isNonPersonalDeployMode(mode: string): mode is NonPersonalDeployMode {
  return (NON_PERSONAL_DEPLOY_MODES as readonly string[]).includes(mode);
}

/**
 * 订阅来源是否允许。默认只允许 loopback；只有显式
 * `IDORIS_SUBSCRIPTION_ALLOW_TAILSCALE=1` 才额外允许 Tailscale 私网。
 */
export function isAllowedSubscriptionSource(
  address: string | undefined,
  env: NodeJS.ProcessEnv = process.env,
): boolean {
  if (isLoopbackAddress(address)) return true;
  if (env.IDORIS_SUBSCRIPTION_ALLOW_TAILSCALE === "1" && isTailscaleAddress(address)) return true;
  return false;
}

/** dispatch 前的来源复核；非 loopback 抛 EgressGuardError。 */
export function assertSubscriptionSource(address: string | undefined, env: NodeJS.ProcessEnv = process.env): void {
  if (!isAllowedSubscriptionSource(address, env)) {
    throw new EgressGuardError(
      "SUBSCRIPTION_SOURCE_NOT_LOOPBACK",
      "the subscription provider only accepts loopback sources (or an explicit Tailscale allowlist); got " +
        (address ?? "<unknown>"),
    );
  }
}

export type SubscriptionStartupAction = "register" | "skip" | "refuse";

export interface SubscriptionStartupDecision {
  action: SubscriptionStartupAction;
  reason: string;
}

/**
 * 启动期注册门禁（registry 调用）。
 * 非 personal 部署**优先**硬拒（即使显式 disable，也不允许在租户/社区/城市端存在订阅卡）。
 */
export function subscriptionStartupGate(
  providerId: string,
  env: NodeJS.ProcessEnv = process.env,
): SubscriptionStartupDecision {
  if (!isSubscriptionProviderId(providerId)) {
    return { action: "register", reason: "not a subscription provider" };
  }
  const mode = deployModeFromEnv(env);
  if (!isPersonalDeployMode(env)) {
    return {
      action: "refuse",
      reason:
        "deploy_mode=" +
        mode +
        " forbids registering the subscription provider (personal only); use your own API (capability 2) or local models (capability 3)",
    };
  }
  if (env.IDORIS_DISABLE_SUBSCRIPTION === "1") {
    return { action: "skip", reason: "IDORIS_DISABLE_SUBSCRIPTION=1" };
  }
  if (env.IDORIS_ENABLE_SUBSCRIPTION !== "1") {
    return {
      action: "skip",
      reason: "subscription relay is fail-closed by default; set IDORIS_ENABLE_SUBSCRIPTION=1 and IDORIS_SUBSCRIPTION_SANDBOX",
    };
  }
  return { action: "register", reason: "personal deploy mode + explicit enable" };
}
