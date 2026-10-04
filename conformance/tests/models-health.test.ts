import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { startFakeUpstream, type FakeUpstream } from "../src/fake-upstream.js";
import { spawnConformanceServer, routingPolicyFixturePath, type RunningServer } from "../src/harness.js";
import { makeComponentsDir, remoteComponent } from "../src/fixtures.js";

let upstream: FakeUpstream;
let server: RunningServer;

beforeAll(async () => {
  upstream = await startFakeUpstream();
  server = await spawnConformanceServer({
    componentsDir: makeComponentsDir([remoteComponent(upstream.url)]),
    routingPolicyPath: routingPolicyFixturePath,
  });
});

afterAll(async () => {
  await server.stop();
  await upstream.close();
});

describe("GET /v1/models provider health", () => {
  it("成功清零连续失败数，三次连续失败后冷却并停止请求上游", async () => {
    upstream.setModels([{ id: "health-model" }]);
    for (const status of [500, 500, 200, 500, 500, 200, 500, 500, 500]) upstream.queueModels(status);
    for (let i = 1; i <= 9; i += 1) {
      const res = await fetch(server.baseUrl + "/v1/models");
      expect(res.status).toBe(200);
      const body = (await res.json()) as { object: string; data: Array<{ id: string; object: string; owned_by: string }> };
      expect(body.object).toBe("list");
      expect(body.data).toEqual(i === 3 || i === 6
        ? [{ id: "health-model", object: "model", owned_by: "omlx" }]
        : []);
      expect(upstream.modelsCount()).toBe(i);
    }
    const cooled = await fetch(server.baseUrl + "/v1/models");
    expect(cooled.status).toBe(200);
    expect(upstream.modelsCount()).toBe(9);
    expect(await cooled.json()).toEqual({ object: "list", data: [] });
  });
});
