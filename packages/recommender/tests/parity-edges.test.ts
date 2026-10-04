import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import {
  parseCatalog,
  recommend,
  type Recommendation,
  type RecommenderPolicy,
} from "../src/recommend.js";
import { makeHostFacts } from "../src/probe.js";

type ScenarioInput = {
  name: string;
  ram: number;
  catalog: unknown;
  policy?: Partial<RecommenderPolicy>;
  env?: Record<string, string | undefined>;
};
type Scenario = { input: ScenarioInput; output: Recommendation };
const FIXTURE_PATH = fileURLToPath(new URL("../../../testdata/recommender/edges.json", import.meta.url));
const fixture = JSON.parse(readFileSync(FIXTURE_PATH, "utf8")) as {
  metadata: { source_commit: string };
  scenarios: Scenario[];
};

function replay(input: ScenarioInput): Recommendation {
  return recommend({
    hardware: makeHostFacts({ ram_gb: input.ram, chip: "fixture", gpu_cores: null, os: "darwin" }),
    catalog: parseCatalog(input.catalog),
    ...(input.policy === undefined ? {} : { policy: input.policy }),
    env: input.env ?? {},
  });
}

function scenario(name: string): Scenario {
  const found = fixture.scenarios.find((item) => item.input.name === name);
  if (found === undefined) throw new Error("missing edge fixture: " + name);
  return found;
}

describe("B2 TypeScript recommendation edge reference", () => {
  it("replays every minimal scenario including complete warnings/tradeoff", () => {
    expect(fixture.metadata.source_commit).toBe("49a66e86e65e9ddf26f3bf3c9d68041df49d8981");
    for (const item of fixture.scenarios) expect(replay(item.input)).toEqual(item.output);
  });

  it("locks stable first-candidate tie behavior and empty-catalog warning output", () => {
    expect(scenario("exact-tie-keeps-first").output.resident?.id).toBe("first");
    const empty = scenario("empty-catalog").output;
    expect(empty.resident).toBeNull();
    expect(empty.warnings.some((warning) => warning.includes("没有模型能放进常驻预算"))).toBe(true);
    expect(empty.tradeoff.trim()).not.toBe("");
  });

  it("RAM quality and resident-budget equality boundaries each have a one-input negative contrast", () => {
    expect(scenario("ram-equality-admits").output.resident?.id).toBe("equal-ram");
    expect(scenario("ram-below-blocks").output.blocked.map((item) => item.id)).toContain("equal-ram");

    expect(scenario("quality-threshold-equality-admits").output.resident?.id).toBe("quality-edge");
    expect(scenario("quality-below-threshold-rejects").output.resident).toBeNull();

    expect(scenario("resident-budget-equality-admits").output.resident?.id).toBe("budget-edge");
    expect(scenario("resident-budget-over-rejects").output.resident).toBeNull();
  });

  it("locks normal unknown and oversized override behavior", () => {
    expect(scenario("override-normal").output.override).toEqual({ id: "forced", active: true });
    expect(scenario("override-unknown-falls-back").output.override).toBeNull();
    const oversized = scenario("override-oversized-still-yields").output;
    expect(oversized.resident?.id).toBe("forced-big");
    expect(oversized.warnings.some((warning) => warning.includes("OOM"))).toBe(true);
    expect(oversized.warnings.some((warning) => warning.includes("绕过 min_ram_gb"))).toBe(true);
  });

  it("locks the current requires-eviction result even when a temp model cannot fit alone", () => {
    const edge = scenario("temp-too-large-even-alone-requires-eviction").output;
    expect(edge.temp).toHaveLength(1);
    expect(edge.temp[0]?.status).toBe("requires_eviction");
    expect(edge.temp[0]?.quant?.footprint_gb).toBeGreaterThan(edge.usable_gb);
    expect(edge.tradeoff).toContain("需驱逐");
  });
});
