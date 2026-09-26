import { describe, expect, it } from "vitest";
import { EvictionLock, OomError } from "../src/evict-lock.js";

const sleep = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));

describe("EvictionLock (T1.3.5)", () => {
  it("serializes 10 concurrent evict_to_load requests with no deadlock", async () => {
    const lock = new EvictionLock();
    const order: number[] = [];
    await Promise.all(
      Array.from({ length: 10 }, (_, i) =>
        lock.withLock("backend-a", 10_000, async () => {
          await sleep(5);
          order.push(i);
          return i;
        }),
      ),
    );
    expect(order).toHaveLength(10);
    expect(new Set(order).size).toBe(10);
  });

  it("returns OomError instead of waiting forever when the lock is held", async () => {
    const lock = new EvictionLock();
    const held = lock.withLock("backend-b", 10_000, async () => {
      await sleep(200);
    });
    await sleep(10);
    await expect(lock.withLock("backend-b", 30, async () => undefined)).rejects.toBeInstanceOf(OomError);
    await held;
  });

  it("locks are per-backend: one held lock does not block another key", async () => {
    const lock = new EvictionLock();
    const a = lock.withLock("a", 10_000, async () => {
      await sleep(200);
      return "a";
    });
    await sleep(10);
    expect(await lock.withLock("b", 50, async () => "b")).toBe("b");
    expect(await a).toBe("a");
  });
});
