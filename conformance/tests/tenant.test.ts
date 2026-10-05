import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { startFakeUpstream, type FakeUpstream } from "../src/fake-upstream.js";
import { spawnConformanceServer, routingPolicyFixturePath, type RunningServer } from "../src/harness.js";
import { localComponent, makeComponentsDir, makeTenantConfigFile } from "../src/fixtures.js";

// tenant 模式要切 IDORIS_DEPLOY_MODE，和其它套件公用一个进程会互相污染，单独起一个。
let upstream: FakeUpstream;
let server: RunningServer;

beforeAll(async () => {
  upstream = await startFakeUpstream();
  const componentsDir = makeComponentsDir([localComponent(upstream.url)]);
  const tenantConfig = makeTenantConfigFile([{ tenantId: "tenant-conformance" }]);
  server = await spawnConformanceServer({
    componentsDir,
    routingPolicyPath: routingPolicyFixturePath,
    env: { IDORIS_DEPLOY_MODE: "tenant", IDORIS_TENANTS_CONFIG: tenantConfig },
  });
});

afterAll(async () => {
  await server.stop();
  await upstream.close();
});

describe("deploy_mode=tenant", () => {
  it("缺少 X-iDoris-Tenant => 400 tenant_missing", async () => {
    const res = await fetch(server.baseUrl + "/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: "idoris/daily", messages: [{ role: "user", content: "hi" }] }),
    });
    expect(res.status).toBe(400);
    const body = (await res.json()) as { error: { type: string } };
    expect(body.error.type).toBe("tenant_missing");
  });

  it("正控：带 X-iDoris-Tenant 正常放行", async () => {
    upstream.queueChat({ kind: "json", status: 200, body: { choices: [{ message: { content: "ok" } }] } });
    const res = await fetch(server.baseUrl + "/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json", "x-idoris-tenant": "tenant-conformance" },
      body: JSON.stringify({ model: "idoris/daily", messages: [{ role: "user", content: "hi" }] }),
    });
    expect(res.status).toBe(200);
  });
});
