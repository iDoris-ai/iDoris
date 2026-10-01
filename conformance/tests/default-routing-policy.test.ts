import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { startFakeUpstream, type FakeUpstream } from "../src/fake-upstream.js";
import { ConformanceStartupError, spawnConformanceServer, type RunningServer } from "../src/harness.js";
import { localComponent, makeComponentsDir } from "../src/fixtures.js";

/**
 * M1（PR #46）：不传 `IDORIS_ROUTING_POLICY` 不再是"路由未配置"，而是落到
 * 仓库自带的 `config/routing-policy.yaml`。原来 R0 第一版假设的"503
 * policy_unconfigured"场景，走生产 CLI 已经不可达——`server.ts` 里
 * `policy === undefined` 那个分支只有直接调库函数 `startRouter()`（不传
 * `routingPolicyPath`）才够得到，`packages/router/tests/crash-safety.test.ts`
 * 那条"负对照"测的就是库层这个分支；黑盒走 CLI 拿不到。
 */
let upstream: FakeUpstream;
let server: RunningServer;

beforeAll(async () => {
  upstream = await startFakeUpstream();
  const componentsDir = makeComponentsDir([localComponent(upstream.url)]);
  // 故意不传 routingPolicyPath：验证"缺省 = 仓库自带的默认路由策略"这个新行为。
  server = await spawnConformanceServer({ componentsDir });
});

afterAll(async () => {
  await server.stop();
  await upstream.close();
});

describe("不传 IDORIS_ROUTING_POLICY 时的缺省行为", () => {
  it("正控：默认策略能正常放行请求（不是 503 policy_unconfigured）", async () => {
    upstream.queueChat({ kind: "json", status: 200, body: { choices: [{ message: { content: "ok" } }] } });
    const res = await fetch(server.baseUrl + "/v1/chat/completions", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ model: "idoris/daily", messages: [{ role: "user", content: "hi" }] }),
    });
    expect(res.status).toBe(200);
  });

  it("/health 与 /v1/models 不受影响", async () => {
    expect((await fetch(server.baseUrl + "/health")).status).toBe(200);
    expect((await fetch(server.baseUrl + "/v1/models")).status).toBe(200);
  });
});

describe("IDORIS_ROUTING_POLICY 指向不存在的文件", () => {
  it("启动直接失败（fail-fast）：非零退出码、且明显快于 healthTimeoutMs，不是靠等超时才报错", async () => {
    const componentsDir = makeComponentsDir([localComponent(upstream.url)]);
    const start = Date.now();
    let caught: unknown;
    try {
      await spawnConformanceServer({
        componentsDir,
        routingPolicyPath: "/no/such/path/routing-policy.yaml",
        // 特意给一个远大于"真的 fail-fast"所需时间的超时：如果实现退化成
        // "挂到超时才报错"，这条用例会直接测出耗时接近 10s，而不是被短
        // healthTimeoutMs 掩盖成"反正都失败了看不出区别"。
        healthTimeoutMs: 10_000,
      });
    } catch (err) {
      caught = err;
    }
    const elapsed = Date.now() - start;

    expect(caught).toBeInstanceOf(ConformanceStartupError);
    const err = caught as ConformanceStartupError;
    // 是 CLI 自己检测到坏路径后主动退出（exited_early，非零退出码），
    // 不是被我们的健康检查耗尽超时后强杀（health_timeout，exitCode 为 null）。
    expect(err.kind).toBe("exited_early");
    expect(err.exitCode).not.toBeNull();
    expect(err.exitCode).not.toBe(0);
    expect(elapsed).toBeLessThan(2_000);
  });
});
