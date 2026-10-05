import { request, type OutgoingHttpHeaders } from "node:http";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { startFakeUpstream, type FakeUpstream } from "../src/fake-upstream.js";
import { localComponent, makeComponentsDir } from "../src/fixtures.js";
import {
  conformanceAuthorizationHeader,
  routingPolicyFixturePath,
  spawnConformanceServer,
  type RunningServer,
} from "../src/harness.js";

const isRust = process.env.IDORIS_CONFORMANCE_IMPLEMENTATION === "rust";

let upstream: FakeUpstream;
let server: RunningServer;
let requestSeq = 0;

beforeAll(async () => {
  upstream = await startFakeUpstream();
  const componentsDir = makeComponentsDir([
    localComponent(upstream.url, { capabilities: ["chat", "coding"] }),
  ]);
  server = await spawnConformanceServer({ componentsDir, routingPolicyPath: routingPolicyFixturePath });
});

afterAll(async () => {
  await server.stop();
  await upstream.close();
});

interface RawResponse {
  status: number;
  body: string;
}

function rawRequest(
  method: string,
  path: string,
  headers: OutgoingHttpHeaders = {},
  body?: string,
): Promise<RawResponse> {
  return new Promise((resolve, reject) => {
    const target = new URL(server.baseUrl);
    const requestHeaders: OutgoingHttpHeaders = { ...headers };
    const authorization = conformanceAuthorizationHeader();
    if (path === "/v1/chat/completions" && authorization !== undefined && requestHeaders.authorization === undefined) {
      requestHeaders.authorization = authorization;
    }
    const req = request(
      {
        hostname: target.hostname,
        port: Number(target.port),
        method,
        path,
        headers: requestHeaders,
      },
      (res) => {
        let data = "";
        res.setEncoding("utf8");
        res.on("data", (chunk: string) => {
          data += chunk;
        });
        res.on("end", () => resolve({ status: res.statusCode ?? 0, body: data }));
      },
    );
    req.once("error", reject);
    if (body !== undefined) req.write(body);
    req.end();
  });
}

function normalBody(): string {
  return JSON.stringify({ model: "idoris/daily", messages: [{ role: "user", content: "edge" }] });
}

describe("D-B1-1 input edge contract", () => {
  it("empty chat body locks the approved implementation difference", async () => {
    if (!isRust) upstream.queueChat({ kind: "json", status: 200, body: { choices: [{ message: { content: "ok" } }] } });
    const res = await rawRequest("POST", "/v1/chat/completions", { "content-type": "application/json" });
    if (isRust) {
      expect(res.status).toBe(400);
      expect(JSON.parse(res.body).error.type).toBe("invalid_json");
    } else {
      expect(res.status).toBe(200);
    }
  });

  it("empty explicit intent header is fail-closed in Rust and defaulted by TS", async () => {
    if (!isRust) upstream.queueChat({ kind: "json", status: 200, body: { choices: [{ message: { content: "ok" } }] } });
    const res = await rawRequest(
      "POST",
      "/v1/chat/completions",
      {
        "content-type": "application/json",
        "x-idoris-intent": "",
        "x-idoris-request-id": `input-edge-empty-${requestSeq++}`,
      },
      normalBody(),
    );
    if (isRust) {
      expect(res.status).toBe(400);
      expect(JSON.parse(res.body).error.type).toBe("invalid_header");
    } else {
      expect(res.status).toBe(200);
    }
  });

  it("duplicate control header values are rejected by Rust instead of silently merged", async () => {
    if (!isRust) upstream.queueChat({ kind: "json", status: 200, body: { choices: [{ message: { content: "ok" } }] } });
    const res = await rawRequest(
      "POST",
      "/v1/chat/completions",
      {
        "content-type": "application/json",
        "x-idoris-capabilities": ["chat", "coding"],
        "x-idoris-request-id": `input-edge-dup-${requestSeq++}`,
      },
      normalBody(),
    );
    if (isRust) {
      expect(res.status).toBe(400);
      expect(JSON.parse(res.body).error.type).toBe("invalid_header");
    } else {
      expect(res.status).toBe(200);
    }
  });

  it("HEAD /health locks Axum HEAD support vs the TS GET-only route", async () => {
    const res = await rawRequest("HEAD", "/health");
    expect(res.status).toBe(isRust ? 200 : 404);
  });

  it("positive controls still execute the real route", async () => {
    upstream.queueChat({ kind: "json", status: 200, body: { choices: [{ message: { content: "ok" } }] } });
    const chat = await rawRequest(
      "POST",
      "/v1/chat/completions",
      {
        "content-type": "application/json",
        "x-idoris-intent": "chat",
        "x-idoris-capabilities": "chat",
        "x-idoris-request-id": `input-edge-positive-${requestSeq++}`,
      },
      normalBody(),
    );
    expect(chat.status).toBe(200);

    const health = await rawRequest("GET", "/health");
    expect(health.status).toBe(200);
    expect(JSON.parse(health.body).status).toBe("ok");
  });
});
