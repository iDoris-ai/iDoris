import { describe, expect, it } from "vitest";
import {
  UNKNOWN_CHIP,
  inspectHost,
  makeHostFacts,
  nominalRamGb,
  parseChip,
  parseSystemProfilerGpuCores,
  withSystemProfiler,
} from "../src/probe.js";

const GIB = 1024 ** 3;

describe("T2.1.1 硬件探测（可注入）", () => {
  it("makeHostFacts 只要求 ram_gb，并为其它字段兜底", () => {
    const facts = makeHostFacts({ ram_gb: 24 });
    expect(facts).toEqual({ ram_gb: 24, chip: UNKNOWN_CHIP, gpu_cores: null, os: "unknown", source: "injected" });
  });

  it("makeHostFacts 保留注入的 chip/gpu_cores/os", () => {
    const facts = makeHostFacts({ ram_gb: 64, chip: "M1 Max", gpu_cores: 32, os: "darwin" });
    expect(facts.chip).toBe("M1 Max");
    expect(facts.gpu_cores).toBe(32);
    expect(facts.os).toBe("darwin");
  });

  it("拒绝非法 ram_gb", () => {
    expect(() => makeHostFacts({ ram_gb: 0 })).toThrow();
    expect(() => makeHostFacts({ ram_gb: Number.NaN })).toThrow();
  });

  it("nominalRamGb 把字节归一到标称 GB", () => {
    expect(nominalRamGb(24 * GIB)).toBe(24);
    expect(nominalRamGb(64 * GIB)).toBe(64);
    expect(nominalRamGb(18 * GIB)).toBe(18);
    expect(() => nominalRamGb(0)).toThrow();
  });

  it("parseChip 从 os.cpus() 的 model 提取", () => {
    expect(parseChip("Apple M4")).toBe("M4");
    expect(parseChip("Apple M1 Max")).toBe("M1 Max");
    expect(parseChip("Apple M3 Ultra")).toBe("M3 Ultra");
    expect(parseChip(undefined)).toBe(UNKNOWN_CHIP);
  });

  it("parseSystemProfilerGpuCores 解析 system_profiler", () => {
    const out = "Chipset Model: Apple M4\n  Total Number of Cores: 10\n";
    expect(parseSystemProfilerGpuCores(out)).toBe(10);
    expect(parseSystemProfilerGpuCores("no gpu info")).toBeNull();
  });

  it("withSystemProfiler 合并 GPU 核数", () => {
    const base = makeHostFacts({ ram_gb: 24, chip: "M4" });
    const merged = withSystemProfiler(base, "Total Number of Cores: 10");
    expect(merged.gpu_cores).toBe(10);
    expect(merged.ram_gb).toBe(24);
  });

  it("inspectHost 真实探测返回正 RAM 与 probe 标记", () => {
    const facts = inspectHost();
    expect(facts.ram_gb).toBeGreaterThan(0);
    expect(facts.source).toBe("probe");
    expect(typeof facts.os).toBe("string");
  });
});
