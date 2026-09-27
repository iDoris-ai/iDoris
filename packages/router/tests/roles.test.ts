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

  it("idoris/<未知角色> 抛 RoleParseError(unknown_role)，reason_code 与 code 一致", () => {
    expect(() => parseModelRole("idoris/core")).toThrow(RoleParseError);
    expect(() => parseModelRole("idoris/temp")).toThrow(RoleParseError);
    expect(() => parseModelRole("idoris/nope")).toThrow(RoleParseError);
    try {
      parseModelRole("idoris/core");
      throw new Error("should have thrown");
    } catch (e) {
      expect(e).toBeInstanceOf(RoleParseError);
      expect((e as RoleParseError).code).toBe("unknown_role");
      expect((e as RoleParseError).reason_code).toBe("unknown_role");
    }
  });

  it("旧角色名 core/temp 不再被接受（区别于新枚举）", () => {
    expect(() => parseModelRole("idoris/core")).toThrow(/未知角色/);
    expect(() => parseModelRole("idoris/temp")).toThrow(/未知角色/);
  });

  it("M2：前缀大小写不敏感（先 trim 再忽略大小写），角色部分精确匹配", () => {
    expect(parseModelRole("IDORIS/fast")).toBe("fast");
    expect(parseModelRole("Idoris/daily")).toBe("daily");
    expect(parseModelRole(" idoris/fast")).toBe("fast");
    expect(parseModelRole("idoris/fast ")).toBe("fast");
  });

  it("M2：命中前缀但角色部分不精确匹配 → unknown_role", () => {
    expect(() => parseModelRole("idoris/")).toThrow(RoleParseError);
    expect(() => parseModelRole("idoris/")).toThrow(/未知角色/);
    expect(() => parseModelRole("idoris/fast/x")).toThrow(RoleParseError);
    expect(() => parseModelRole("idoris/__proto__")).toThrow(RoleParseError);
  });

  it("L1：非字符串输入抛 RoleParseError（不是「按原模型名处理」的 null）", () => {
    expect(() => parseModelRole(undefined)).toThrow(RoleParseError);
    expect(() => parseModelRole(null)).toThrow(RoleParseError);
    expect(() => parseModelRole(1)).toThrow(RoleParseError);
    expect(() => parseModelRole({})).toThrow(RoleParseError);
    expect(() => parseModelRole(["idoris/fast"])).toThrow(RoleParseError);
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
      id: "daily-a",
      params_total_b: 9,
      arch: { n_layers: 1, n_kv_heads: 1, head_dim: 1 },
      quant_options: [{ label: "q4_k_m", weights_gb: 5, quality: 0.98 }],
      min_ram_gb: 16,
      roles: ["daily"],
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
      id: "vision-on-demand",
      params_total_b: 8,
      arch: { n_layers: 1, n_kv_heads: 1, head_dim: 1 },
      quant_options: [{ label: "q8_0", weights_gb: 8, quality: 0.998 }],
      min_ram_gb: 16,
      roles: ["vision"],
      load_hint: "on_demand",
    },
    {
      id: "coding-on-demand",
      params_total_b: 14,
      arch: { n_layers: 1, n_kv_heads: 1, head_dim: 1 },
      quant_options: [{ label: "q4_k_m", weights_gb: 8, quality: 0.98 }],
      min_ram_gb: 24,
      roles: [],
      load_hint: "on_demand",
    },
    {
      id: "experiment-daily",
      params_total_b: 9,
      arch: { n_layers: 1, n_kv_heads: 1, head_dim: 1 },
      quant_options: [{ label: "q4_k_m", weights_gb: 5, quality: 0.98 }],
      min_ram_gb: 16,
      roles: ["daily"],
      status: "experiment",
    },
    {
      id: "experiment-vision",
      params_total_b: 8,
      arch: { n_layers: 1, n_kv_heads: 1, head_dim: 1 },
      quant_options: [{ label: "q8_0", weights_gb: 8, quality: 0.998 }],
      min_ram_gb: 16,
      roles: ["vision"],
      status: "experiment",
    },
  ]);

  it("按角色返回 catalog 顺序内的候选 id", () => {
    expect(resolveRoleToCandidates("fast", catalog)).toEqual(["fast-a", "daily-explicit"]);
    expect(resolveRoleToCandidates("daily", catalog)).toEqual(["daily-a", "daily-explicit"]);
    expect(resolveRoleToCandidates("deep", catalog)).toEqual(["deep-a"]);
  });

  it("roles: [] 的按需槽不出现在任何角色候选里", () => {
    for (const role of ["fast", "daily", "deep", "vision", "embed", "rerank", "decide"] as const) {
      expect(resolveRoleToCandidates(role, catalog)).not.toContain("coding-on-demand");
    }
  });

  it("load_hint: on_demand 不影响角色候选归属（roles 和 load_hint 是独立的轴）", () => {
    expect(resolveRoleToCandidates("vision", catalog)).toContain("vision-on-demand");
  });

  it("H2 负对照：status: experiment 的条目即使带 daily/vision 角色也不得出现在候选里", () => {
    expect(resolveRoleToCandidates("daily", catalog)).not.toContain("experiment-daily");
    expect(resolveRoleToCandidates("vision", catalog)).not.toContain("experiment-vision");
  });

  it("按 installedModelIds 过滤已安装模型", () => {
    expect(resolveRoleToCandidates("daily", catalog, ["daily-explicit"])).toEqual(["daily-explicit"]);
    expect(resolveRoleToCandidates("daily", catalog, [])).toEqual([]);
  });

  it("没有 installedModelIds 时不过滤（按角色声明返回全部）", () => {
    expect(resolveRoleToCandidates("daily", catalog, undefined)).toEqual(["daily-a", "daily-explicit"]);
  });

  it("H2：minRamGb 作为可选参数参与过滤（排除 min_ram_gb 超出硬件的条目）", () => {
    // deep-a 需要 32GB；16GB 硬件下不应出现在候选里。
    expect(resolveRoleToCandidates("deep", catalog, undefined, 16)).toEqual([]);
    expect(resolveRoleToCandidates("deep", catalog, undefined, 32)).toEqual(["deep-a"]);
    // 不传 minRamGb 时不做硬件门槛过滤。
    expect(resolveRoleToCandidates("deep", catalog)).toEqual(["deep-a"]);
  });
});
