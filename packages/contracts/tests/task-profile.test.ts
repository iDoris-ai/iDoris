import { describe, expect, it } from "vitest";
import { taskProfileSchema } from "../src/task-profile.js";

describe("TaskProfile", () => {
  it("accepts a valid profile", () => {
    expect(taskProfileSchema.safeParse({ privacy: "any", intent: "coding", complexity: "complex", capabilities: ["coding"] }).success).toBe(true);
  });
  it("applies the conservative default privacy=local_only when omitted", () => {
    const res = taskProfileSchema.parse({});
    expect(res.privacy).toBe("local_only");
    expect(res.intent).toBe("chat");
    expect(res.complexity).toBe("simple");
    expect(res.capabilities).toEqual(["chat"]);
  });
  it("rejects an unknown privacy value", () => {
    expect(taskProfileSchema.safeParse({ privacy: "secret" }).success).toBe(false);
  });
});
