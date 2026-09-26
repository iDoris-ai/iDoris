import { describe, expect, it } from "vitest";
import { MockBackend } from "../mock/mock-backend.js";

const models = [
  { id: "a", memoryGb: 6 },
  { id: "b", memoryGb: 6 },
  { id: "c", memoryGb: 4 },
];
const resident = { mode: "resident", keepalive: { pinned: true }, admission: "coexist" } as const;
const onDemand = { mode: "on_demand", keepalive: { idle_ttl_s: 300 }, admission: "requires_eviction" } as const;

describe("MockBackend - LoadPolicy 语义", () => {
  it("never evicts a pinned (resident) model under ceiling pressure", async () => {
    const b = new MockBackend({ memoryMaxGb: 10, models });
    await b.load("a", resident);
    await b.load("c", onDemand); // 6 + 4 = 10 (ceiling)
    await b.load("b", onDemand); // 16 > 10 -> evict unpinned
    const st = await b.status();
    expect(st.loaded).toContain("a");
    expect(st.loaded).not.toContain("b");
    expect(st.loaded).not.toContain("c");
  });

  it("evicts unpinned models in LRU order", async () => {
    const four = [
      { id: "x", memoryGb: 4 },
      { id: "y", memoryGb: 4 },
      { id: "z", memoryGb: 4 },
    ];
    const b = new MockBackend({ memoryMaxGb: 10, models: four });
    await b.load("x", onDemand);
    await b.load("y", onDemand);
    await b.load("z", onDemand); // 12 > 10 -> evict LRU x
    const st = await b.status();
    expect(st.loaded).not.toContain("x");
    expect(st.loaded).toContain("y");
    expect(st.loaded).toContain("z");
  });

  it("reports admission coexist/requires_eviction", async () => {
    const b = new MockBackend({ memoryMaxGb: 10, models });
    await b.load("a", resident); // used 6
    expect(await b.admission("c")).toBe("coexist"); // 6+4 = 10
    expect(await b.admission("b")).toBe("requires_eviction"); // 6+6 = 12
    expect(await b.admission("a")).toBe("coexist");
  });

  it("grades pressure ok/soft/hard/ceiling", async () => {
    const b = new MockBackend({ memoryMaxGb: 10, models });
    expect((await b.status()).pressure).toBe("ok");
    await b.load("c", onDemand); // 4/10
    expect((await b.status()).pressure).toBe("ok");
    await b.load("a", resident); // 10/10
    expect((await b.status()).pressure).toBe("ceiling");
  });

  /**
   * 上面那条测试的名字承诺了四档，但它构造的 ratio 只有 0 / 0.4 / 1.0 —— **从未落进
   * soft(0.85–0.95) 与 hard(0.95–1.0) 两个区间**。评审（PR #14 第 2 项）做过变异验证：
   * 把 `softThreshold` 与 `hardThreshold` 的默认值对调，那 6 条测试**全部照常通过**。
   * 也就是说这两档的分级逻辑一直是**裸奔**的，真回归会静默上线。
   *
   * ⚠️ 这不是「再加一条断言」的问题，是**量纲**问题：`a=6/b=6/c=4` 配 `memoryMaxGb: 10`
   * 这组固定值，无论怎么组合都凑不出 0.85–1.0 之间的任何一个点。所以必须换一组
   * 能落进目标区间的体积，而不是在旧 fixture 上加断言。
   */
  describe("pressure 的 soft/hard 两档必须被真正落点覆盖", () => {
    /** 单模型单实例，一次 load 直达目标 ratio；不依赖加载顺序，也不会触发 enforce()（只在 > max 时驱逐）。 */
    const atRatio = (usedGb: number): MockBackend =>
      new MockBackend({ memoryMaxGb: 100, models: [{ id: "m", memoryGb: usedGb }] });

    it("ratio 落在 soft 区间内 → soft", async () => {
      const b = atRatio(88); // 0.88 ∈ [0.85, 0.95)
      await b.load("m", resident);
      expect((await b.status()).pressure).toBe("soft");
    });

    it("ratio 落在 hard 区间内 → hard", async () => {
      const b = atRatio(97); // 0.97 ∈ [0.95, 1.0)
      await b.load("m", resident);
      expect((await b.status()).pressure).toBe("hard");
    });

    it("边界：恰好 = softThreshold → soft（判定是 >=，不是 >）", async () => {
      const b = atRatio(85); // 0.85
      await b.load("m", resident);
      expect((await b.status()).pressure).toBe("soft");
    });

    it("边界：恰好 = hardThreshold → hard", async () => {
      const b = atRatio(95); // 0.95
      await b.load("m", resident);
      expect((await b.status()).pressure).toBe("hard");
    });

    it("边界：soft 区间下沿之下 → 仍是 ok", async () => {
      const b = atRatio(84); // 0.84
      await b.load("m", resident);
      expect((await b.status()).pressure).toBe("ok");
    });

    it("边界：恰好 = 1.0 → ceiling（不是 hard）", async () => {
      const b = atRatio(100); // 1.0
      await b.load("m", resident);
      expect((await b.status()).pressure).toBe("ceiling");
    });
  });

  it("chat loads on demand and touches LRU", async () => {
    const b = new MockBackend({ memoryMaxGb: 10, models });
    const res = await b.chat({ model: "c", messages: [{ role: "user", content: "hi" }] });
    expect(res.content).toBe("mock:c");
    expect((await b.status()).loaded).toContain("c");
  });

  it("rejects unknown models", async () => {
    const b = new MockBackend({ memoryMaxGb: 10, models });
    await expect(b.load("nope")).rejects.toThrow(/unknown model/);
  });
});
