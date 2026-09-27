import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import { loadComponents } from "../src/registry.js";

const fixtures = join(dirname(fileURLToPath(import.meta.url)), "fixtures");
const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");

describe("loadComponents", () => {
  it("loads and validates a good component card", () => {
    const reg = loadComponents(join(fixtures, "good"));
    expect(reg).toHaveLength(1);
    expect(reg[0]?.card.provider.id).toBe("mock");
  });
  it("throws (startup fails) on a card missing a mandatory policy field", () => {
    expect(() => loadComponents(join(fixtures, "bad"))).toThrow();
  });
});

function writeCard(dir: string, file: string, yaml: string): void {
  writeFileSync(join(dir, file), yaml, "utf8");
}

const HTTP_CARD = (id: string, endpoint: string, locality: string): string =>
  [
    "provider:",
    `  id: ${id}`,
    "  family: local",
    "  tier: local",
    "  capabilities: [chat]",
    "  privacy_class: local_only",
    "  cost: { input_per_m: 0, output_per_m: 0 }",
    `  locality: ${locality}`,
    "form: http_service",
    `endpoint: "${endpoint}"`,
    `version_pin: "${id}@1"`,
    "privacy_class: local_only",
    "allowed_egress: [loopback]",
    "fallback_policy: fail_closed",
    "fail_closed: true",
    "load_policy: { mode: resident, keepalive: { pinned: true }, admission: coexist }",
    "",
  ].join("\n");

/** M6：http_service 卡的 endpoint host 必须和 provider.locality: loopback 互相印证。 */
describe("loadComponents：endpoint host 与 locality 一致性校验（M6）", () => {
  let dir: string;
  afterEach(() => {
    if (dir) rmSync(dir, { recursive: true, force: true });
  });

  // 用 "mock"/"omlx" 而不是随便起的 id：createBackend 的 provider→引擎映射
  // 只认这两个名字（见 @idoris/adapters factory.ts），用别的 id 会在
  // "M6 校验通过之后"的 createBackend 那一步炸「no adapter for provider」，
  // 跟本测试要验的东西无关，会把「M6 拦下了」和「adapter 不认识这个 id」混在一起。

  it("locality: loopback + endpoint host 是 127.0.0.1 → 正常注册（正对照）", () => {
    dir = mkdtempSync(join(tmpdir(), "idoris-registry-m6-"));
    writeCard(dir, "ok.yaml", HTTP_CARD("mock", "http://127.0.0.1:9999", "loopback"));
    expect(() => loadComponents(dir)).not.toThrow();
  });

  it("负对照：locality: loopback 但 endpoint host 是外部域名 → 拒绝注册（fail-closed）", () => {
    dir = mkdtempSync(join(tmpdir(), "idoris-registry-m6-"));
    writeCard(dir, "bad.yaml", HTTP_CARD("mock", "http://example.com:9999", "loopback"));
    expect(() => loadComponents(dir)).toThrow(/provider\.locality: loopback.*不是 127\.0\.0\.1/s);
  });

  it("mock:// 这类非真实网络 scheme 不受这条校验约束（fixtures/good 本身就是这样，不能被误伤）", () => {
    dir = mkdtempSync(join(tmpdir(), "idoris-registry-m6-"));
    writeCard(dir, "mock.yaml", HTTP_CARD("mock", "mock://in-memory", "loopback"));
    expect(() => loadComponents(dir)).not.toThrow();
  });
});

/** L5：同一个 provider.id 出现两次 → 拒绝启动（谁生效是未定义行为，不如直接报错）。 */
describe("loadComponents：provider.id 重复声明拒绝启动（L5）", () => {
  let dir: string;
  afterEach(() => {
    if (dir) rmSync(dir, { recursive: true, force: true });
  });

  it("两张卡用了同一个 provider.id → 抛错（错误信息明确说是重复声明，不是别的原因）", () => {
    dir = mkdtempSync(join(tmpdir(), "idoris-registry-l5-"));
    writeCard(dir, "a.yaml", HTTP_CARD("mock", "http://127.0.0.1:9001", "loopback"));
    writeCard(dir, "b.yaml", HTTP_CARD("mock", "http://127.0.0.1:9002", "loopback"));
    expect(() => loadComponents(dir)).toThrow(/重复声明/);
  });

  it("负对照：id 不同的两张卡（都是认识的 provider）正常注册", () => {
    dir = mkdtempSync(join(tmpdir(), "idoris-registry-l5-"));
    writeCard(dir, "a.yaml", HTTP_CARD("mock", "http://127.0.0.1:9001", "loopback"));
    writeCard(dir, "b.yaml", HTTP_CARD("omlx", "http://127.0.0.1:9002", "loopback"));
    const reg = loadComponents(dir);
    expect(reg.map((r) => r.card.provider.id).sort()).toEqual(["mock", "omlx"]);
  });
});

