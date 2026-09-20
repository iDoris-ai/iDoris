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
