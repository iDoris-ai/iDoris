import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { loadComponents } from "../src/registry.js";

const fixtures = join(dirname(fileURLToPath(import.meta.url)), "fixtures");

describe("loadComponents", () => {
  it("loads and validates a good component card", () => {
    const reg = loadComponents(join(fixtures, "good"));
    expect(reg).toHaveLength(1);
    expect(reg[0]?.card.provider.id).toBe("mock");
  });
  it("throws (startup fails) on a card missing a mandatory policy field", () => {
    expect(() => loadComponents(join(fixtures, "bad"))).toThrow();
  });
});
