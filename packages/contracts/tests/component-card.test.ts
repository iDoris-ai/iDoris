import { describe, expect, it } from "vitest";
import { componentCardSchema } from "../src/component-card.js";

const valid = {
  provider: {
    id: "omlx-local",
    family: "local",
    tier: "local",
    capabilities: ["chat", "embedding"],
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
};

describe("ComponentCard", () => {
  it("accepts a valid card", () => {
    expect(componentCardSchema.safeParse(valid).success).toBe(true);
  });
  it("rejects a card missing version_pin", () => {
    const { version_pin: _omitted, ...missing } = valid;
    const res = componentCardSchema.safeParse(missing);
    expect(res.success).toBe(false);
    if (!res.success) {
      expect(res.error.issues.some((i) => i.path.join(".") === "version_pin")).toBe(true);
    }
  });
});
