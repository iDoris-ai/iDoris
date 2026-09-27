import { describe, expect, it } from "vitest";
import type { Catalog } from "@idoris/recommender";
import { parseModelRole, resolveRoleToCandidates, RoleParseError } from "../src/roles.js";

describe("parseModelRole（T4.2）", () => {
  it("解析 idoris/<role> 为角色", () => {
    expect(parseModelRole("idoris/fast")).toBe("fast");
    expect(parseModelRole("idoris/daily")).toBe("daily");
    expect(parseModelRole("idoris/deep")).toBe("deep");
    expect(parseModelRole("idoris/vision")).toBe("vision");
    expect(parseModelRole("idoris/embed")).toBe("embed");
    expect(parseModelRole("idoris/rerank")).toBe("rerank");
    expect(parseModelRole("idoris/decide")).toBe("decide");
    expect(parseModelRole("idoris/auto")).toBe("auto");
  });

  it("非 idoris/ 前缀返回 null（按原模型名处理）", () => {
    expect(parseModelRole("gpt-4o")).toBeNull();
    expect(parseModelRole("qwen3.5-9b")).toBeNull();
    expect(parseModelRole("")).toBeNull();
  });

  it("idoris/<未知角色> 抛 RoleParseError(unknown_role)", () => {
    expect(() => parseModelRole("idoris/core")).toThrow(RoleParseError);
    expect(() => parseModelRole("idoris/temp")).toThrow(RoleParseError);
    expect(() => parseModelRole("idoris/nope")).toThrow(RoleParseError);
    try {
      parseModelRole("idoris/core");
      throw new Error("should have thrown");
    } catch (e) {
      expect(e).toBeInstanceOf(RoleParseError);
      expect((e as RoleParseError).code).toBe("unknown_role");
    }
  });

  it("旧角色名 core/temp 不再被接受（区别于新枚举）", () => {
    expect(() => parseModelRole("idoris/core")).toThrow(/未知角色/);
    expect(() => parseModelRole("idoris/temp")).toThrow(/未知角色/);
  });
});

function makeCatalog(models: Catalog["catalog"]): Catalog {
  return { version: 1, catalog: models };
}

describe("resolveRoleToCandidates（T4.2）", () => {
  const catalog = makeCatalog([
    {
      id: "fast-a",
      params_total_b: 2,
      arch: { n_layers: 1, n_kv_heads: 1, head_dim: 1 },
      quant_options: [{ label: "q4_k_m", weights_gb: 1, quality: 0.98 }],
      min_ram_gb: 8,
      roles: ["fast"],
    },
    {
      id: "daily-implicit",
      params_total_b: 9,
      arch: { n_layers: 1, n_kv_heads: 1, head_dim: 1 },
      quant_options: [{ label: "q4_k_m", weights_gb: 5, quality: 0.98 }],
      min_ram_gb: 16,
      // 省略 roles：按 daily 处理。
    },
    {
      id: "daily-explicit",
      params_total_b: 9,
      arch: { n_layers: 1, n_kv_heads: 1, head_dim: 1 },
      quant_options: [{ label: "q4_k_m", weights_gb: 5, quality: 0.98 }],
      min_ram_gb: 16,
      roles: ["fast", "daily"],
    },
    {
      id: "deep-a",
      params_total_b: 35,
      arch: { n_layers: 1, n_kv_heads: 1, head_dim: 1 },
      quant_options: [{ label: "q4_k_m", weights_gb: 20, quality: 0.98 }],
      min_ram_gb: 32,
      roles: ["deep"],
    },
    {
      id: "on-demand-vl",
      params_total_b: 8,
      arch: { n_layers: 1, n_kv_heads: 1, head_dim: 1 },
      quant_options: [{ label: "q8_0", weights_gb: 8, quality: 0.998 }],
      min_ram_gb: 16,
      roles: [],
      load_hint: "on_demand",
    },
  ]);

  it("按角色返回 catalog 顺序内的候选 id", () => {
    expect(resolveRoleToCandidates("fast", catalog)).toEqual(["fast-a", "daily-explicit"]);
    expect(resolveRoleToCandidates("daily", catalog)).toEqual(["daily-implicit", "daily-explicit"]);
    expect(resolveRoleToCandidates("deep", catalog)).toEqual(["deep-a"]);
  });

  it("roles: [] 的按需槽不出现在任何角色候选里", () => {
    for (const role of ["fast", "daily", "deep", "vision", "embed", "rerank", "decide"] as const) {
      expect(resolveRoleToCandidates(role, catalog)).not.toContain("on-demand-vl");
    }
  });

  it("role=auto 没有静态候选（交给 iDoris 选，属于 recommender/dispatch 的职责）", () => {
    expect(resolveRoleToCandidates("auto", catalog)).toEqual([]);
  });

  it("按 installedModelIds 过滤已安装模型", () => {
    expect(resolveRoleToCandidates("daily", catalog, ["daily-explicit"])).toEqual(["daily-explicit"]);
    expect(resolveRoleToCandidates("daily", catalog, [])).toEqual([]);
  });

  it("没有 installedModelIds 时不过滤（按角色声明返回全部）", () => {
    expect(resolveRoleToCandidates("daily", catalog, undefined)).toEqual(["daily-implicit", "daily-explicit"]);
  });
});
