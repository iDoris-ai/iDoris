import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { startFakeUpstream, type FakeUpstream } from "../src/fake-upstream.js";
import { spawnConformanceServer, routingPolicyFixturePath, type RunningServer } from "../src/harness.js";
import { localComponent, makeComponentsDir } from "../src/fixtures.js";

/**
 * T4.1（PR #46，接口规范 §3.1/§3.7/§3.12）：服务身份字段、`X-iDoris-Record-Id`、
 * `X-iDoris-Served-Locality`、`X-iDoris-Cached`/`X-iDoris-Origin-Record-Id`。
 * 这批行为已经在 `feat/m4-t4.1-agent24-prereqs` 落地，不再是"待实现"，直接跑。
 */
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

let upstream: FakeUpstream;
let server: RunningServer;

beforeAll(async () => {
  upstream = await startFakeUpstream();
  const componentsDir = makeComponentsDir([localComponent(upstream.url)]);
  server = await spawnConformanceServer({ componentsDir, routingPolicyPath: routingPolicyFixturePath });
});

afterAll(async () => {
  await server.stop();
  await upstream.close();
});

const postChat = (headers: Record<string, string> = {}, body?: string) =>
  fetch(server.baseUrl + "/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json", ...headers },
    body: body ?? JSON.stringify({ model: "idoris/daily", messages: [{ role: "user", content: "hi" }] }),
  });

describe("GET /health 服务身份", () => {
  it("字段齐全：service/version/contract_version/instance_id/components", async () => {
    const res = await fetch(server.baseUrl + "/health");
    const body = (await res.json()) as Record<string, unknown>;
    expect(body.status).toBe("ok");
    expect(body.service).toBe("idoris");
    expect(typeof body.version).toBe("string");
    // contract_version 是规范文档标注的值（当前 v1.0.1），不是随便一个字符串。
    expect(body.contract_version).toBe("1.0.1");
    expect(typeof body.instance_id).toBe("string");
    expect((body.instance_id as string).length).toBeGreaterThan(0);
    expect(body.components).toBeGreaterThanOrEqual(1);
  });

  it("instance_id 同一进程两次请求保持不变", async () => {
    const a = (await (await fetch(server.baseUrl + "/health")).json()) as { instance_id: string };
    const b = (await (await fetch(server.baseUrl + "/health")).json()) as { instance_id: string };
    expect(b.instance_id).toBe(a.instance_id);
  });
});

describe("X-iDoris-Record-Id", () => {
  it("成功响应（/health）带服务端生成的 Record-Id", async () => {
    const res = await fetch(server.baseUrl + "/health");
    expect(res.headers.get("x-idoris-record-id")).toMatch(UUID_RE);
  });

  it("404（未知路由）也带 Record-Id", async () => {
    const res = await fetch(server.baseUrl + "/nope");
    expect(res.status).toBe(404);
    expect(res.headers.get("x-idoris-record-id")).toMatch(UUID_RE);
  });

  it("400 invalid_json 响应带 Record-Id", async () => {
    const res = await postChat({}, "{not valid json");
    expect(res.status).toBe(400);
    expect(res.headers.get("x-idoris-record-id")).toMatch(UUID_RE);
  });

  it("调用方自己填的 X-iDoris-Request-Id / X-iDoris-Record-Id 都不会被采纳", async () => {
    const res = await fetch(server.baseUrl + "/health", {
      headers: { "x-idoris-request-id": "caller-request-id", "x-idoris-record-id": "caller-record-id" },
    });
    const recordId = res.headers.get("x-idoris-record-id");
    expect(recordId).toMatch(UUID_RE);
    expect(recordId).not.toBe("caller-request-id");
    expect(recordId).not.toBe("caller-record-id");
  });

  it("负对照：两次请求的 Record-Id 不同", async () => {
    const a = (await fetch(server.baseUrl + "/health")).headers.get("x-idoris-record-id");
    const b = (await fetch(server.baseUrl + "/health")).headers.get("x-idoris-record-id");
    expect(a).not.toBeNull();
    expect(a).not.toBe(b);
  });
});

describe("X-iDoris-Served-Locality", () => {
  it("已选定后端（loopback 本地候选）的成功响应带 Served-Locality: loopback", async () => {
    upstream.queueChat({ kind: "json", status: 200, body: { choices: [{ message: { content: "ok" } }] } });
    const res = await postChat();
    expect(res.status).toBe(200);
    expect(res.headers.get("x-idoris-served-locality")).toBe("loopback");
  });

  it("上游返回 5xx（依然是“某后端服务了这次请求”）也带 Served-Locality", async () => {
    for (let i = 0; i < 3; i += 1) upstream.queueChat({ kind: "json", status: 503, body: { error: "x" } });
    const res = await postChat();
    expect(res.status).toBe(503);
    expect(res.headers.get("x-idoris-served-locality")).toBe("loopback");
  });

  it("还没选定后端就被拒绝的响应不带 Served-Locality（400 invalid_privacy）", async () => {
    const res = await postChat({ "x-idoris-privacy": "bogus" });
    expect(res.status).toBe(400);
    expect(res.headers.get("x-idoris-served-locality")).toBeNull();
  });

  it("还没选定后端就被拒绝的响应不带 Served-Locality（400 invalid_body）", async () => {
    const res = await postChat({}, "null");
    expect(res.status).toBe(400);
    expect(res.headers.get("x-idoris-served-locality")).toBeNull();
  });
});

describe("X-iDoris-Cached / X-iDoris-Origin-Record-Id", () => {
  it("首次是真实推理：不带 Cached / Origin-Record-Id；第二次命中缓存才带，且指回第一条 Record-Id", async () => {
    upstream.queueChat({ kind: "json", status: 200, body: { choices: [{ message: { content: "ok" } }] } });
    const id = "headers-cache-" + String(Date.now());

    const first = await postChat({ "x-idoris-request-id": id });
    expect(first.status).toBe(200);
    const firstRecordId = first.headers.get("x-idoris-record-id");
    expect(firstRecordId).toMatch(UUID_RE);
    expect(first.headers.get("x-idoris-cached")).toBeNull();
    expect(first.headers.get("x-idoris-origin-record-id")).toBeNull();

    const second = await postChat({ "x-idoris-request-id": id });
    expect(second.status).toBe(200);
    const secondRecordId = second.headers.get("x-idoris-record-id");
    expect(second.headers.get("x-idoris-cached")).toBe("true");
    expect(second.headers.get("x-idoris-origin-record-id")).toBe(firstRecordId);
    // Record-Id 永远是这次请求新生成的，即便命中缓存也不会退化成复用第一条的。
    expect(secondRecordId).not.toBe(firstRecordId);
  });

  it("负对照：不同 Request-Id 不会互相命中缓存", async () => {
    upstream.queueChat({ kind: "json", status: 200, body: { marker: "a" } });
    upstream.queueChat({ kind: "json", status: 200, body: { marker: "b" } });
    const a = await postChat({ "x-idoris-request-id": "headers-cache-a-" + String(Date.now()) });
    const b = await postChat({ "x-idoris-request-id": "headers-cache-b-" + String(Date.now()) });
    expect(a.status).toBe(200);
    expect(b.status).toBe(200);
    expect(b.headers.get("x-idoris-cached")).toBeNull();
  });
});
