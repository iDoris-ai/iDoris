import { describe, expect, it } from "vitest";
import { decisionReason, MissingReasonError, reasonHeader, requireReason, REASON_KINDS } from "../src/reason.js";

describe("decision reason (T1.5.4)", () => {
  it("produces a structured reason for each of the four kinds", () => {
    for (const kind of REASON_KINDS) {
      const r = decisionReason(kind, "why " + kind);
      expect(r.kind).toBe(kind);
      expect(requireReason(r)).toEqual(r);
    }
  });
  it("rejects an empty reason (a bare routed is not acceptable)", () => {
    expect(() => decisionReason("intent_match", "   ")).toThrow(MissingReasonError);
    expect(() => requireReason(undefined)).toThrow(MissingReasonError);
    expect(() => requireReason({ kind: "budget", detail: "" })).toThrow(MissingReasonError);
  });
  it("renders a non-empty response header", () => {
    const h = reasonHeader(decisionReason("privacy_enforced", "local_only kept local"));
    expect(h["x-idoris-reason"]).toContain("privacy_enforced");
    expect(h["x-idoris-reason"]).toContain("local_only kept local");
  });
});
