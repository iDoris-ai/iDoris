import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { parse } from "yaml";
import { describe, expect, it } from "vitest";
import { CatalogError, loadCatalog, parseCatalog, recommend } from "../src/recommend.js";
import { makeHostFacts } from "../src/probe.js";

const CATALOG_PATH = fileURLToPath(new URL("../../../config/catalog.yaml", import.meta.url));
const catalog = loadCatalog(CATALOG_PATH);

function m4(ramGb: number) {
  return makeHostFacts({ ram_gb: ramGb, chip: "M4", gpu_cores: 10, os: "darwin" });
}

describe("T2.1.2 catalog 加载与硬门槛", () => {
  it("加载 config/catalog.yaml，含验收所需条目", () => {
    expect(catalog.version).toBe(1);
    const ids = catalog.catalog.map((m) => m.id);
    expect(ids).toContain("ornith-1.0-9b");
    expect(ids).toContain("agents-a1-35b");
    expect(ids).toContain("qwen3-8b");
    expect(ids).toContain("qwen2.5-vl-7b");
  });

  it("每条都有 min_ram_gb 硬门槛字段", () => {
    for (const model of catalog.catalog) {
      expect(typeof model.min_ram_gb).toBe("number");
      expect(model.min_ram_gb).toBeGreaterThan(0);
    }
  });

  it("拒绝缺 min_ram_gb 的目录条目", () => {
    const bad = {
      version: 1,
      catalog: [
        {
          id: "x",
          params_total_b: 1,
          arch: { n_layers: 1, n_kv_heads: 1, head_dim: 1 },
          quant_options: [{ label: "q4_k_m", weights_gb: 1, quality: 0.98 }],
        },
      ],
    };
    expect(() => parseCatalog(bad)).toThrow(CatalogError);
  });

  it("拒绝重复 id", () => {
    const model = {
      id: "dup",
      params_total_b: 1,
      arch: { n_layers: 1, n_kv_heads: 1, head_dim: 1 },
      quant_options: [{ label: "q4_k_m", weights_gb: 1, quality: 0.98 }],
      min_ram_gb: 8,
    };
    expect(() => parseCatalog({ version: 1, catalog: [model, model] })).toThrow(/重复/);
  });
});

describe("T2.1.2 M4/24GB profile（验收主场景）", () => {
  const rec = recommend({ hardware: m4(24), catalog });

  it("resident = ornith-1.0-9b@q6_k", () => {
    expect(rec.resident_label).toBe("ornith-1.0-9b@q6_k");
    expect(rec.resident?.id).toBe("ornith-1.0-9b");
    expect(rec.resident?.label).toBe("q6_k");
    expect(rec.resident?.quality).toBeCloseTo(0.995, 6);
    expect(rec.resident?.footprint_gb).toBeCloseTo(10.5075, 3);
    expect(rec.resident_budget_gb).toBeCloseTo(11.34, 6);
  });

  it("agents-a1-35b 被 min_ram_gb 硬门槛判 BLOCKED", () => {
    const blocked = rec.blocked.find((b) => b.id === "agents-a1-35b");
    expect(blocked).toBeDefined();
    expect(blocked?.status).toBe("BLOCKED");
    expect(blocked?.min_ram_gb).toBe(32);
    expect(rec.warnings.some((w) => w.includes("agents-a1-35b") && w.includes("BLOCKED"))).toBe(true);
  });

  it("Q8 因临时槽预留被挡在常驻预算外", () => {
    // 若选 Q8 会吃掉临时槽：这正是 07 §4.1「Q6_K 最优」的算法体现。
    expect(rec.temp_reserve_gb).toBeGreaterThan(0);
    expect(rec.usable_gb).toBeCloseTo(15.84, 6);
  });
});

describe("T2.1.2 其它 RAM 档", () => {
  it("16GB 降到 q4/q5（不再推荐 q6/q8）", () => {
    const rec = recommend({ hardware: m4(16), catalog });
    expect(rec.resident).not.toBeNull();
    expect(["q4_k_m", "q5_k_m"]).toContain(rec.resident?.label);
    expect(rec.resident?.label).not.toBe("q6_k");
    expect(rec.resident?.label).not.toBe("q8_0");
    // 且常驻体积确实比 24GB 档小。
    const rec24 = recommend({ hardware: m4(24), catalog });
    expect(rec.resident?.footprint_gb ?? Infinity).toBeLessThan(rec24.resident?.footprint_gb ?? 0);
  });

  it("32GB 允许 35B（不再 BLOCKED，但需驱逐常驻）", () => {
    const rec = recommend({ hardware: m4(32), catalog });
    expect(rec.blocked.some((b) => b.id === "agents-a1-35b")).toBe(false);
    const coding = rec.temp.find((t) => t.capability === "coding");
    expect(coding?.id).toBe("agents-a1-35b");
    expect(coding?.status).toBe("requires_eviction");
  });

  it("64GB 预算与 docs/13 §3.1 的 42.2GB 一致", () => {
    const rec = recommend({ hardware: m4(64), catalog });
    expect(rec.usable_gb).toBeCloseTo(42.24, 6);
  });

  it("8GB 上 9B / 35B 全部 BLOCKED（min_ram_gb 硬门槛）", () => {
    const rec = recommend({ hardware: m4(8), catalog });
    for (const model of catalog.catalog) {
      if (8 < model.min_ram_gb) {
        expect(rec.blocked.some((b) => b.id === model.id)).toBe(true);
      }
    }
  });
});

