import { chmodSync, readFileSync, writeFileSync } from "node:fs";
import { createConnection } from "node:net";
import { afterEach, expect, it } from "vitest";
import {
  ConformanceStartupError,
  routingPolicyFixturePath,
  spawnConformanceServer,
  type RunningServer,
} from "../src/harness.js";
import {
  makeSubscriptionFixture,
  type SubscriptionFixture,
} from "../src/subscription-fixture.js";

let fixture: SubscriptionFixture | undefined;
let server: RunningServer | undefined;

afterEach(async () => {
  await server?.stop();
  server = undefined;
  fixture?.cleanup();
  fixture = undefined;
});

function offEnv(f: SubscriptionFixture): NodeJS.ProcessEnv {
  const env = { ...f.env };
  delete env.IDORIS_ENABLE_SUBSCRIPTION;
  delete env.IDORIS_SUBSCRIPTION_SANDBOX;
  delete env.IDORIS_SUBSCRIPTION_CLI;
  return env;
}

async function start(env: NodeJS.ProcessEnv): Promise<RunningServer> {
  if (fixture === undefined) throw new Error("fixture not initialized");
  return await spawnConformanceServer({
    componentsDir: fixture.componentsDir,
    routingPolicyPath: routingPolicyFixturePath,
    pathPrepend: fixture.pathPrepend,
    env: { ...env, IDORIS_DB_PATH: fixture.markerPath + ".sqlite3" },
  });
}

async function healthComponents(s: RunningServer): Promise<number> {
  const response = await fetch(s.baseUrl + "/health");
  expect(response.status).toBe(200);
  const body = (await response.json()) as { components: number };
  return body.components;
}

async function postSubscription(
  s: RunningServer,
  options: { privacy?: "any" | "local_only"; stream?: boolean } = {},
): Promise<Response> {
  return await fetch(s.baseUrl + "/v1/chat/completions", {
    method: "POST",
    headers: {
      "content-type": "application/json",
      "x-idoris-privacy": options.privacy ?? "any",
      "x-idoris-complexity": "complex",
    },
    body: JSON.stringify({
      model: "claude-subscription",
      stream: options.stream ?? false,
      messages: [{ role: "user", content: "hello-subscription" }],
    }),
  });
}

async function expectStartupFailure(env: NodeJS.ProcessEnv): Promise<void> {
  if (fixture === undefined) throw new Error("fixture not initialized");
  const failure = await spawnConformanceServer({
    componentsDir: fixture.componentsDir,
    routingPolicyPath: routingPolicyFixturePath,
    pathPrepend: fixture.pathPrepend,
    env: { ...env, IDORIS_DB_PATH: fixture.markerPath + ".sqlite3" },
    healthTimeoutMs: 3_000,
  }).then(
    async (running) => {
      await running.stop();
      return undefined;
    },
    (error: unknown) => error,
  );
  expect(failure).toBeInstanceOf(ConformanceStartupError);
  expect((failure as ConformanceStartupError).kind).toBe("exited_early");
  expect(fixture.readMarker()).toBe("");
}

it("subscription is default-off and explicit disable remains zero-spawn", async () => {
  fixture = makeSubscriptionFixture("claude");
  server = await start(offEnv(fixture));
  expect(await healthComponents(server)).toBe(0);
  expect(fixture.readMarker()).toBe("");
  await server.stop();
  server = undefined;

  server = await start({ ...fixture.env, IDORIS_DISABLE_SUBSCRIPTION: "1" });
  expect(await healthComponents(server)).toBe(0);
  expect(fixture.readMarker()).toBe("");
});

it("non-personal deploy mode hard-refuses even when subscription is explicitly enabled", async () => {
  fixture = makeSubscriptionFixture("claude");
  await expectStartupFailure({ ...fixture.env, IDORIS_DEPLOY_MODE: "tenant" });
});

it("explicit enable without the declared sandbox fails closed", async () => {
  fixture = makeSubscriptionFixture("claude");
  const env = { ...fixture.env };
  delete env.IDORIS_SUBSCRIPTION_SANDBOX;
  await expectStartupFailure(env);
});

it("loopback enabled subscription returns buffered OpenAI JSON and remote served-locality", async () => {
  fixture = makeSubscriptionFixture("claude");
  server = await start(fixture.env);
  const response = await postSubscription(server, { stream: true });
  expect(response.status).toBe(200);
  expect(response.headers.get("x-idoris-served-locality")).toBe("remote");
  expect(response.headers.get("x-idoris-record-id")).toBeTruthy();
  expect(response.headers.get("content-type")).toContain("application/json");
  const body = (await response.json()) as {
    object: string;
    model: string;
    choices: Array<{ message: { content: string } }>;
  };
  expect(body.object).toBe("chat.completion");
  expect(body.model).toBe("claude-subscription");
  expect(body.choices[0]?.message.content).toMatch(/^fixture:/);
  expect(fixture.readMarker().trim().split("\n")).toHaveLength(1);
});

it("local_only rejects before spawning the subscription CLI", async () => {
  fixture = makeSubscriptionFixture("claude");
  server = await start(fixture.env);
  const response = await postSubscription(server, { privacy: "local_only" });
  expect(response.status).toBe(503);
  expect(fixture.readMarker()).toBe("");
});

async function waitForMarker(f: SubscriptionFixture, pattern: RegExp): Promise<string> {
  const deadline = Date.now() + 5_000;
  for (;;) {
    const marker = f.readMarker();
    if (pattern.test(marker)) return marker;
    if (Date.now() >= deadline) throw new Error("timed out waiting for subscription marker: " + pattern);
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
}

it("closing the HTTP socket after full request body cancels the running subscription CLI", async () => {
  fixture = makeSubscriptionFixture("claude");
  const claude = fixture.binDir + "/claude";
  writeFileSync(
    claude,
    `#!/bin/sh\nset -eu\nmarker="\${SUBSCRIPTION_FIXTURE_MARKER:?}"\npgid="$(ps -o pgid= -p $$ | tr -d ' ')"\ntrap 'printf "term:%s\\n" "$$" >> "$marker"; exit 0' TERM\nprintf 'spawn:%s:%s\\n' "$$" "$pgid" >> "$marker"\nwhile :; do sleep 1; done\n`,
    "utf8",
  );
  chmodSync(claude, 0o755);
  server = await start(fixture.env);

  const body = JSON.stringify({
    model: "claude-subscription",
    messages: [{ role: "user", content: "disconnect-me" }],
  });
  const socket = createConnection({ host: "127.0.0.1", port: server.port });
  await new Promise<void>((resolve, reject) => {
    socket.once("connect", resolve);
    socket.once("error", reject);
  });
  socket.write(
    "POST /v1/chat/completions HTTP/1.1\r\n" +
      "Host: localhost\r\n" +
      "Content-Type: application/json\r\n" +
      "X-iDoris-Privacy: any\r\n" +
      "X-iDoris-Complexity: complex\r\n" +
      "Content-Length: " + String(Buffer.byteLength(body)) + "\r\n\r\n" +
      body,
  );
  await waitForMarker(fixture, /^spawn:/m);
  socket.destroy();
  const marker = await waitForMarker(fixture, /^term:/m);
  expect(marker.match(/^spawn:/gm)).toHaveLength(1);
  expect(marker.match(/^term:/gm)).toHaveLength(1);
  expect(readFileSync(fixture.markerPath, "utf8")).not.toContain("SECRET");
});
