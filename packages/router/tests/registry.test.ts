import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import { loadComponents } from "../src/registry.js";

const fixtures = join(dirname(fileURLToPath(import.meta.url)), "fixtures");

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
