import { describe, expect, it } from "vitest";
import { providerDescriptorSchema } from "../src/provider.js";

const valid = {
  id: "ornith-local",
  family: "idoris",
  tier: "local",
  capabilities: ["chat", "reasoning"],
  privacy_class: "local_only",
  cost: { input_per_m: 0, output_per_m: 0 },
  locality: "loopback",
};

describe("ProviderDescriptor", () => {
  it("accepts a valid descriptor", () => {
    expect(providerDescriptorSchema.safeParse(valid).success).toBe(true);
  });
  it("rejects a descriptor missing a required field (locality)", () => {
    const { locality: _omitted, ...missing } = valid;
    const res = providerDescriptorSchema.safeParse(missing);
    expect(res.success).toBe(false);
    if (!res.success) {
      expect(res.error.issues.some((i) => i.path.join(".") === "locality")).toBe(true);
    }
  });
  it("rejects empty capabilities", () => {
    expect(providerDescriptorSchema.safeParse({ ...valid, capabilities: [] }).success).toBe(false);
  });
});
