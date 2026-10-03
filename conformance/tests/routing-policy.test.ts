import { describe, expect, it } from "vitest";
import { startFakeUpstream } from "../src/fake-upstream.js";
import { localComponent, makeComponentsDir, makeRoutingPolicyFile } from "../src/fixtures.js";
import { spawnConformanceServer, type RunningServer } from "../src/harness.js";

describe("YAML policy 真正决定请求候选", () => {
  // 同一组件、请求和上游，仅改变命中 action 的 tier；未命中必须使用 default。
  it.each([
    { rule: "local", fallback: "remote", intent: "coding", privacy: "any", failClosed: true, status: 200 },
    { rule: "remote", fallback: "local", intent: "coding", privacy: "any", failClosed: true, status: 503 },
    { rule: "remote", fallback: "local", intent: "chat", privacy: "any", failClosed: true, status: 200 },
    { rule: "local", fallback: "remote", intent: "chat", privacy: "any", failClosed: true, status: 503 },
    { rule: "remote", fallback: "local", intent: "coding", privacy: "any", failClosed: false, status: 503 },
    { rule: "local", fallback: "remote", intent: "chat", privacy: "any", failClosed: false, status: 503 },
    { rule: "remote", fallback: "local", intent: "coding", privacy: "local_only", failClosed: false, status: 503 },
  ])("rule=$rule default=$fallback intent=$intent privacy=$privacy failClosed=$failClosed => $status", async ({ rule, fallback, intent, privacy, failClosed, status }) => {
    const upstream = await startFakeUpstream();
    let server: RunningServer | undefined;
    try {
      const componentsDir = makeComponentsDir([localComponent(upstream.url)]);
      const routingPolicyPath = makeRoutingPolicyFile(`routing_policy:
  version: 1
  rules:
    - if: { intent: coding }
      then: { tiers: [${rule}], fail_closed: ${failClosed} }
  default: { tiers: [${fallback}], fail_closed: ${failClosed} }
`);
      server = await spawnConformanceServer({ componentsDir, routingPolicyPath });
      upstream.queueChat({ kind: "json", body: { choices: [{ message: { content: "policy-ok" } }] } });
      const res = await fetch(server.baseUrl + "/v1/chat/completions", {
        method: "POST",
        headers: {
          "content-type": "application/json",
          "x-idoris-privacy": privacy,
          "x-idoris-intent": intent,
        },
        body: JSON.stringify({ model: "idoris/daily", messages: [{ role: "user", content: "hi" }] }),
      });
      expect(res.status).toBe(status);
      expect(upstream.chatCount()).toBe(status === 200 ? 1 : 0);
      if (status === 200) {
        expect(res.headers.get("x-idoris-served-locality")).toBe("loopback");
        expect((await res.json() as { choices: { message: { content: string } }[] }).choices[0]?.message.content).toBe("policy-ok");
      } else {
        expect((await res.json() as { error: { type: string } }).error.type).toBe(failClosed || privacy === "local_only" ? "local_only_unavailable" : "no_candidate");
      }
    } finally {
      await server?.stop();
      await upstream.close();
    }
  });

  it("uses the first matching rule and the default only when no rule matches", async () => {
    const upstream = await startFakeUpstream();
    let server: RunningServer | undefined;
    try {
      const componentsDir = makeComponentsDir([localComponent(upstream.url)]);
      const routingPolicyPath = makeRoutingPolicyFile(`routing_policy:
  version: 1
  rules:
    - if: { intent: coding }
      then: { tiers: [local], fail_closed: true }
    - if: { intent: coding }
      then: { tiers: [remote], fail_closed: true }
  default: { tiers: [remote], fail_closed: true }
`);
      server = await spawnConformanceServer({ componentsDir, routingPolicyPath });

      const request = (intent: string) => fetch(server!.baseUrl + "/v1/chat/completions", {
        method: "POST",
        headers: { "content-type": "application/json", "x-idoris-intent": intent },
        body: JSON.stringify({ model: "idoris/daily", messages: [{ role: "user", content: "hi" }] }),
      });

      upstream.queueChat({ kind: "json", body: { choices: [{ message: { content: "first-match" } }] } });
      const matched = await request("coding");
      expect(matched.status).toBe(200);
      expect((await matched.json() as { choices: { message: { content: string } }[] }).choices[0]?.message.content).toBe("first-match");
      expect(upstream.chatCount()).toBe(1);

      const unmatched = await request("chat");
      expect(unmatched.status).toBe(503);
      expect((await unmatched.json() as { error: { type: string } }).error.type).toBe("local_only_unavailable");
      expect(upstream.chatCount()).toBe(1);
    } finally {
      await server?.stop();
      await upstream.close();
    }
  });
});

