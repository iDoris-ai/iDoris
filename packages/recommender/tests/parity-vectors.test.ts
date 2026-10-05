import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import {
  appleUsableGb,
  bytesToGb,
  bytesToGib,
  bytesToMib,
  footprintGb,
  kvBytes,
  nominalRamGb,
  parseChip,
  parseSystemProfilerGpuCores,
  weightsGb,
  type KvQuant,
  type ModelArch,
  type WiredMode,
} from "../src/index.js";

const root = resolve(import.meta.dirname, "../../..");
const read = <T>(name: string): T =>
  JSON.parse(readFileSync(resolve(root, "testdata/recommender", name), "utf8")) as T;

interface ProbeVectors {
  nominal_ram: Array<{ bytes: number; expected_gb: number }>;
  chips: Array<{ input: string; expected: string }>;
  gpu_profiler: Array<{ input: string; expected: number | null }>;
  invalid_ram_labels: string[];
}

interface MemoryVectors {
  units: Array<{ bytes: number; gb: number; mib: number; gib: number }>;
  weights: Array<{ params_total_b: number; bpp: number; weights_gb?: number; expected_gb: number }>;
  kv: { arch: ModelArch; ctx: number; quant: KvQuant; expected_bytes: number; expected_mib: number };
  apple_budget_64: Array<{ mode: WiredMode; expected_usable_gb: number }>;
  moe_total_params: {
    params_total_b: number;
    bpp: number;
    arch: ModelArch;
    ctx: number;
    kv_quant: KvQuant;
    overhead_gb: number;
    expected_footprint_gb: number;
  };
}

const probe = read<ProbeVectors>("probe.json");
const memory = read<MemoryVectors>("memory.json");

describe("B2 shared parity vectors", () => {
  it("locks probe byte/text behavior and named non-finite inputs", () => {
    for (const vector of probe.nominal_ram) expect(nominalRamGb(vector.bytes)).toBe(vector.expected_gb);
    for (const vector of probe.chips) expect(parseChip(vector.input || undefined)).toBe(vector.expected);
    for (const vector of probe.gpu_profiler) {
      expect(parseSystemProfilerGpuCores(vector.input)).toBe(vector.expected);
    }
    const invalid: Record<string, number> = {
      zero: 0,
      negative: -1,
      nan: Number.NaN,
      positive_infinity: Number.POSITIVE_INFINITY,
    };
    for (const label of probe.invalid_ram_labels) {
      expect(() => nominalRamGb(invalid[label] ?? Number.NaN)).toThrow();
    }
  });

  it("locks GB/GiB/MiB, weights priority, KV, Apple budgets, and MoE total params", () => {
    for (const vector of memory.units) {
      expect(bytesToGb(vector.bytes)).toBeCloseTo(vector.gb, 12);
      expect(bytesToMib(vector.bytes)).toBeCloseTo(vector.mib, 12);
      expect(bytesToGib(vector.bytes)).toBeCloseTo(vector.gib, 12);
    }
    for (const vector of memory.weights) {
      expect(
        weightsGb(vector.params_total_b, {
          bpp: vector.bpp,
          ...(vector.weights_gb === undefined ? {} : { weights_gb: vector.weights_gb }),
          quality: 1,
        }),
      ).toBeCloseTo(vector.expected_gb, 12);
    }
    const kv = kvBytes(memory.kv.arch, memory.kv.ctx, memory.kv.quant);
    expect(kv).toBe(memory.kv.expected_bytes);
    expect(bytesToMib(kv)).toBe(memory.kv.expected_mib);
    for (const vector of memory.apple_budget_64) {
      expect(appleUsableGb(64, vector.mode)).toBeCloseTo(vector.expected_usable_gb, 12);
    }
    const moe = memory.moe_total_params;
    expect(
      footprintGb({
        params_total_b: moe.params_total_b,
        quant: { bpp: moe.bpp, quality: 1 },
        arch: moe.arch,
        ctx: moe.ctx,
        kv_quant: moe.kv_quant,
        overhead_gb: moe.overhead_gb,
      }),
    ).toBeCloseTo(moe.expected_footprint_gb, 12);
  });
});
