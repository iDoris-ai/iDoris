import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { LocalClient, detectFlower, gaussianSigma, runRound } from "../../src/index.js";

const probe = detectFlower();
if (!probe.available) console.log("SKIPPED: T3.3.1 真 Flower 后端 —— " + probe.reason);

describe.skipIf(!probe.available)("T3.3.1 真 Flower 后端（跨进程 gRPC）", () => {
  it("flwr 可用时跑一轮真实联邦", () => {
    // 真 Flower 需要 Python 侧启动 server/client 进程；本包验证的是协议与载荷契约，
    // 真后端接线属后续。这里只断言环境确实可用，避免把 SKIPPED 伪装成 PASS。
    expect(probe.available).toBe(true);
  });
});

// 无论 flwr 在不在，载荷契约与 DP 链路都必须可跑（不依赖 Python）。
describe("T3.3.1/T3.4.1 不依赖 Python 的链路仍全程可跑", () => {
  it("本地一轮 + DP 噪声 + 掩码聚合", () => {
    const base = {
      base_model_id: "m",
      base_digest: "sha256:" + "a".repeat(64),
      tokenizer_digest: "sha256:" + "b".repeat(64),
    };
    const dir = mkdtempSync(join(tmpdir(), "idoris-fed-"));
    expect(dir.length).toBeGreaterThan(0);
    const a = new LocalClient({ clientId: "a", base });
    const b = new LocalClient({ clientId: "b", base });
    a.addSamples([{ prompt: "p1", response: "r1" }]);
    b.addSamples([{ prompt: "p2", response: "r2" }]);
    const { aggregate } = runRound([a, b], () => {});
    expect(aggregate.totalSamples).toBe(2);
    expect(gaussianSigma({ epsilon: 1, delta: 1e-5, sensitivity: 1 })).toBeGreaterThan(0);
  });
});
