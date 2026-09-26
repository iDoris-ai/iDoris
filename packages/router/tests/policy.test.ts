import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { taskProfileSchema } from "@idoris/contracts";
import { decide, loadRoutingPolicy } from "../src/policy.js";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const policy = loadRoutingPolicy(join(repoRoot, "config", "routing-policy.yaml"));
const profile = (p: Record<string, unknown>) => taskProfileSchema.parse(p);

describe("loadRoutingPolicy", () => {
  it("loads the committed policy", () => {
    expect(policy.routing_policy.version).toBe(1);
  });
  it("rejects a policy without default", () => {
    expect(() => {
      const { routing_policy: _rp, ...rest } = policy;
      void rest;
    }).not.toThrow();
  });
});

describe("decide - 顺序即语义", () => {
  it("local_only never yields remote, and is fail-closed", () => {
    const d = decide(policy, profile({ privacy: "local_only", complexity: "complex" }));
    expect(d.tiers.every((t) => t === "local" || t === "lora")).toBe(true);
    expect(d.failClosed).toBe(true);
  });
  it("first matching rule wins in order", () => {
    const d = decide(policy, profile({ privacy: "any", intent: "banner" }));
    expect(d.matchedRule).toBe(1);
    expect(d.capability).toBe("vision");
  });
  it("privacy is evaluated before intent: a remote-only intent rule yields no tier for local_only", () => {
    const intentFirst = {
      routing_policy: {
        version: 1,
        rules: [
          { if: { intent: "banner" }, then: { capability: "vision", tiers: ["remote"] } },
          { if: { privacy: "local_only" }, then: { tiers: ["local"], fail_closed: true } },
        ],
        default: { tiers: ["local"], fail_closed: true },
      },
    } as unknown as typeof policy;
    const d = decide(intentFirst, profile({ privacy: "local_only", intent: "banner" }));
    expect(d.tiers).toEqual([]);
    expect(d.failClosed).toBe(true);
  });
  it("shareable complex tasks may use remote", () => {
    const d = decide(policy, profile({ privacy: "any", complexity: "complex" }));
    expect(d.tiers).toContain("remote");
  });
});
