import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { MockBackend } from "@idoris/adapters";
import { validateComponentCard } from "@idoris/contracts";
import type { Catalog, HostFacts, RecommenderPolicy } from "@idoris/recommender";
import { DefaultCapabilitiesProvider, type CapabilityEntry } from "../src/capabilities.js";
import type { Registered } from "../src/registry.js";
import { startRouter } from "../src/server.js";

const ADMISSION = ["ready", "requires_eviction", "blocked"];

const hardware: HostFacts = {
  ram_gb: 24,
  chip: "M4",
  gpu_cores: 10,
  os: "darwin",
  source: "injected",
};

const catalog = {
  version: 1,
  catalog: [
    {
      id: "core-9b",
      params_total_b: 9,
      arch: { n_layers: 2, n_kv_heads: 2, head_dim: 8 },
      roles: ["core"],
      capability: { reasoning: 0.9, coding: 0.8 },
      quant_options: [{ label: "q4", weights_gb: 5, quality: 0.98 }],
      min_ram_gb: 8,
    },
    {
      id: "vl-3b",
      params_total_b: 3,
      arch: { n_layers: 2, n_kv_heads: 2, head_dim: 8 },
      roles: ["temp"],
      capability: { vision: 0.9 },
      quant_options: [{ label: "q4", weights_gb: 2, quality: 0.98 }],
      min_ram_gb: 8,
    },
    {
      id: "big-35b",
      params_total_b: 35,
      arch: { n_layers: 2, n_kv_heads: 2, head_dim: 8 },
      roles: ["deep"],
      capability: { coding: 0.95, reasoning: 0.9 },
      quant_options: [{ label: "q4", weights_gb: 20, quality: 0.98 }],
      min_ram_gb: 64,
    },
  ],
} satisfies Catalog;

const policy: Partial<RecommenderPolicy> = { needed_capabilities: ["vision"], context_target: 4096 };

function makeRegistered(): Registered[] {
  const card = validateComponentCard({
    provider: {
      id: "mock",
      family: "local",
      tier: "local",
      capabilities: ["chat"],
      privacy_class: "local_only",
      cost: { input_per_m: 0, output_per_m: 0 },
      locality: "loopback",
    },
    form: "http_service",
    endpoint: "mock://in-memory",
    version_pin: "mock@0.1.0",
    privacy_class: "local_only",
    allowed_egress: ["none"],
    fallback_policy: "fail_closed",
    fail_closed: true,
    load_policy: { mode: "resident", keepalive: { pinned: true }, admission: "coexist" },
  });
  return [{ card, backend: new MockBackend({ memoryMaxGb: 16, models: [{ id: "mock-small", memoryGb: 2 }] }) }];
}

function assertEntry(entry: CapabilityEntry): void {
  expect(typeof entry.id).toBe("string");
  expect(entry.id.length).toBeGreaterThan(0);
  expect(typeof entry.capability).toBe("string");
  expect(entry.capability.length).toBeGreaterThan(0);
  expect(typeof entry.resident).toBe("boolean");
  expect(typeof entry.estimated_memory_gb).toBe("number");
  expect(typeof entry.ctx_limit).toBe("number");
  expect(typeof entry.queue_depth).toBe("number");
  expect(ADMISSION).toContain(entry.admission_status);
}

describe("T2.2.1 /capabilities 条目", () => {
  it("从 recommender + backend.status() 生成条目并带全部容量字段", async () => {
    const registered = makeRegistered();
    await registered[0]?.backend.load("mock-small");
    const provider = new DefaultCapabilitiesProvider({ registered, catalog, hardware, policy });
    const entries = await provider.snapshot();

    expect(entries.length).toBeGreaterThan(0);
    for (const entry of entries) assertEntry(entry);

    const resident = entries.find((e) => e.resident === true);
    expect(resident?.id).toBe("core-9b");
    expect(resident?.admission_status).toBe("ready");

    const vision = entries.find((e) => e.capability === "vision");
    expect(vision?.id).toBe("vl-3b");

    const blocked = entries.find((e) => e.admission_status === "blocked");
    expect(blocked?.id).toBe("big-35b");
    expect(blocked?.capability).toBe("coding");
    expect(blocked?.estimated_memory_gb).toBeGreaterThan(0);

    // queue_depth 来自 backend.status().loaded（已加载 1 个模型）。
    expect(entries.every((e) => e.queue_depth === 1)).toBe(true);
  });

  it("HTTP GET /capabilities 返回顶层 JSON 数组", async () => {
    const registered = makeRegistered();
    const provider = new DefaultCapabilitiesProvider({ registered, catalog, hardware, policy });
    const fixtures = join(dirname(fileURLToPath(import.meta.url)), "fixtures", "good");
    const router = await startRouter({ componentsDir: fixtures, capabilities: provider });
    try {
      const res = await fetch("http://127.0.0.1:" + router.port + "/capabilities");
      expect(res.status).toBe(200);
      const body = (await res.json()) as unknown;
      expect(Array.isArray(body)).toBe(true);
      const entries = body as CapabilityEntry[];
      expect(entries.length).toBeGreaterThan(0);
      for (const entry of entries) assertEntry(entry);
    } finally {
      await router.close();
    }
  });
});
