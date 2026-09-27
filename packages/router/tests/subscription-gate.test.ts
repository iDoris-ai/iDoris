import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { MockBackend, SUBSCRIPTION_SANDBOX_PROFILE_ID, SubscriptionRelay } from "@idoris/adapters";
import { taskProfileSchema, validateComponentCard } from "@idoris/contracts";
import { parse } from "yaml";
import { describe, expect, it } from "vitest";
import { dispatch, type EgressCounter } from "../src/dispatch.js";
import { decide, loadRoutingPolicy } from "../src/policy.js";
import { ComponentRegistrationError, loadComponents } from "../src/registry.js";
import { startRouter } from "../src/server.js";

const testsDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(testsDir, "..", "..", "..");
const subDir = join(testsDir, "fixtures", "subscription");
const goodDir = join(testsDir, "fixtures", "good");

const personal = { IDORIS_DEPLOY_MODE: "personal" };
const enabled = {
  ...personal,
  IDORIS_ENABLE_SUBSCRIPTION: "1",
  IDORIS_SUBSCRIPTION_SANDBOX: SUBSCRIPTION_SANDBOX_PROFILE_ID,
};

describe("registry - subscription provider startup gate (T1.4.2)", () => {
  it("refuses to register in tenant / community / city (startup must exit non-zero)", () => {
    for (const mode of ["tenant", "community", "city"]) {
      let thrown: unknown;
      try {
        loadComponents(subDir, { env: { IDORIS_DEPLOY_MODE: mode, IDORIS_DISABLE_SUBSCRIPTION: "1" } });
      } catch (err) {
        thrown = err;
      }
      expect(thrown).toBeInstanceOf(ComponentRegistrationError);
      expect((thrown as Error).message).toContain("deploy_mode=" + mode);
    }
  });

  it("startRouter rejects for a non-personal deploy mode", async () => {
    await expect(startRouter({ componentsDir: subDir, env: { IDORIS_DEPLOY_MODE: "community" } })).rejects.toBeInstanceOf(
      ComponentRegistrationError,
    );
  });

  it("skips (does not crash) when explicitly disabled", () => {
    const reg = loadComponents(subDir, { env: { ...personal, IDORIS_DISABLE_SUBSCRIPTION: "1" } });
    expect(reg).toHaveLength(0);
  });

  it("skips (fail-closed default) when not explicitly enabled", () => {
    const reg = loadComponents(subDir, { env: personal });
    expect(reg).toHaveLength(0);
  });

  it("refuses enable-without-sandbox instead of running unsandboxed", () => {
    expect(() => loadComponents(subDir, { env: { ...personal, IDORIS_ENABLE_SUBSCRIPTION: "1" } })).toThrow(
      /IDORIS_SUBSCRIPTION_SANDBOX/,
    );
  });

  it("registers only with personal + enable + sandbox", () => {
    const reg = loadComponents(subDir, { env: enabled });
    expect(reg).toHaveLength(1);
    expect(reg[0]?.backend).toBeInstanceOf(SubscriptionRelay);
    (reg[0]?.backend as SubscriptionRelay).dispose();
  });

  it("a disabled subscription never breaks non-subscription components (T1.4.3)", () => {
    const reg = loadComponents(goodDir, { env: { ...enabled, IDORIS_DISABLE_SUBSCRIPTION: "1" } });
    expect(reg).toHaveLength(1);
    expect(reg[0]?.card.provider.id).toBe("mock");
  });
});

describe("routing policy - subscription is never in the default chain (T1.4.3)", () => {
  const policy = loadRoutingPolicy(join(repoRoot, "config", "routing-policy.yaml"));
  const subscriptionCard = validateComponentCard(parse(readFileSync(join(subDir, "subscription.yaml"), "utf8")));

  it("the committed default chain is local-only", () => {
    expect(policy.routing_policy.default.tiers).toEqual(["local"]);
    expect(policy.routing_policy.default.tiers).not.toContain("remote");
  });

  it("a default-profile request never selects the subscription provider", () => {
    const profile = taskProfileSchema.parse({ privacy: "any", intent: "chat", complexity: "simple" });
    const decision = decide(policy, profile);
    expect(decision.tiers).toEqual(["local"]);
    const egress: EgressCounter = { count: 0 };
    const registered = [{ card: subscriptionCard, backend: new MockBackend({ memoryMaxGb: 1, models: [] }) }];
    const outcome = dispatch(profile, decision, registered, egress);
    expect(outcome.status).toBe(503);
    expect(outcome.providerId).toBeUndefined();
    expect(egress.count).toBe(0);
  });
});
