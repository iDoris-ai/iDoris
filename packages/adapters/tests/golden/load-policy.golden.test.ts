import { describe, expect, it } from "vitest";
import type { ModelBackend } from "../../src/backend.js";
import { MockBackend } from "../../mock/mock-backend.js";
import { OmlxBackend } from "../../omlx/omlx-backend.js";

/**
 * LoadPolicy 黄金一致性（T1.2.4）：同一组断言跑在 mock 与 oMLX 上。
 * oMLX 不可达或无模型时打印 SKIPPED（不得静默通过）。
 */
async function assertLoadPolicyContract(backend: ModelBackend, modelId: string): Promise<void> {
  const admitBefore = await backend.admission(modelId);
  expect(["coexist", "requires_eviction"]).toContain(admitBefore);
  await backend.load(modelId, { mode: "resident", keepalive: { pinned: true }, admission: "coexist" });
  expect((await backend.status()).loaded).toContain(modelId);
  expect(await backend.admission(modelId)).toBe("coexist");
  await backend.unload(modelId);
  expect((await backend.status()).loaded).not.toContain(modelId);
}

describe("LoadPolicy golden consistency", () => {
  it("mock backend satisfies the contract", async () => {
    const b = new MockBackend({ memoryMaxGb: 10, models: [{ id: "m1", memoryGb: 4 }] });
    await assertLoadPolicyContract(b, "m1");
  });

  it("oMLX backend satisfies the same contract, or prints SKIPPED", async () => {
    const b = new OmlxBackend({ baseUrl: process.env.OMLX_URL ?? "http://127.0.0.1:8088" });
    let models: Awaited<ReturnType<ModelBackend["list"]>>;
    try {
      models = await b.list();
    } catch {
      console.log("SKIPPED: no oMLX reachable — golden suite ran only on the mock backend");
      return;
    }
    if (models.length === 0) {
      console.log("SKIPPED: oMLX reachable but no models available");
      return;
    }
    await assertLoadPolicyContract(b, models[0]!.id);
  });
});
