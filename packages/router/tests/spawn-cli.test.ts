import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { ModelBackend } from "@idoris/adapters";
import { validateComponentCard } from "@idoris/contracts";
import { afterAll, describe, expect, it } from "vitest";
import { startRouter, type Router } from "../src/server.js";

const dir = mkdtempSync(join(tmpdir(), "idoris-spawn-cli-"));
const policyPath = join(dir, "policy.yaml");
writeFileSync(
  policyPath,
  [
    "routing_policy:",
    "  version: 1",
    "  rules:",
    "    - if: { complexity: complex }",
    "      then: { tiers: [remote] }",
    "  default: { tiers: [local], fail_closed: true }",
    "",
  ].join("\n"),
  "utf8",
);

const card = validateComponentCard({
  provider: {
    id: "subscription",
    family: "other",
    tier: "remote",
    capabilities: ["chat"],
    privacy_class: "any",
    cost: { input_per_m: 0, output_per_m: 0 },
    locality: "loopback",
  },
  form: "spawn_cli",
  endpoint: "spawn://subscription",
  version_pin: "test@1",
  privacy_class: "any",
  allowed_egress: ["loopback"],
  fallback_policy: "next_in_chain",
  fail_closed: false,
});

let running: Router | undefined;
afterAll(async () => {
  if (running) await running.close();
  rmSync(dir, { recursive: true, force: true });
});

describe("startRouter spawn_cli path", () => {
  it("serves a spawn_cli provider through the backend instead of HTTP proxy", async () => {
    const calls: string[] = [];
    const backend: ModelBackend = {
      list: async () => [{ id: "claude-subscription", memoryGb: 0 }],
      load: async () => undefined,
      unload: async () => undefined,
      admission: async () => "coexist",
      status: async () => ({ pressure: "ok", usedGb: 0, modelMemoryMaxGb: 0, loaded: [] }),
      chat: async (req) => {
        calls.push(req.messages[0]?.content ?? "");
        return { model: req.model, content: "stub:" + (req.messages[0]?.content ?? "") };
      },
    };
    running = await startRouter({
      componentsDir: dir,
      registered: [{ card, backend }],
      routingPolicyPath: policyPath,
      env: { IDORIS_DEPLOY_MODE: "personal" },
    });
    const res = await fetch("http://127.0.0.1:" + running.port + "/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json", "x-idoris-privacy": "any", "x-idoris-complexity": "complex" },
      body: JSON.stringify({ model: "claude-subscription", messages: [{ role: "user", content: "hello" }] }),
    });
    expect(res.status).toBe(200);
    const body = (await res.json()) as { object: string; choices: Array<{ message: { content: string } }> };
    expect(body.object).toBe("chat.completion");
    expect(body.choices[0]?.message.content).toBe("stub:hello");
    expect(calls).toEqual(["hello"]);
  });
});
