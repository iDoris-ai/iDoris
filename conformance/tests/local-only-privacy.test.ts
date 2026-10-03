import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { startFakeUpstream, type FakeUpstream } from "../src/fake-upstream.js";
import { spawnConformanceServer, routingPolicyFixturePath, type RunningServer } from "../src/harness.js";
import { localComponent, makeComponentsDir, remoteComponent } from "../src/fixtures.js";

// 只注册一个远程后端（tier=remote, locality=remote），
// 用来锁定"local_only 且唯一候选是远程"时的 fail-closed 行为：
// 503 + 远程假上游收到的真实请求数必须是 0（不是靠内部计数器，是靠真的 HTTP 请求计数）。
let upstream: FakeUpstream;
let server: RunningServer;

beforeAll(async () => {
  upstream = await startFakeUpstream();
  const componentsDir = makeComponentsDir([remoteComponent(upstream.url)]);
  server = await spawnConformanceServer({ componentsDir, routingPolicyPath: routingPolicyFixturePath });
});

afterAll(async () => {
  await server.stop();
  await upstream.close();
});

const postChat = (headers: Record<string, string> = {}) =>
  fetch(server.baseUrl + "/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json", ...headers },
    body: JSON.stringify({ model: "idoris/daily", messages: [{ role: "user", content: "hi" }] }),
  });
const postLocalChat = (baseUrl: string) =>
  fetch(baseUrl + "/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json", "x-idoris-privacy": "local_only" },
    body: JSON.stringify({ model: "idoris/daily", messages: [{ role: "user", content: "hi" }] }),
  });

describe("privacy=local_only fail-closed（唯一候选是远程后端）", () => {
  it("缺省（未带 Privacy header）按 local_only 处理 => 503，远程出站 0 次", async () => {
    const before = upstream.chatCount();
    const res = await postChat();
    expect(res.status).toBe(503);
    const body = (await res.json()) as { error: { type: string } };
    expect(body.error.type).toBe("local_only_unavailable");
    expect(upstream.chatCount()).toBe(before);
  });

  it("显式 local_only => 503，远程出站 0 次", async () => {
    const before = upstream.chatCount();
    const res = await postChat({ "x-idoris-privacy": "local_only" });
    expect(res.status).toBe(503);
    expect(upstream.chatCount()).toBe(before);
  });

  it("local_only + 命中 vision 规则依然不会选中唯一的远程候选 => 503，0 次出站", async () => {
    const before = upstream.chatCount();
    const res = await postChat({ "x-idoris-privacy": "local_only", "x-idoris-intent": "banner" });
    expect(res.status).toBe(503);
    expect(upstream.chatCount()).toBe(before);
  });

  it("正控：privacy=any + complexity=complex 会路由到唯一的远程候选（证明前面 3 条不是因为路由本身坏了）", async () => {
    const before = upstream.chatCount();
    upstream.queueChat({ kind: "json", status: 200, body: { choices: [{ message: { content: "remote-ok" } }] } });
    const res = await postChat({ "x-idoris-privacy": "any", "x-idoris-complexity": "complex" });
    expect(res.status).toBe(200);
    expect(upstream.chatCount() - before).toBe(1);
  });
});

describe("loopback 卡的 provider/card privacy=any 不满足 local_only", () => {
  it("保持 local tier 可选时拒绝不可信卡且零出站；可信 local_only 正控成功", async () => {
    const localUpstream = await startFakeUpstream();
    let localServer: RunningServer | undefined;
    try {
      const anyCard = localComponent(localUpstream.url, { privacyClass: "any" });
      localServer = await spawnConformanceServer({
        componentsDir: makeComponentsDir([anyCard]),
        routingPolicyPath: routingPolicyFixturePath,
      });
      const rejected = await postLocalChat(localServer.baseUrl);
      expect(rejected.status).toBe(503);
      expect((await rejected.json() as { error: { type: string } }).error.type).toBe("local_only_unavailable");
      expect(localUpstream.chatCount()).toBe(0);
      await localServer.stop();
      localServer = await spawnConformanceServer({
        componentsDir: makeComponentsDir([localComponent(localUpstream.url)]),
        routingPolicyPath: routingPolicyFixturePath,
      });
      localUpstream.queueChat({ kind: "json", body: { choices: [{ message: { content: "local-ok" } }] } });
      const accepted = await postLocalChat(localServer.baseUrl);
      expect(accepted.status).toBe(200);
      expect(accepted.headers.get("x-idoris-served-locality")).toBe("loopback");
      expect(await accepted.json()).toMatchObject({ choices: [{ message: { content: "local-ok" } }] });
      expect(localUpstream.chatCount()).toBe(1);
    } finally {
      await localServer?.stop();
      await localUpstream.close();
    }
  });
});
