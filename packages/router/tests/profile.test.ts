import { describe, expect, it } from "vitest";
import { parseProfile, ProfileError } from "../src/profile.js";

describe("parseProfile", () => {
  it("defaults privacy to local_only (conservative)", () => {
    expect(parseProfile({}, "personal").profile.privacy).toBe("local_only");
  });
  it("rejects an illegal privacy value with 400", () => {
    try {
      parseProfile({ "x-idoris-privacy": "secret" }, "personal");
      throw new Error("should have thrown");
    } catch (err) {
      expect(err).toBeInstanceOf(ProfileError);
      expect((err as ProfileError).status).toBe(400);
      expect((err as ProfileError).code).toBe("invalid_privacy");
    }
  });
  it("parses capabilities and fallback", () => {
    const r = parseProfile({ "x-idoris-capabilities": "vision, coding", "x-idoris-fallback": "next_in_chain" }, "personal");
    expect(r.profile.capabilities).toEqual(["vision", "coding"]);
    expect(r.profile.fallback).toBe("next_in_chain");
  });
  it("requires X-iDoris-Tenant in tenant mode (no default tenant)", () => {
    try {
      parseProfile({}, "tenant");
      throw new Error("should have thrown");
    } catch (err) {
      expect((err as ProfileError).code).toBe("tenant_missing");
      expect((err as ProfileError).status).toBe(400);
    }
  });
  it("accepts a tenant header in tenant mode", () => {
    expect(parseProfile({ "x-idoris-tenant": "acme" }, "tenant").tenantId).toBe("acme");
  });
});
