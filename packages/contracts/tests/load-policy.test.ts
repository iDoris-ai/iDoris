import { describe, expect, it } from "vitest";
import { loadPolicySchema } from "../src/load-policy.js";

describe("LoadPolicy", () => {
  it("accepts a valid policy", () => {
    expect(loadPolicySchema.safeParse({ mode: "on_demand", keepalive: { idle_ttl_s: 300 }, admission: "requires_eviction" }).success).toBe(true);
  });
  it("rejects a policy missing mode", () => {
    const res = loadPolicySchema.safeParse({ keepalive: { pinned: true }, admission: "coexist" });
    expect(res.success).toBe(false);
    if (!res.success) expect(res.error.issues.some((i) => i.path.join(".") === "mode")).toBe(true);
  });
  it("rejects an empty keepalive", () => {
    expect(loadPolicySchema.safeParse({ mode: "resident", keepalive: {}, admission: "coexist" }).success).toBe(false);
  });
});
