import { describe, expect, it } from "vitest";
import { startFakeUpstream } from "../src/fake-upstream.js";
import { localComponent, makeComponentsDir, makeRoutingPolicyFile } from "../src/fixtures.js";
import { spawnConformanceServer, type RunningServer } from "../src/harness.js";

describe("intent fallback wiring", () => {
  it.each([
    { explicitIntent: undefined, prompt: "fix this bug", expectedStatus: 200 },
    { explicitIntent: "chat", prompt: "fix this bug", expectedStatus: 503 },
    { explicitIntent: undefined, prompt: "tell me a joke", expectedStatus: 503 },
  ])("intent=$explicitIntent prompt=$prompt => $expectedStatus", async ({ explicitIntent, prompt, expectedStatus }) => {
    const upstream = await startFakeUpstream();
    let server: RunningServer | undefined;
    try {
      const componentsDir = makeComponentsDir([localComponent(upstream.url)]);
      const routingPolicyPath = makeRoutingPolicyFile(`routing_policy:
  version: 1
  rules:
    - if: { intent: coding }
      then: { tiers: [local], fail_closed: true }
  default: { tiers: [remote], fail_closed: true }
`);
      server = await spawnConformanceServer({ componentsDir, routingPolicyPath });
      upstream.queueChat({ kind: "json", body: { choices: [{ message: { content: "intent-ok" } }] } });
      const headers: Record<string, string> = { "content-type": "application/json" };
      if (explicitIntent !== undefined) headers["x-idoris-intent"] = explicitIntent;
      const res = await fetch(server.baseUrl + "/v1/chat/completions", {
        method: "POST",
        headers,
        body: JSON.stringify({ model: "idoris/daily", messages: [{ role: "user", content: prompt }] }),
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
});
