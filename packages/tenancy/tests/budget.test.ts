import { describe, expect, it } from "vitest";
import type { TenantContext } from "@idoris/contracts";
import { checkBudget, type BillingCounter } from "../src/budget.js";

const ctx = (over: boolean, scope: "paid_only" | "all"): TenantContext => ({
  tenant_id: "acme",
  budget: { limit_minor: 1000, spent_minor: over ? 1000 : 100, scope },
  billing_timezone: "Asia/Bangkok",
});
const priced = [
  { id: "paid", costMinor: 5 },
  { id: "local", costMinor: 0 },
];

describe("checkBudget", () => {
  it("allows when under budget", () => {
    const d = checkBudget(ctx(false, "all"), priced);
    expect(d.status).toBe(200);
    expect(d.allowed).toEqual(["paid", "local"]);
  });
  it("scope=all rejects with 402 budget_exceeded and no billing", () => {
    const billing: BillingCounter = { count: 0 };
    const d = checkBudget(ctx(true, "all"), priced);
    expect(d.status).toBe(402);
    expect((d.body.error as { type: string }).type).toBe("budget_exceeded");
    expect(billing.count).toBe(0);
  });
  it("scope=paid_only still allows the zero-cost local model", () => {
    const billing: BillingCounter = { count: 0 };
    const d = checkBudget(ctx(true, "paid_only"), priced);
    expect(d.status).toBe(200);
    expect(d.allowed).toEqual(["local"]);
    expect(billing.count).toBe(0);
  });
  it("scope=paid_only rejects when every candidate costs money", () => {
    const d = checkBudget(ctx(true, "paid_only"), [{ id: "paid", costMinor: 5 }]);
    expect(d.status).toBe(402);
    expect(d.allowed).toEqual([]);
  });
  it("personal mode (no tenant) is not gated", () => {
    const d = checkBudget(undefined, priced);
    expect(d.status).toBe(200);
  });
});