const SPAWN_CLI_CARD = (id: string, opts: { tier: string; privacyClass: string; locality: string }): string =>
  [
    "provider:",
    `  id: ${id}`,
    "  family: other",
    `  tier: ${opts.tier}`,
    "  capabilities: [chat]",
    `  privacy_class: ${opts.privacyClass}`,
    "  cost: { input_per_m: 0, output_per_m: 0 }",
    `  locality: ${opts.locality}`,
    "form: spawn_cli",
    'endpoint: "spawn://fake"',
    `version_pin: "${id}@1"`,
    `privacy_class: ${opts.privacyClass}`,
    "allowed_egress: [loopback]",
    "fallback_policy: fail_closed",
    "fail_closed: true",
    ...(opts.tier === "local" ? ["load_policy: { mode: resident, keepalive: { pinned: true }, admission: coexist }"] : []),
    "",
  ].join("\n");

/**
 * H1（PR #46 复审，真实复现）：spawn_cli / 订阅类 provider 不允许声明
 * `privacy_class: local_only` 或 `tier: local`——这类卡的推理实际发生在它
 * 背后的外部 CLI/服务里，声明本地语义会让 local_only 门禁把它当"可信本地"放行。
 */
describe("loadComponents：spawn_cli/订阅类 provider 不允许声明本地语义（H1）", () => {
  let dir: string;
  afterEach(() => {
    if (dir) rmSync(dir, { recursive: true, force: true });
  });

  it("provider.id=subscription 的 http_service 卡（tier: local + privacy_class: local_only）→ 拒绝注册", () => {
    // 就是 HTTP_CARD 的默认形状（tier: local、privacy_class: local_only），
    // 只是 provider.id 用 "subscription"——isSubscriptionProviderId 只认 id、
    // 不认 form，这正是复审指出的真实触发路径：不需要 form 是 spawn_cli。
    dir = mkdtempSync(join(tmpdir(), "idoris-registry-h1-"));
    writeCard(dir, "subscription.yaml", HTTP_CARD("subscription", "http://127.0.0.1:9999", "loopback"));
    expect(() => loadComponents(dir)).toThrow(/语义矛盾/);
  });

  it("form: spawn_cli 的卡声明 tier: local → 拒绝注册（即便 id 不是订阅类）", () => {
    dir = mkdtempSync(join(tmpdir(), "idoris-registry-h1-"));
    writeCard(dir, "relay.yaml", SPAWN_CLI_CARD("some-relay", { tier: "local", privacyClass: "any", locality: "loopback" }));
    expect(() => loadComponents(dir)).toThrow(/语义矛盾/);
  });

  it("form: spawn_cli 的卡声明 privacy_class: local_only（tier 是 remote）→ 依然拒绝", () => {
    dir = mkdtempSync(join(tmpdir(), "idoris-registry-h1-"));
    writeCard(
      dir,
      "relay.yaml",
      SPAWN_CLI_CARD("some-relay", { tier: "remote", privacyClass: "local_only", locality: "loopback" }),
    );
    expect(() => loadComponents(dir)).toThrow(/语义矛盾/);
  });

  it("负对照：真实 config/components/subscription.yaml（tier: remote, privacy_class: any）不会被这条新校验拦下", () => {
    // 用仓库里真实的那张卡，防止"校验太严，把合法配置也拦了"。
    const real = readFileSync(join(repoRoot, "config", "components", "subscription.yaml"), "utf8");
    dir = mkdtempSync(join(tmpdir(), "idoris-registry-h1-"));
    writeCard(dir, "subscription.yaml", real);
    try {
      // 走到这里可能因为 subscriptionStartupGate（非 personal/未 enable）而
      // 跳过注册或报 SUBSCRIPTION_FORBIDDEN，那些都不是本测试要断言的东西；
      // 只要不是"语义矛盾"这条新校验报错就行。
      loadComponents(dir, { env: { IDORIS_DEPLOY_MODE: "personal", IDORIS_DISABLE_SUBSCRIPTION: "1" } });
    } catch (err) {
      expect(String(err)).not.toMatch(/语义矛盾/);
    }
  });

  it("负对照：普通本地卡（tier: local + privacy_class: local_only，非 spawn_cli/非订阅 id）正常注册", () => {
    dir = mkdtempSync(join(tmpdir(), "idoris-registry-h1-"));
    writeCard(dir, "mock.yaml", HTTP_CARD("mock", "http://127.0.0.1:9999", "loopback"));
    expect(() => loadComponents(dir)).not.toThrow();
  });
});
