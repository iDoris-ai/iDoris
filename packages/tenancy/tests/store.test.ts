import { describe, expect, it } from "vitest";
import type { TenantContext } from "@idoris/contracts";
import { TenantScopeError, TenantStore } from "../src/store.js";

const ctx = (tenant_id: string): TenantContext => ({
  tenant_id,
  budget: { limit_minor: 1_000_000, spent_minor: 0, scope: "paid_only" },
  billing_timezone: "Asia/Bangkok",
});

describe("TenantStore - 硬隔离", () => {
  it("A cannot see any of B's usage/budget/audit rows", () => {
    const s = new TenantStore();
    s.put(ctx("A"), "usage", "u1", { tokens: 1 });
    s.put(ctx("A"), "budget", "b1", { spent: 1 });
    s.put(ctx("B"), "usage", "u2", { tokens: 2 });
    s.put(ctx("B"), "audit", "a2", { reason: "privacy_enforced" });
    expect(s.list(ctx("A")).map((r) => r.id)).toEqual(["u1", "b1"]);
    expect(s.get(ctx("A"), "usage", "u2")).toBeUndefined();
    expect(s.list(ctx("B")).map((r) => r.id)).toEqual(["u2", "a2"]);
  });

  it("throws when tenant scope is missing (not returning everything)", () => {
    const s = new TenantStore();
    s.put(ctx("A"), "usage", "u1", {});
    expect(() => s.list(undefined)).toThrow(TenantScopeError);
    expect(() => s.put(undefined, "usage", "x", {})).toThrow(TenantScopeError);
    expect(() => s.get(undefined, "usage", "u1")).toThrow(TenantScopeError);
  });

  it("filters by kind within a tenant", () => {
    const s = new TenantStore();
    s.put(ctx("A"), "usage", "u1", {});
    s.put(ctx("A"), "audit", "a1", {});
    expect(s.list(ctx("A"), "audit").map((r) => r.id)).toEqual(["a1"]);
  });
});
