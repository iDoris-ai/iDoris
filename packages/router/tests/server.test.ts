import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import { HealthTracker } from "../src/health.js";
import { startRouter, type Router } from "../src/server.js";

const fixtures = join(dirname(fileURLToPath(import.meta.url)), "fixtures", "good");
let running: Router | undefined;

afterEach(async () => {
  if (running) {
    await running.close();
    running = undefined;
  }
});

const url = (r: Router, path: string): string => "http://127.0.0.1:" + r.port + path;

describe("startRouter", () => {
  it("binds loopback only and serves /v1/models with data", async () => {
    running = await startRouter({ componentsDir: fixtures });
    expect(running.host).toBe("127.0.0.1");
    const res = await fetch(url(running, "/v1/models"));
    expect(res.status).toBe(200);
    const body = (await res.json()) as { data: unknown[] };
    expect(Array.isArray(body.data)).toBe(true);
    expect(body.data.length).toBeGreaterThan(0);
  });
  it("serves /health", async () => {
    running = await startRouter({ componentsDir: fixtures });
    const body = (await (await fetch(url(running, "/health"))).json()) as { status: string };
    expect(body.status).toBe("ok");
  });
  it("records health for a backend", async () => {
    const health = new HealthTracker();
    running = await startRouter({ componentsDir: fixtures, health });
    await fetch(url(running, "/v1/models"));
    expect(typeof health.snapshot().mock).toBe("object");
  });
});
