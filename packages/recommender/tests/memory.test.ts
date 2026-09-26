import { describe, expect, it } from "vitest";
import {
  appleReserveGb,
  appleUsableGb,
  bytesToGb,
  bytesToMib,
  footprintGb,
  kvBytes,
  kvCacheGb,
  recommendedWiredLimitMb,
  weightsGb,
  type ModelArch,
} from "../src/memory.js";

/** U0-LOG 实测用的 8B arch：36 层 / 8 KV head / head_dim 128。 */
const QWEN3_8B_ARCH: ModelArch = { n_layers: 36, n_kv_heads: 8, head_dim: 128 };

function relErr(actual: number, expected: number): number {
  return Math.abs(actual - expected) / expected;
}

describe("T2.1.1 内存公式 vs U0 实测基线", () => {
  it("Qwen3-8B 4bit 权重 ≈4.48GB（误差 <5%）", () => {
    // U0-LOG：Qwen3-8B-4bit 冷加载实测 4.48GB。
    const gb = weightsGb(8.19, { bpp: 0.55, quality: 0.98 });
    expect(gb).toBeCloseTo(4.5045, 3);
    expect(relErr(gb, 4.48)).toBeLessThan(0.05);
  });

  it("VL-7B 8bit 权重 ≈8.50GB（误差 <5%）", () => {
    // U0-LOG：Qwen2.5-VL-7B-8bit 实测 8.50GB。
    const gb = weightsGb(8.29, { bpp: 1.0, quality: 0.998 });
    expect(gb).toBeCloseTo(8.29, 3);
    expect(relErr(gb, 8.5)).toBeLessThan(0.05);
  });

  it("8B KV ≈9.00 MiB/64token（误差 <5%）", () => {
    // U0-LOG 实测 "9.00 MB/64token"；macOS 内存读数的 MB 是 MiB（2^20）。
    const mib = bytesToMib(kvBytes(QWEN3_8B_ARCH, 64, "fp16"));
    expect(mib).toBeCloseTo(9.0, 9);
    expect(relErr(mib, 9.0)).toBeLessThan(0.05);
  });

  it("8B KV@32K Q8 ≈2.4GB（docs/07 §1 的示例量级）", () => {
    const gb = kvCacheGb(QWEN3_8B_ARCH, 32768, "q8");
    expect(gb).toBeCloseTo(2.416, 2);
    // fp16 恰好是 Q8 的两倍。
    expect(kvCacheGb(QWEN3_8B_ARCH, 32768, "fp16") / gb).toBeCloseTo(2.0, 6);
  });

  it("KV 随 ctx 线性增长（docs/07 §1 的一等变量）", () => {
    expect(kvBytes(QWEN3_8B_ARCH, 2000, "q8") / kvBytes(QWEN3_8B_ARCH, 1000, "q8")).toBeCloseTo(2.0, 9);
  });

  it("footprint = 权重 + KV + 开销", () => {
    const fp = footprintGb({
      params_total_b: 9.41,
      quant: { weights_gb: 7.36, quality: 0.995 },
      arch: { n_layers: 32, n_kv_heads: 4, head_dim: 256 },
      ctx: 32768,
      kv_quant: "q8",
      overhead_gb: 1.0,
    });
    const expected = 7.36 + kvCacheGb({ n_layers: 32, n_kv_heads: 4, head_dim: 256 }, 32768, "q8") + 1.0;
    expect(fp).toBeCloseTo(expected, 9);
    expect(fp).toBeCloseTo(10.5075, 3);
  });
});

describe("T2.1.1 Apple 可用预算（docs/13 §3.1 修正公式）", () => {
  it("reserve = clamp(RAM × 0.30, 3, 16)", () => {
    expect(appleReserveGb(8)).toBe(3); // 2.4 → clamp 到 3
    expect(appleReserveGb(24)).toBeCloseTo(7.2, 6);
    expect(appleReserveGb(128)).toBe(16); // 38.4 → clamp 到 16
  });

  it("usable = min(RAM × pct, RAM − reserve)", () => {
    expect(appleUsableGb(8)).toBeCloseTo(5.0, 6);
    expect(appleUsableGb(16)).toBeCloseTo(10.56, 6);
    expect(appleUsableGb(24)).toBeCloseTo(15.84, 6); // 与 docs/13 §3.1 的 15.8 一致
    expect(appleUsableGb(64)).toBeCloseTo(42.24, 6); // 与 docs/13 §3.1 的 42.2 一致
    expect(appleUsableGb(128)).toBeCloseTo(84.48, 6);
  });

  it("三档 pct：0.66 / 0.70 / 0.75", () => {
    expect(appleUsableGb(64, "conservative")).toBeCloseTo(42.24, 6);
    expect(appleUsableGb(64, "moderate")).toBeCloseTo(44.8, 6);
    expect(appleUsableGb(64, "aggressive")).toBeCloseTo(48.0, 6);
  });

  it("sysctl 建议 = usable × 1024", () => {
    expect(recommendedWiredLimitMb(15.84)).toBe(16220);
    expect(recommendedWiredLimitMb(42.24)).toBe(43254);
  });

  it("bytesToGb / bytesToMib 口径分离", () => {
    expect(bytesToGb(1e9)).toBe(1);
    expect(bytesToMib(1024 * 1024)).toBe(1);
    expect(bytesToMib(1e9)).toBeCloseTo(953.67, 1);
  });
});
