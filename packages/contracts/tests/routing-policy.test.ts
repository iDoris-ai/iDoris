import { describe, expect, it } from "vitest";
import { routingPolicySchema } from "../src/routing-policy.js";

const valid = {
  routing_policy: {
    version: 1,
    rules: [
      { if: { privacy: "local_only" }, then: { tiers: ["local", "lora"], fail_closed: true } },
      { if: { intent: "banner" }, then: { capability: "vision", load: "on_demand" } },
    ],
    default: { tiers: ["local"], fail_closed: true },
  },
};

describe("RoutingPolicy", () => {
  it("accepts a valid policy", () => {
    expect(routingPolicySchema.safeParse(valid).success).toBe(true);
  });
  it("rejects a policy missing default", () => {
    const res = routingPolicySchema.safeParse({ routing_policy: { version: 1, rules: [] } });
    expect(res.success).toBe(false);
  });
  it("rejects an empty default strategy", () => {
    const bad = { routing_policy: { version: 1, rules: [], default: {} } };
    expect(routingPolicySchema.safeParse(bad).success).toBe(false);
  });
  it("rejects an empty then", () => {
    const bad = { routing_policy: { version: 1, rules: [{ if: {}, then: {} }], default: { tiers: ["local"] } } };
    expect(routingPolicySchema.safeParse(bad).success).toBe(false);
  });
  it("rejects an empty tiers list", () => {
    const bad = { routing_policy: { version: 1, rules: [], default: { tiers: [] } } };
    expect(routingPolicySchema.safeParse(bad).success).toBe(false);
  });
  it("rejects an unknown tier in a rule", () => {
    const bad = { routing_policy: { ...valid.routing_policy, rules: [{ if: {}, then: { tiers: ["local", "cloud"] } }] } };
    expect(routingPolicySchema.safeParse(bad).success).toBe(false);
  });
});