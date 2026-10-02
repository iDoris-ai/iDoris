/**
 * 生成组件卡目录（config/components/*.yaml 同格式）。
 *
 * 只允许两种 provider.id：`mock` / `omlx` —— 这是被测 TS 实现
 * `packages/adapters/src/factory.ts` 目前唯一认识的两个 id（其余 id 会在
 * 启动时抛 "no adapter for provider"）。id 本身不代表本地/远程语义，
 * tier/locality/privacy_class/allowed_egress 才是；所以同一个 id 在不同
 * fixture 里可以配成本地也可以配成远程，两者互不影响。
 *
 * endpoint 一律指向测试起的假上游（node:http），这样组件卡的 `form:
 * http_service` 才能真正打一次 HTTP 请求，而不是像仓库里某些单测那样用
 * `mock://` 这种假 scheme（那种只在注入了自定义 fetch 的单测里能用）。
 */
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

export type ComponentTier = "local" | "remote" | "lora";
export type ComponentLocality = "loopback" | "lan" | "remote";
export type PrivacyClass = "local_only" | "any";
export type EgressKind = "none" | "loopback" | "lan" | "internet";

export interface ComponentCardSpec {
  id: "mock" | "omlx";
  tier: ComponentTier;
  locality: ComponentLocality;
  privacyClass: PrivacyClass;
  allowedEgress: EgressKind[];
  endpoint: string;
  capabilities?: string[];
  fallbackPolicy?: "fail_closed" | "next_in_chain";
  failClosed?: boolean;
  /** 不传保留 resident 默认值；null 省略字段；对象原样输出以构造策略正负例。 */
  loadPolicy?: Record<string, unknown> | null;
  extensions?: Record<string, unknown>;
  providerExtensions?: Record<string, unknown>;
}

function renderCard(c: ComponentCardSpec): string {
  const caps = c.capabilities ?? ["chat"];
  const family = c.tier === "remote" ? "other" : "local";
  return [
    "provider:",
    "  id: " + c.id,
    "  family: " + family,
    "  tier: " + c.tier,
    "  capabilities: [" + caps.join(", ") + "]",
    "  privacy_class: " + c.privacyClass,
    "  cost: { input_per_m: 0, output_per_m: 0 }",
    "  locality: " + c.locality,
    ...(c.providerExtensions === undefined ? [] : ["  extensions: " + JSON.stringify(c.providerExtensions)]),
    "form: http_service",
    "endpoint: " + JSON.stringify(c.endpoint),
    'version_pin: "' + c.id + '@conformance-1"',
    "privacy_class: " + c.privacyClass,
    "allowed_egress: [" + c.allowedEgress.join(", ") + "]",
    "fallback_policy: " + (c.fallbackPolicy ?? "fail_closed"),
    "fail_closed: " + String(c.failClosed ?? true),
    ...(c.loadPolicy === null ? [] : [c.loadPolicy === undefined
      ? "load_policy: { mode: resident, keepalive: { pinned: true }, admission: coexist }"
      : "load_policy: " + JSON.stringify(c.loadPolicy)]),
    ...(c.extensions === undefined ? [] : ["extensions: " + JSON.stringify(c.extensions)]),
    "",
  ].join("\n");
}

/** 每次调用生成一个全新的临时目录（不同测试互不干扰，进程结束后由 OS temp 清理）。 */
export function makeComponentsDir(cards: ComponentCardSpec[]): string {
  const dir = mkdtempSync(join(tmpdir(), "idoris-conformance-"));
  cards.forEach((c, i) => {
    writeFileSync(join(dir, String(i) + "-" + c.id + ".yaml"), renderCard(c), "utf8");
  });
  return dir;
}

/** 原样写入 YAML（包括故意无效的策略），由被测实现负责校验。 */
export function makeRoutingPolicyFile(content: string): string {
  const dir = mkdtempSync(join(tmpdir(), "idoris-conformance-policy-"));
  const path = join(dir, "routing-policy.yaml");
  writeFileSync(path, content, "utf8");
  return path;
}

/** 便利构造：单个 loopback + local_only 的可信本地组件，指向给定假上游 URL。 */
export function localComponent(endpoint: string, extra: Partial<ComponentCardSpec> = {}): ComponentCardSpec {
  return {
    id: "omlx",
    tier: "local",
    locality: "loopback",
    privacyClass: "local_only",
    allowedEgress: ["loopback"],
    endpoint,
    ...extra,
  };
}

/** 便利构造：单个 remote + any 的组件，指向给定假上游 URL。 */
export function remoteComponent(endpoint: string, extra: Partial<ComponentCardSpec> = {}): ComponentCardSpec {
  return {
    id: "omlx",
    tier: "remote",
    locality: "remote",
    privacyClass: "any",
    allowedEgress: ["internet"],
    fallbackPolicy: "next_in_chain",
    failClosed: false,
    endpoint,
    ...extra,
  };
}
