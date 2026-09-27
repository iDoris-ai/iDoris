import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { startFakeUpstream, type FakeUpstream } from "../src/fake-upstream.js";
import { spawnConformanceServer, routingPolicyFixturePath, type RunningServer } from "../src/harness.js";
import { localComponent, makeComponentsDir } from "../src/fixtures.js";

let upstream: FakeUpstream;
let server: RunningServer;
let reqSeq = 0;
const nextRequestId = (): string => "core-" + String(reqSeq++) + "-" + String(Date.now());

const postChat = (headers: Record<string, string> = {}, body?: string) =>
  fetch(server.baseUrl + "/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json", ...headers },
    body: body ?? JSON.stringify({ model: "idoris/daily", messages: [{ role: "user", content: "你好" }] }),
  });

beforeAll(async () => {
  upstream = await startFakeUpstream();
  const componentsDir = makeComponentsDir([localComponent(upstream.url)]);
  server = await spawnConformanceServer({ componentsDir, routingPolicyPath: routingPolicyFixturePath });
});

afterAll(async () => {
  await server.stop();
  await upstream.close();
});

describe("GET /health", () => {
  it("200，且带组件数量", async () => {
    const res = await fetch(server.baseUrl + "/health");
    expect(res.status).toBe(200);
    const body = (await res.json()) as { status: string; components: number };
    expect(body.status).toBe("ok");
    expect(body.components).toBeGreaterThanOrEqual(1);
  });
});

describe("GET /v1/models", () => {
  it("形状正确：id/object/owned_by 来自假上游的映射", async () => {
    upstream.setModels([{ id: "conformance-model-a" }, { id: "conformance-model-b" }]);
    const res = await fetch(server.baseUrl + "/v1/models");
    expect(res.status).toBe(200);
    const body = (await res.json()) as {
      object: string;
      data: Array<{ id: string; object: string; owned_by: string }>;
    };
    expect(body.object).toBe("list");
    const ids = body.data.map((m) => m.id);
    expect(ids).toContain("conformance-model-a");
    expect(ids).toContain("conformance-model-b");
    for (const m of body.data) {
      expect(m.object).toBe("model");
      expect(m.owned_by).toBe("omlx");
    }
  });
});

describe("请求体解析", () => {
  it("非法 JSON => 400 invalid_json", async () => {
    const res = await postChat({}, "{not valid json");
    expect(res.status).toBe(400);
    const body = (await res.json()) as { error: { type: string } };
    expect(body.error.type).toBe("invalid_json");
  });

  it("正控：合法最小请求走完整链路 => 200", async () => {
    upstream.queueChat({ kind: "json", status: 200, body: { choices: [{ message: { content: "hi" } }] } });
    const res = await postChat({ "x-idoris-request-id": nextRequestId() });
    expect(res.status).toBe(200);
  });

  // H2（PR #46）：合法 JSON 但不是对象（null / 数组 / 数字）单独判成 invalid_body，
  // 跟"压根不是合法 JSON"的 invalid_json 分开。
  for (const [label, rawBody] of [
    ["null", "null"],
    ["数组", "[]"],
    ["数字", "1"],
  ] as const) {
    it("合法 JSON 但不是对象（" + label + "）=> 400 invalid_body", async () => {
      const res = await postChat({}, rawBody);
      expect(res.status).toBe(400);
      const body = (await res.json()) as { error: { type: string } };
      expect(body.error.type).toBe("invalid_body");
    });
  }

  it("顺序锁定：invalid_json 优先于 invalid_body（先解析 JSON，再判断形状）", async () => {
    const res = await postChat({}, "{not valid json");
    const body = (await res.json()) as { error: { type: string } };
    expect(body.error.type).toBe("invalid_json");
  });

  it("顺序锁定：invalid_body 优先于 header 校验（body 形状先于控制面 header 判定）", async () => {
    // body 是数组（invalid_body）+ Privacy header 也非法（invalid_privacy）：
    // 应该报 invalid_body，不是 invalid_privacy——证明 body 形状检查排在 header 校验前面。
    const res = await postChat({ "x-idoris-privacy": "bogus" }, "[]");
    expect(res.status).toBe(400);
    const body = (await res.json()) as { error: { type: string } };
    expect(body.error.type).toBe("invalid_body");
  });
});

describe("控制面 header 解析（非法值处理，以现有 TS 行为为准）", () => {
  it("X-iDoris-Privacy 非法值 => 400 invalid_privacy", async () => {
    const res = await postChat({ "x-idoris-privacy": "bogus" });
    expect(res.status).toBe(400);
    const body = (await res.json()) as { error: { type: string } };
    expect(body.error.type).toBe("invalid_privacy");
  });

  it("缺少 Privacy header 时按 local_only 处理（正控：仍命中本地候选）", async () => {
    upstream.queueChat({ kind: "json", status: 200, body: { choices: [{ message: { content: "hi" } }] } });
    const res = await postChat({ "x-idoris-request-id": nextRequestId() });
    expect(res.status).toBe(200);
  });

  it("X-iDoris-Complexity 非法值 => 400 invalid_header", async () => {
    const res = await postChat({ "x-idoris-complexity": "ultra" });
    expect(res.status).toBe(400);
    const body = (await res.json()) as { error: { type: string } };
    expect(body.error.type).toBe("invalid_header");
  });

  it("X-iDoris-Capabilities 含未知能力 => 400 invalid_header", async () => {
    const res = await postChat({ "x-idoris-capabilities": "chat,teleport" });
    expect(res.status).toBe(400);
    const body = (await res.json()) as { error: { type: string } };
    expect(body.error.type).toBe("invalid_header");
  });

  it("X-iDoris-Fallback 非法值 => 400 invalid_header", async () => {
    const res = await postChat({ "x-idoris-fallback": "yolo" });
    expect(res.status).toBe(400);
    const body = (await res.json()) as { error: { type: string } };
    expect(body.error.type).toBe("invalid_header");
  });

  it("正控：X-iDoris-Complexity/Capabilities/Fallback 合法值均放行", async () => {
    upstream.queueChat({ kind: "json", status: 200, body: { choices: [{ message: { content: "hi" } }] } });
    const res = await postChat({
      "x-idoris-complexity": "simple",
      "x-idoris-capabilities": "chat,coding",
      "x-idoris-fallback": "next_in_chain",
      "x-idoris-request-id": nextRequestId(),
    });
    expect(res.status).toBe(200);
  });

  it("X-iDoris-Intent 接受任意非空字符串（不做枚举校验，正控）", async () => {
    upstream.queueChat({ kind: "json", status: 200, body: { choices: [{ message: { content: "hi" } }] } });
    const res = await postChat({
      "x-idoris-intent": "totally-made-up-intent",
      "x-idoris-request-id": nextRequestId(),
    });
    expect(res.status).toBe(200);
  });
});
