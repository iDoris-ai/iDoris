import { taskProfileSchema, validateComponentCard, type ComponentCard } from "@idoris/contracts";
import { describe, expect, it } from "vitest";
import { MockBackend } from "@idoris/adapters";
import { dispatch, type EgressCounter } from "../src/dispatch.js";
import { decide, loadRoutingPolicy } from "../src/policy.js";
import type { Registered } from "../src/registry.js";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const policy = loadRoutingPolicy(join(repoRoot, "config", "routing-policy.yaml"));

const remoteCard = validateComponentCard({
  provider: { id: "remote", family: "openai", tier: "remote", capabilities: ["chat", "vision"], privacy_class: "any", cost: { input_per_m: 1, output_per_m: 1 }, locality: "remote" },
  form: "http_service", endpoint: "https://api.example.com", version_pin: "remote@1",
  privacy_class: "any", allowed_egress: ["internet"], fallback_policy: "next_in_chain", fail_closed: false,
}) as ComponentCard;

const localCard = validateComponentCard({
  provider: { id: "local", family: "local", tier: "local", capabilities: ["chat"], privacy_class: "local_only", cost: { input_per_m: 0, output_per_m: 0 }, locality: "loopback" },
  form: "http_service", endpoint: "mock://local", version_pin: "local@1",
  privacy_class: "local_only", allowed_egress: ["loopback"], fallback_policy: "fail_closed", fail_closed: true,
  load_policy: { mode: "resident", keepalive: { pinned: true }, admission: "coexist" },
}) as ComponentCard;

const remote: Registered = { card: remoteCard, backend: new MockBackend({ memoryMaxGb: 16, models: [] }) };
const local: Registered = { card: localCard, backend: new MockBackend({ memoryMaxGb: 16, models: [{ id: "local", memoryGb: 2 }] }) };
const prof = (p: Record<string, unknown>) => taskProfileSchema.parse(p);

describe("dispatch - fail-closed 隐私门禁", () => {
  it("20 local_only requests with no local provider => 20x503 and ZERO egress", () => {
    const egress: EgressCounter = { count: 0 };
    let blocked = 0;
    for (let i = 0; i < 20; i += 1) {
      const profile = prof({ privacy: "local_only" });
      const outcome = dispatch(profile, decide(policy, profile), [remote], egress);
      if (outcome.status === 503) blocked += 1;
    }
    expect(blocked).toBe(20);
    expect(egress.count).toBe(0);
  });

  it("positive control: a local provider serves local_only", () => {
    const egress: EgressCounter = { count: 0 };
    const profile = prof({ privacy: "local_only" });
    const outcome = dispatch(profile, decide(policy, profile), [local, remote], egress);
    expect(outcome.status).toBe(200);
    expect(outcome.providerId).toBe("local");
    expect(egress.count).toBe(0);
  });

  it("egress counter positive control: remote dispatch DOES increment it", () => {
    const egress: EgressCounter = { count: 0 };
    const profile = prof({ privacy: "any", complexity: "complex" });
    const outcome = dispatch(profile, decide(policy, profile), [remote], egress);
    expect(outcome.status).toBe(200);
    expect(egress.count).toBe(1);
  });

  it("local_only never selects a remote provider even if it is the only one", () => {
    const egress: EgressCounter = { count: 0 };
    const profile = prof({ privacy: "local_only", intent: "banner" });
    const outcome = dispatch(profile, decide(policy, profile), [remote], egress);
    expect(outcome.status).toBe(503);
    expect(outcome.providerId).toBeUndefined();
    expect(egress.count).toBe(0);
  });
});
