import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { loadCatalog, recommend, type Recommendation } from "../src/recommend.js";
import { makeHostFacts } from "../src/probe.js";

const CATALOG_PATH = fileURLToPath(new URL("../../../config/catalog.yaml", import.meta.url));
const FIXTURE_PATH = fileURLToPath(new URL("../../../testdata/recommender/ram.json", import.meta.url));
const catalog = loadCatalog(CATALOG_PATH);
const fixture = JSON.parse(readFileSync(FIXTURE_PATH, "utf8")) as {
  metadata: { recommender_commit: string; catalog_commit: string; catalog_sha256: string };
  cases: Record<string, { full?: Recommendation; projection?: ReturnType<typeof projection> }>;
};

function projection(rec: Recommendation) {
  return {
    resident_label: rec.resident_label,
    usable_gb: rec.usable_gb,
    reserve_gb: rec.reserve_gb,
    temp_reserve_gb: rec.temp_reserve_gb,
    resident_budget_gb: rec.resident_budget_gb,
    temp: rec.temp.map((item) => ({ id: item.id, capability: item.capability, status: item.status, quant: item.quant?.label ?? null })),
    blocked_ids: rec.blocked.map((item) => item.id),
    iogpu_wired_limit_mb: rec.recommended_sysctl.iogpu_wired_limit_mb,
  };
}

function recommendation(ram: number): Recommendation {
  return recommend({
    hardware: makeHostFacts({ ram_gb: ram, chip: "M4", gpu_cores: 10, os: "darwin" }),
    catalog,
  });
}

describe("B2 TS RAM reference fixture", () => {
  it("matches the real catalog at 8/16/24/32/64/128GB", () => {
    for (const ram of [8, 16, 24, 32, 64, 128]) {
      const expected = fixture.cases[String(ram)];
      expect(expected).toBeDefined();
      const actual = recommendation(ram);
      if (ram === 24) expect(actual).toEqual(expected?.full);
      else expect(projection(actual)).toEqual(expected?.projection);
    }
  });

  it("locks the source identity used to generate the fixture", () => {
    expect(fixture.metadata.recommender_commit).toBe("49a66e86e65e9ddf26f3bf3c9d68041df49d8981");
    expect(fixture.metadata.catalog_commit).toBe("49a66e86e65e9ddf26f3bf3c9d68041df49d8981");
    expect(fixture.metadata.catalog_sha256).toBe("5a49fe4e27dab1bca020479c956de7fc874cf6c856502617ca84a05304b4988a");
  });

  it("negative controls prove a one-field quant or budget drift is visible", () => {
    const expected = fixture.cases["24"]?.full;
    expect(expected).toBeDefined();
    const quantMutation = structuredClone(expected as Recommendation);
    if (quantMutation.resident !== null) quantMutation.resident.label = "q8_0";
    quantMutation.resident_label = quantMutation.resident === null ? null : quantMutation.resident.id + "@q8_0";
    expect(quantMutation).not.toEqual(expected);

    const budgetMutation = structuredClone(expected as Recommendation);
    budgetMutation.resident_budget_gb -= 1;
    expect(budgetMutation).not.toEqual(expected);
  });
});
