import { describe, expect, it } from "vitest";
import { ComponentCardPolicyError, validateComponentCard } from "../../src/validate.js";

/** 合法基线；每条反例从它派生，只破坏一条规则。 */
const base = () => ({
  provider: {
    id: "omlx-local",
    family: "local",
    tier: "local",
    capabilities: ["chat"],
    privacy_class: "local_only",
    cost: { input_per_m: 0, output_per_m: 0 },
    locality: "loopback",
  },
  form: "http_service",
  endpoint: "http://127.0.0.1:8088/v1",
  version_pin: "omlx@0.6.4",
  privacy_class: "local_only",
  allowed_egress: ["loopback"],
  fallback_policy: "fail_closed",
  fail_closed: true,
  load_policy: { mode: "resident", keepalive: { pinned: true }, admission: "coexist" },
});

function expectCode(fn: () => unknown, code: string) {
  try {
    fn();
  } catch (err) {
    expect(err).toBeInstanceOf(ComponentCardPolicyError);
    expect((err as ComponentCardPolicyError).code).toBe(code);
    return;
  }
  throw new Error("expected ComponentCardPolicyError(" + code + ") but nothing was thrown");
}

describe("validateComponentCard - 合法基线", () => {
  it("accepts the base card", () => {
    expect(validateComponentCard(base()).provider.tier).toBe("local");
  });
  it("positive control: privacy_class=any + fail_closed=false", () => {
    const card = base();
    card.provider.privacy_class = "any";
    card.privacy_class = "any";
    card.fail_closed = false;
    card.load_policy.mode = "on_demand";
    (card.load_policy as { keepalive: unknown }).keepalive = { idle_ttl_s: 300 };
    expect(validateComponentCard(card).fail_closed).toBe(false);
  });
  it("positive control: privacy_class=any + internet egress", () => {
    const card = base();
    card.provider.privacy_class = "any";
    card.privacy_class = "any";
    card.fail_closed = false;
    (card as { allowed_egress: string[] }).allowed_egress = ["internet"];
    (card.load_policy as { mode: string }).mode = "on_demand";
    (card.load_policy as { keepalive: unknown }).keepalive = { idle_ttl_s: 300 };
    expect(validateComponentCard(card).privacy_class).toBe("any");
  });
  it("positive control: local_only + egress none is allowed", () => {
    const card = base();
    (card as { allowed_egress: string[] }).allowed_egress = ["none"];
    expect(validateComponentCard(card).allowed_egress).toEqual(["none"]);
  });
});

describe("validateComponentCard - 缺强制字段（每条各一）", () => {
  for (const field of ["privacy_class", "allowed_egress", "fallback_policy", "fail_closed", "version_pin"]) {
    it("rejects a card missing " + field, () => {
      const card = base() as Record<string, unknown>;
      delete card[field];
      expectCode(() => validateComponentCard(card), "MISSING_POLICY_FIELD");
    });
  }
});

describe("validateComponentCard - 交叉规则（每条带反例）", () => {
  it("rejects local_only without fail_closed=true", () => {
    expectCode(() => validateComponentCard({ ...base(), fail_closed: false }), "LOCAL_ONLY_REQUIRES_FAIL_CLOSED");
  });
  it("rejects a card privacy_class broader than the provider's", () => {
    const card = base();
    card.privacy_class = "any";
    (card as { allowed_egress: string[] }).allowed_egress = ["internet"];
    card.fail_closed = true;
    expectCode(() => validateComponentCard(card), "PRIVACY_CLASS_MISMATCH");
  });
  it("rejects tier=local with locality=remote", () => {
    const card = base();
    card.provider.locality = "remote";
    expectCode(() => validateComponentCard(card), "LOCAL_TIER_CANNOT_BE_REMOTE_LOCALITY");
  });
  it("rejects a tier=local card without load_policy", () => {
    const card = base() as Record<string, unknown>;
    delete card.load_policy;
    expectCode(() => validateComponentCard(card), "LOCAL_TIER_REQUIRES_LOAD_POLICY");
  });
  it("rejects resident without pinned=true", () => {
    const card = base();
    (card.load_policy as { keepalive: unknown }).keepalive = { idle_ttl_s: 300 };
    expectCode(() => validateComponentCard(card), "LOAD_POLICY_MODE_KEEPALIVE_MISMATCH");
  });
  it("rejects on_demand with pinned=true", () => {
    const card = base();
    (card.load_policy as { mode: string }).mode = "on_demand";
    expectCode(() => validateComponentCard(card), "LOAD_POLICY_MODE_KEEPALIVE_MISMATCH");
  });
  it("rejects none mixed with other egress", () => {
    const card = base();
    card.provider.privacy_class = "any";
    card.privacy_class = "any";
    (card as { allowed_egress: string[] }).allowed_egress = ["none", "internet"];
    expectCode(() => validateComponentCard(card), "EGRESS_NONE_MUST_BE_EXCLUSIVE");
  });
  it("rejects local_only with internet egress", () => {
    expectCode(
      () => validateComponentCard({ ...base(), allowed_egress: ["loopback", "internet"] }),
      "LOCAL_ONLY_CANNOT_ALLOW_INTERNET",
    );
  });
  it("rejects local_only with lan egress", () => {
    expectCode(() => validateComponentCard({ ...base(), allowed_egress: ["lan"] }), "LOCAL_ONLY_EGRESS_MUST_BE_LOOPBACK");
  });
});

describe("validateComponentCard - 结构非法", () => {
  it("rejects a non-object", () => {
    expectCode(() => validateComponentCard("nope"), "INVALID_SCHEMA");
  });
  it("accepts extensions (06 §10.7)", () => {
    const card = { ...base(), extensions: { "provider.anthropic.prompt_cache": { ttl: 5 } } };
    expect(validateComponentCard(card).provider.tier).toBe("local");
  });
});