describe("T2.1.2 IDORIS_CORE_MODEL override", () => {
  it("强制常驻时推荐让路，并输出警告", () => {
    const rec = recommend({ hardware: m4(24), catalog, env: { IDORIS_CORE_MODEL: "qwen3.5-9b" } });
    expect(rec.override).toEqual({ id: "qwen3.5-9b", active: true });
    expect(rec.resident?.id).toBe("qwen3.5-9b");
    expect(rec.warnings.some((w) => w.includes("IDORIS_CORE_MODEL=qwen3.5-9b") && w.includes("让路"))).toBe(true);
  });

  it("强制超出 min_ram_gb 的模型也放行，但必须抱怨", () => {
    const rec = recommend({ hardware: m4(24), catalog, env: { IDORIS_CORE_MODEL: "agents-a1-35b" } });
    expect(rec.resident?.id).toBe("agents-a1-35b");
    expect(rec.warnings.some((w) => w.includes("绕过 min_ram_gb"))).toBe(true);
  });

  it("未知 override 回落到自动选择并警告", () => {
    const rec = recommend({ hardware: m4(24), catalog, env: { IDORIS_CORE_MODEL: "does-not-exist" } });
    expect(rec.override).toBeNull();
    expect(rec.resident_label).toBe("ornith-1.0-9b@q6_k");
    expect(rec.warnings.some((w) => w.includes("不在目录中"))).toBe(true);
  });
});

describe("T4.2 角色枚举统一（core→daily，temp→load_hint）", () => {
  it("config/catalog.yaml 数据区不再残留旧角色名 core/temp", () => {
    const raw = parse(readFileSync(CATALOG_PATH, "utf8")) as { catalog: Array<{ roles?: string[] }> };
    for (const model of raw.catalog) {
      for (const role of model.roles ?? []) {
        expect(role).not.toBe("core");
        expect(role).not.toBe("temp");
      }
    }
  });

  it("旧 core 角色条目已改名为 daily，且仍参与常驻自动推荐", () => {
    const ornith = catalog.catalog.find((m) => m.id === "ornith-1.0-9b");
    expect(ornith?.roles).toEqual(["daily"]);
    // 24GB 档验收基线：ornith-1.0-9b 仍被选为常驻（改名未改变行为）。
    const rec = recommend({ hardware: m4(24), catalog });
    expect(rec.resident?.id).toBe("ornith-1.0-9b");
  });

  it("旧 temp 角色条目已移出 roles（空数组，不落入「未声明 roles=daily」的默认档），改用 load_hint: on_demand", () => {
    const vl = catalog.catalog.find((m) => m.id === "qwen2.5-vl-7b");
    expect(vl?.roles).toEqual([]);
    expect(vl?.load_hint).toBe("on_demand");
    const coder = catalog.catalog.find((m) => m.id === "qwen2.5-coder-14b");
    expect(coder?.roles).toEqual([]);
    expect(coder?.load_hint).toBe("on_demand");
  });

  it("load_hint: on_demand 条目不参与常驻自动推荐（roles=[] 排除在 daily 之外）", () => {
    const rec = recommend({ hardware: m4(24), catalog });
    expect(rec.resident?.id).not.toBe("qwen2.5-vl-7b");
    expect(rec.resident?.id).not.toBe("qwen2.5-coder-14b");
  });

  it("未知角色显式报错，不静默忽略", () => {
    const bad = {
      version: 1,
      catalog: [
        {
          id: "x",
          params_total_b: 1,
          arch: { n_layers: 1, n_kv_heads: 1, head_dim: 1 },
          quant_options: [{ label: "q4_k_m", weights_gb: 1, quality: 0.98 }],
          min_ram_gb: 8,
          roles: ["core"],
        },
      ],
    };
    expect(() => parseCatalog(bad)).toThrow(CatalogError);
    expect(() => parseCatalog(bad)).toThrow(/未知角色/);
  });

  it("旧 temp 角色值同样显式报错", () => {
    const bad = {
      version: 1,
      catalog: [
        {
          id: "x",
          params_total_b: 1,
          arch: { n_layers: 1, n_kv_heads: 1, head_dim: 1 },
          quant_options: [{ label: "q4_k_m", weights_gb: 1, quality: 0.98 }],
          min_ram_gb: 8,
          roles: ["temp"],
        },
      ],
    };
    expect(() => parseCatalog(bad)).toThrow(/未知角色/);
  });

  it("未知 load_hint 显式报错", () => {
    const bad = {
      version: 1,
      catalog: [
        {
          id: "x",
          params_total_b: 1,
          arch: { n_layers: 1, n_kv_heads: 1, head_dim: 1 },
          quant_options: [{ label: "q4_k_m", weights_gb: 1, quality: 0.98 }],
          min_ram_gb: 8,
          load_hint: "resident",
        },
      ],
    };
    expect(() => parseCatalog(bad)).toThrow(/load_hint/);
  });

  it("合法角色枚举（fast|daily|deep|vision|embed|rerank|decide）全部被接受", () => {
    for (const role of ["fast", "daily", "deep", "vision", "embed", "rerank", "decide"]) {
      const ok = {
        version: 1,
        catalog: [
          {
            id: "x",
            params_total_b: 1,
            arch: { n_layers: 1, n_kv_heads: 1, head_dim: 1 },
            quant_options: [{ label: "q4_k_m", weights_gb: 1, quality: 0.98 }],
            min_ram_gb: 8,
            roles: [role],
          },
        ],
      };
      expect(() => parseCatalog(ok)).not.toThrow();
    }
  });
});
