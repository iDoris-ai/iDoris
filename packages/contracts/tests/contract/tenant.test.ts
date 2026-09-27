import { describe, expect, it } from "vitest";
import { deployModeSchema, isIanaTimeZone, tenantContextSchema } from "../../src/tenant.js";

const valid = {
  tenant_id: "acme-co",
  budget: { limit_minor: 5_000_000, spent_minor: 1_234_567, scope: "paid_only" },
  billing_timezone: "Asia/Bangkok",
  quota: { rpm: 60, tpm: 200_000 },
};

const issues = (input: unknown): string[] => {
  const res = tenantContextSchema.safeParse(input);
  return res.success ? [] : res.error.issues.map((i) => i.path.join("."));
};

describe("TenantContext - 合法", () => {
  it("accepts a valid context", () => {
    expect(tenantContextSchema.safeParse(valid).success).toBe(true);
  });
  it("defaults budget.scope to paid_only when omitted", () => {
    const { budget: _b, ...rest } = valid;
    const parsed = tenantContextSchema.parse({ ...rest, budget: { limit_minor: 1, spent_minor: 0 } });
    expect(parsed.budget.scope).toBe("paid_only");
  });
  it("accepts UTC", () => {
    expect(tenantContextSchema.safeParse({ ...valid, billing_timezone: "UTC" }).success).toBe(true);
  });
});

describe("TenantContext - 非法", () => {
  it("rejects missing tenant_id", () => {
    const { tenant_id: _t, ...rest } = valid;
    expect(issues(rest)).toContain("tenant_id");
  });
  it("rejects a blank tenant_id", () => {
    expect(issues({ ...valid, tenant_id: "   " })).toContain("tenant_id");
  });
  it("rejects missing billing_timezone", () => {
    const { billing_timezone: _b, ...rest } = valid;
    expect(issues(rest)).toContain("billing_timezone");
  });
  it("rejects a non-IANA billing_timezone", () => {
    expect(issues({ ...valid, billing_timezone: "Mars/Phobos" })).toContain("billing_timezone");
  });
  it.each(["+08:00", "+08", "−08:00", "EST5EDT", "GMT0"])("rejects offset/legacy timezone %s", (tz) => {
    expect(issues({ ...valid, billing_timezone: tz })).toContain("billing_timezone");
  });
  it("rejects a non-enum budget.scope", () => {
    expect(issues({ ...valid, budget: { ...valid.budget, scope: "free" } })).toContain("budget.scope");
  });
});

describe("deploy_mode", () => {
  it("accepts personal / tenant", () => {
    expect(deployModeSchema.safeParse("personal").success).toBe(true);
    expect(deployModeSchema.safeParse("tenant").success).toBe(true);
  });
  it("rejects an unknown mode", () => {
    expect(deployModeSchema.safeParse("community").success).toBe(false);
  });
});

describe("isIanaTimeZone", () => {
  it("accepts canonical names and UTC", () => {
    expect(isIanaTimeZone("UTC")).toBe(true);
    expect(isIanaTimeZone("Asia/Bangkok")).toBe(true);
  });
  it("rejects offsets (ASCII and U+2212), legacy aliases, and junk", () => {
    for (const bad of ["+08:00", "+08", "-08", "−08:00", "EST5EDT", "GMT0", "", "Not/AZone"]) {
      expect(isIanaTimeZone(bad)).toBe(false);
    }
  });
});
