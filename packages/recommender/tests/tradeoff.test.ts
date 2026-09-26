import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { loadCatalog, recommend } from "../src/recommend.js";
import { makeHostFacts } from "../src/probe.js";

const CATALOG_PATH = fileURLToPath(new URL("../../../config/catalog.yaml", import.meta.url));
const catalog = loadCatalog(CATALOG_PATH);

function m4(ramGb: number) {
  return makeHostFacts({ ram_gb: ramGb, chip: "M4", gpu_cores: 10, os: "darwin" });
}

describe("T2.1.3 可读 tradeoff 输出 + sysctl 建议", () => {
  const rec = recommend({ hardware: m4(24), catalog });

  it("输出含 warnings[]、recommended_sysctl.iogpu_wired_limit_mb、非空 tradeoff", () => {
    expect(Array.isArray(rec.warnings)).toBe(true);
    expect(rec.warnings.length).toBeGreaterThan(0);
    for (const w of rec.warnings) expect(w.length).toBeGreaterThan(0);

    expect(rec.recommended_sysctl).toBeDefined();
    expect(typeof rec.recommended_sysctl.iogpu_wired_limit_mb).toBe("number");
    expect(rec.recommended_sysctl.iogpu_wired_limit_mb).toBeGreaterThan(0);
    // docs/07 §5.3：iogpu.wired_limit_mb = usable × 1024。
    expect(rec.recommended_sysctl.iogpu_wired_limit_mb).toBe(Math.round(rec.usable_gb * 1024));

    expect(typeof rec.tradeoff).toBe("string");
    expect(rec.tradeoff.trim().length).toBeGreaterThan(0);
  });

  it("tradeoff 是可读多行文本，解释为什么这么选", () => {
    const lines = rec.tradeoff.split("\n");
    expect(lines.length).toBeGreaterThanOrEqual(4);
    expect(rec.tradeoff).toContain("常驻");
    expect(rec.tradeoff).toContain("BLOCKED");
    expect(rec.tradeoff).toContain("预算");
    expect(rec.tradeoff).toContain("ornith-1.0-9b@q6_k");
    expect(rec.tradeoff).not.toContain("undefined");
  });

  it("warnings 不空且提到预算与需驱逐项", () => {
    expect(rec.warnings.some((w) => w.includes("usable"))).toBe(true);
    expect(rec.warnings.some((w) => w.includes("需驱逐常驻"))).toBe(true);
  });

  it("每个 RAM 档都给出非空 tradeoff 与 sysctl", () => {
    for (const ram of [8, 16, 24, 32, 64, 128]) {
      const out = recommend({ hardware: m4(ram), catalog });
      expect(out.tradeoff.trim().length).toBeGreaterThan(0);
      expect(out.warnings.length).toBeGreaterThan(0);
      expect(out.recommended_sysctl.iogpu_wired_limit_mb).toBeGreaterThan(0);
      expect(out.recommended_sysctl.iogpu_wired_limit_mb).toBe(Math.round(out.usable_gb * 1024));
    }
  });
});
