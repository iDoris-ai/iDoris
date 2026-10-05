import { join } from "node:path";
import { afterEach, expect, it, vi } from "vitest";
import { ConformanceStartupError, spawnConformanceServer } from "../src/harness.js";
import {
  makeSubscriptionFixture,
  probeSubscriptionFixture,
  type SubscriptionFixture,
} from "../src/subscription-fixture.js";

let fixture: SubscriptionFixture | undefined;
afterEach(() => {
  vi.unstubAllEnvs();
  fixture?.cleanup();
  fixture = undefined;
});

it("controlled fake CLI executes once and exposes pid/pgid marker", async () => {
  fixture = makeSubscriptionFixture("claude");
  expect(await probeSubscriptionFixture(fixture, "claude")).toBe("fixture:probe");
  const lines = fixture.readMarker().trim().split("\n");
  expect(lines).toHaveLength(1);
  expect(lines[0]).toMatch(/^spawn:\d+:\d+$/);
});

it("wrong fake CLI path fails instead of silently falling back", async () => {
  fixture = makeSubscriptionFixture("claude");
  await expect(probeSubscriptionFixture(
    { ...fixture, binDir: join(fixture.binDir, "missing") },
    "claude",
  )).rejects.toBeInstanceOf(Error);
  expect(fixture.readMarker()).toBe("");
});


it("harness strips inherited IDORIS env and only re-adds case-scoped values", async () => {
  fixture = makeSubscriptionFixture("claude");
  vi.stubEnv("IDORIS_HOST_SENTINEL", "MUST_NOT_LEAK");
  vi.stubEnv("IDORIS_CONFORMANCE_ARGV", JSON.stringify([process.execPath, "-e", `
    require("node:http").createServer((req, res) => {
      res.setHeader("content-type", "application/json");
      res.end(JSON.stringify({
        leaked: process.env.IDORIS_HOST_SENTINEL ?? null,
        enabled: process.env.IDORIS_ENABLE_SUBSCRIPTION ?? null,
      }));
    }).listen(Number(process.env.IDORIS_PORT), "127.0.0.1");
  `]));
  const server = await spawnConformanceServer({
    componentsDir: fixture.componentsDir,
    pathPrepend: fixture.pathPrepend,
    env: fixture.env,
  });
  try {
    expect(await (await fetch(server.baseUrl + "/health")).json()).toEqual({
      leaked: null,
      enabled: "1",
    });
  } finally {
    await server.stop();
  }
});

it("wrong tested binary fails the harness as a structured startup error", async () => {
  fixture = makeSubscriptionFixture("claude");
  vi.stubEnv("IDORIS_CONFORMANCE_ARGV", JSON.stringify([join(fixture.binDir, "does-not-exist")]));
  const error = await spawnConformanceServer({
    componentsDir: fixture.componentsDir,
    pathPrepend: fixture.pathPrepend,
    env: fixture.env,
    healthTimeoutMs: 1_000,
  }).then(
    async (server) => { await server.stop(); return undefined; },
    (failure: unknown) => failure,
  );
  expect(error).toBeInstanceOf(ConformanceStartupError);
  expect(error).toMatchObject({ kind: "spawn_error" });
  expect(fixture.readMarker()).toBe("");
});
