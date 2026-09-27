import { describe, expect, it } from "vitest";
import {
  EgressGuardError,
  assertSubscriptionSource,
  isAllowedSubscriptionSource,
  isLoopbackAddress,
  isNonPersonalDeployMode,
  isTailscaleAddress,
  subscriptionStartupGate,
} from "../src/egress-guard.js";

describe("isLoopbackAddress", () => {
  it("accepts loopback forms", () => {
    for (const a of ["127.0.0.1", "127.5.5.5", "::1", "::ffff:127.0.0.1", "localhost"]) {
      expect(isLoopbackAddress(a)).toBe(true);
    }
  });
  it("rejects everything else", () => {
    for (const a of ["10.0.0.1", "192.168.1.5", "100.64.0.5", "8.8.8.8", "::ffff:10.0.0.1", "", undefined]) {
      expect(isLoopbackAddress(a)).toBe(false);
    }
  });
});

describe("isTailscaleAddress", () => {
  it("matches 100.64.0.0/10 only", () => {
    expect(isTailscaleAddress("100.64.0.1")).toBe(true);
    expect(isTailscaleAddress("100.127.255.254")).toBe(true);
    expect(isTailscaleAddress("100.63.0.1")).toBe(false);
    expect(isTailscaleAddress("100.128.0.1")).toBe(false);
    expect(isTailscaleAddress("10.0.0.1")).toBe(false);
  });
});

describe("subscription source gate", () => {
  it("allows loopback", () => {
    expect(isAllowedSubscriptionSource("127.0.0.1", {})).toBe(true);
    expect(() => assertSubscriptionSource("::1", {})).not.toThrow();
  });
  it("rejects non-loopback by default", () => {
    expect(isAllowedSubscriptionSource("100.64.0.5", {})).toBe(false);
    expect(() => assertSubscriptionSource("10.0.0.1", {})).toThrowError(EgressGuardError);
  });
  it("allows Tailscale only when explicitly enabled", () => {
    expect(isAllowedSubscriptionSource("100.64.0.5", { IDORIS_SUBSCRIPTION_ALLOW_TAILSCALE: "1" })).toBe(true);
    expect(() => assertSubscriptionSource("100.64.0.5", { IDORIS_SUBSCRIPTION_ALLOW_TAILSCALE: "1" })).not.toThrow();
    expect(isAllowedSubscriptionSource("100.128.0.5", { IDORIS_SUBSCRIPTION_ALLOW_TAILSCALE: "1" })).toBe(false);
  });
  it("carries a machine-checkable error code", () => {
    try {
      assertSubscriptionSource("10.0.0.1", {});
      throw new Error("should have thrown");
    } catch (err) {
      expect((err as { code: string }).code).toBe("SUBSCRIPTION_SOURCE_NOT_LOOPBACK");
    }
  });
});

describe("subscriptionStartupGate", () => {
  it("refuses tenant / community / city even when disabled", () => {
    for (const mode of ["tenant", "community", "city"]) {
      const d = subscriptionStartupGate("subscription", { IDORIS_DEPLOY_MODE: mode, IDORIS_DISABLE_SUBSCRIPTION: "1" });
      expect(d.action).toBe("refuse");
      expect(d.reason).toContain("deploy_mode=" + mode);
    }
  });
  it("skips when disabled or not enabled", () => {
    expect(subscriptionStartupGate("subscription", { IDORIS_DEPLOY_MODE: "personal", IDORIS_DISABLE_SUBSCRIPTION: "1" }).action).toBe("skip");
    expect(subscriptionStartupGate("subscription", { IDORIS_DEPLOY_MODE: "personal" }).action).toBe("skip");
  });
  it("registers only with personal + enable", () => {
    expect(subscriptionStartupGate("subscription", { IDORIS_DEPLOY_MODE: "personal", IDORIS_ENABLE_SUBSCRIPTION: "1" }).action).toBe(
      "register",
    );
  });
  it("treats unknown deploy modes as non-personal (fail-closed)", () => {
    expect(isNonPersonalDeployMode("tenant")).toBe(true);
    expect(isNonPersonalDeployMode("personal")).toBe(false);
    expect(subscriptionStartupGate("subscription", { IDORIS_DEPLOY_MODE: "somewhere" }).action).toBe("refuse");
  });
  it("ignores non-subscription providers", () => {
    expect(subscriptionStartupGate("mock", { IDORIS_DEPLOY_MODE: "tenant" }).action).toBe("register");
  });
});
