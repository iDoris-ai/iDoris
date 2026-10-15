import { describe, expect, it } from "vitest";
import { startFakeUpstream } from "../src/fake-upstream.js";
import { localComponent, makeComponentsDir, makeRoutingPolicyFile, remoteComponent } from "../src/fixtures.js";
import { spawnConformanceServer, type RunningServer } from "../src/harness.js";

const codingLocalPolicy = `routing_policy:
  version: 1
  rules:
    - if: { intent: coding }
      then: { tiers: [local], fail_closed: true }
  default: { tiers: [remote], fail_closed: true }
`;

function chatBody(model: string | undefined, prompt: string): string {
  const value: Record<string, unknown> = { messages: [{ role: "user", content: prompt }] };
  if (model !== undefined) value.model = model;
  return JSON.stringify(value);
}

describe("intent fallback wiring", () => {
  it.each([
    { model: "idoris/auto", explicitIntent: undefined, prompt: "fix this bug", expectedStatus: 200 },
    { model: "idoris/auto", explicitIntent: "chat", prompt: "fix this bug", expectedStatus: 503 },
    { model: "idoris/auto", explicitIntent: undefined, prompt: "tell me a joke", expectedStatus: 503 },
    { model: "idoris/daily", explicitIntent: undefined, prompt: "fix this bug", expectedStatus: 503 },
    { model: "idoris/fast", explicitIntent: undefined, prompt: "fix this bug", expectedStatus: 503 },
    { model: "idoris/deep", explicitIntent: undefined, prompt: "fix this bug", expectedStatus: 503 },
    { model: "idoris/vision", explicitIntent: undefined, prompt: "fix this bug", expectedStatus: 503 },
    { model: "idoris/embed", explicitIntent: undefined, prompt: "fix this bug", expectedStatus: 503 },
    { model: "idoris/rerank", explicitIntent: undefined, prompt: "fix this bug", expectedStatus: 503 },
    { model: "idoris/decide", explicitIntent: undefined, prompt: "fix this bug", expectedStatus: 503 },
    { model: "org/opaque-model", explicitIntent: undefined, prompt: "fix this bug", expectedStatus: 503 },
    { model: undefined, explicitIntent: undefined, prompt: "fix this bug", expectedStatus: 503 },
    { model: "idoris/vision", explicitIntent: "coding", prompt: "tell me a joke", expectedStatus: 200 },
  ])("model=$model intent=$explicitIntent prompt=$prompt => $expectedStatus", async ({ model, explicitIntent, prompt, expectedStatus }) => {
    const upstream = await startFakeUpstream();
    let server: RunningServer | undefined;
    try {
      const componentsDir = makeComponentsDir([localComponent(upstream.url)]);
      const routingPolicyPath = makeRoutingPolicyFile(codingLocalPolicy);
      server = await spawnConformanceServer({ componentsDir, routingPolicyPath });
      upstream.queueChat({ kind: "json", body: { choices: [{ message: { content: "intent-ok" } }] } });
      const headers: Record<string, string> = { "content-type": "application/json" };
      if (explicitIntent !== undefined) headers["x-idoris-intent"] = explicitIntent;
      const res = await fetch(server.baseUrl + "/v1/chat/completions", {
        method: "POST",
        headers,
        body: chatBody(model, prompt),
      });
      expect(res.status).toBe(expectedStatus);
      expect(upstream.chatCount()).toBe(expectedStatus === 200 ? 1 : 0);
      if (expectedStatus === 200) {
        expect((await res.json() as { choices: { message: { content: string } }[] }).choices[0]?.message.content).toBe("intent-ok");
      }
    } finally {
      await server?.stop();
      await upstream.close();
    }
  });

  it("非法 complexity header 仍是 400，且不打上游", async () => {
    const upstream = await startFakeUpstream();
    let server: RunningServer | undefined;
    try {
      server = await spawnConformanceServer({
        componentsDir: makeComponentsDir([localComponent(upstream.url)]),
        routingPolicyPath: makeRoutingPolicyFile(codingLocalPolicy),
      });
      const res = await fetch(server.baseUrl + "/v1/chat/completions", {
        method: "POST",
        headers: { "content-type": "application/json", "x-idoris-complexity": "ultra" },
        body: chatBody("idoris/auto", "fix this bug"),
      });
      expect(res.status).toBe(400);
      expect(((await res.json()) as { error: { type: string } }).error.type).toBe("invalid_header");
      expect(upstream.chatCount()).toBe(0);
    } finally {
      await server?.stop();
      await upstream.close();
    }
  });

  it("auto 声明 complex 后，识别出的 coding 不把复杂度改回 simple", async () => {
    const upstream = await startFakeUpstream();
    let server: RunningServer | undefined;
    try {
      server = await spawnConformanceServer({
        componentsDir: makeComponentsDir([localComponent(upstream.url)]),
        routingPolicyPath: makeRoutingPolicyFile(`routing_policy:
  version: 1
  rules:
    - if: { intent: coding, complexity: simple }
      then: { tiers: [local], fail_closed: true }
    - if: { complexity: complex }
      then: { tiers: [remote], fail_closed: true }
  default: { tiers: [remote], fail_closed: true }
`),
      });
      upstream.queueChat({ kind: "json", body: { choices: [{ message: { content: "simple-ok" } }] } });
      const simple = await fetch(server.baseUrl + "/v1/chat/completions", {
        method: "POST",
        headers: { "content-type": "application/json", "x-idoris-privacy": "any" },
        body: chatBody("idoris/auto", "fix this bug"),
      });
      expect(simple.status).toBe(200);
      expect(upstream.chatCount()).toBe(1);

      const complex = await fetch(server.baseUrl + "/v1/chat/completions", {
        method: "POST",
        headers: {
          "content-type": "application/json",
          "x-idoris-privacy": "any",
          "x-idoris-complexity": "complex",
        },
        body: chatBody("idoris/auto", "fix this bug"),
      });
      expect(complex.status).toBe(503);
      expect(upstream.chatCount()).toBe(1);
    } finally {
      await server?.stop();
      await upstream.close();
    }
  });

  it("三种模式下 local_only 都不会选中远程候选", async () => {
    const localUpstream = await startFakeUpstream();
    const remoteUpstream = await startFakeUpstream();
    let server: RunningServer | undefined;
    try {
      server = await spawnConformanceServer({
        componentsDir: makeComponentsDir([
          localComponent(localUpstream.url),
          remoteComponent(remoteUpstream.url, { id: "mock" }),
        ]),
        routingPolicyPath: makeRoutingPolicyFile(`routing_policy:
  version: 1
  rules:
    - if: { intent: coding }
      then: { tiers: [remote], fail_closed: true }
  default: { tiers: [local], fail_closed: true }
`),
      });
      localUpstream.queueChat({ kind: "json", body: { choices: [{ message: { content: "local-ok" } }] } });
      const chat = await fetch(server.baseUrl + "/v1/chat/completions", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: chatBody("idoris/daily", "fix this bug"),
      });
      expect(chat.status).toBe(200);
      expect(localUpstream.chatCount()).toBe(1);
      expect(remoteUpstream.chatCount()).toBe(0);

      const auto = await fetch(server.baseUrl + "/v1/chat/completions", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: chatBody("idoris/auto", "fix this bug"),
      });
      expect(auto.status).toBe(503);
      expect(((await auto.json()) as { error: { type: string } }).error.type).toBe("local_only_unavailable");
      expect(remoteUpstream.chatCount()).toBe(0);

      const declared = await fetch(server.baseUrl + "/v1/chat/completions", {
        method: "POST",
        headers: { "content-type": "application/json", "x-idoris-intent": "coding" },
        body: chatBody("org/opaque-model", "tell me a joke"),
      });
      expect(declared.status).toBe(503);
      expect(((await declared.json()) as { error: { type: string } }).error.type).toBe("local_only_unavailable");
      expect(remoteUpstream.chatCount()).toBe(0);

      remoteUpstream.queueChat({ kind: "json", body: { choices: [{ message: { content: "remote-ok" } }] } });
      const opened = await fetch(server.baseUrl + "/v1/chat/completions", {
        method: "POST",
        headers: { "content-type": "application/json", "x-idoris-privacy": "any" },
        body: chatBody("idoris/auto", "fix this bug"),
      });
      expect(opened.status).toBe(200);
      expect(remoteUpstream.chatCount()).toBe(1);
      expect(localUpstream.chatCount()).toBe(1);
    } finally {
      await server?.stop();
      await localUpstream.close();
      await remoteUpstream.close();
    }
  });
});
