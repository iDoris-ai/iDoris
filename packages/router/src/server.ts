import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import type { RoutingPolicy } from "@idoris/contracts";
import {
  DefaultCapabilitiesProvider,
  type CapabilitiesProvider,
} from "./capabilities.js";
import { dispatch, type EgressCounter } from "./dispatch.js";
import { HealthTracker } from "./health.js";
import { decide, loadRoutingPolicy } from "./policy.js";
import { parseProfile, ProfileError } from "./profile.js";
import { ChatProxy } from "./proxy.js";
import { loadComponents, type Registered } from "./registry.js";

export interface RouterOptions {
  componentsDir: string;
  routingPolicyPath?: string;
  port?: number;
  health?: HealthTracker;
  proxy?: ChatProxy;
  /** 容量接口提供者；缺省时首次请求 /capabilities 时按 config/catalog.yaml 构造。 */
  capabilities?: CapabilitiesProvider;
}

export interface Router {
  server: Server;
  registered: Registered[];
  port: number;
  host: string;
  health: HealthTracker;
  close(): Promise<void>;
}

/** 绑定点硬编码 loopback（涉安全）：不监听非本机地址。 */
const BIND_HOST = "127.0.0.1";

function json(res: ServerResponse, status: number, body: unknown): void {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
}

export async function startRouter(opts: RouterOptions): Promise<Router> {
  const registered = loadComponents(opts.componentsDir);
  const health = opts.health ?? new HealthTracker();
  const policy = opts.routingPolicyPath === undefined ? undefined : loadRoutingPolicy(opts.routingPolicyPath);
  const proxy = opts.proxy ?? new ChatProxy();
  const egress: EgressCounter = { count: 0 };
  let capabilities = opts.capabilities;
  const getCapabilities = (): CapabilitiesProvider => {
    if (capabilities === undefined) capabilities = new DefaultCapabilitiesProvider({ registered });
    return capabilities;
  };
  const server = createServer((req, res) => {
    void handle(req, res, registered, health, policy, proxy, egress, getCapabilities);
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(opts.port ?? 0, BIND_HOST, () => resolve());
  });
  const addr = server.address();
  const port = typeof addr === "object" && addr !== null ? addr.port : (opts.port ?? 0);
  return {
    server,
    registered,
    port,
    host: BIND_HOST,
    health,
    close: () => new Promise<void>((resolve) => server.close(() => resolve())),
  };
}

function readBody(req: IncomingMessage): Promise<string> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    req.on("data", (c: Buffer) => chunks.push(c));
    req.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
    req.on("error", reject);
  });
}

async function handle(
  req: IncomingMessage,
  res: ServerResponse,
  registered: Registered[],
  health: HealthTracker,
  policy: RoutingPolicy | undefined,
  proxy: ChatProxy,
  egress: EgressCounter,
  getCapabilities: () => CapabilitiesProvider,
): Promise<void> {
  if (req.method === "GET" && req.url === "/health") {
    json(res, 200, { status: "ok", components: registered.length });
    return;
  }
  if (req.method === "GET" && req.url === "/v1/models") {
    const data: Array<{ id: string; object: string; owned_by: string }> = [];
    for (const { card, backend } of registered) {
      if (health.isCoolingDown(card.provider.id)) continue;
      try {
        const models = await backend.list();
        health.record(card.provider.id, true);
        for (const m of models) data.push({ id: m.id, object: "model", owned_by: card.provider.id });
      } catch {
        health.record(card.provider.id, false);
      }
    }
    json(res, 200, { object: "list", data });
    return;
  }
  if (req.method === "GET" && req.url === "/capabilities") {
    // T2.2.1：顶层 JSON 数组；每项附容量字段（06 §10.8）。
    try {
      json(res, 200, await getCapabilities().snapshot());
    } catch (err) {
      json(res, 503, {
        error: {
          type: "capabilities_unavailable",
          message: err instanceof Error ? err.message : String(err),
        },
      });
    }
    return;
  }
  if (req.method === "POST" && req.url === "/v1/chat/completions") {
    await handleChat(req, res, registered, policy, proxy, egress);
    return;
  }
  json(res, 404, { error: { type: "not_found" } });
}

async function handleChat(
  req: IncomingMessage,
  res: ServerResponse,
  registered: Registered[],
  policy: RoutingPolicy | undefined,
  proxy: ChatProxy,
  egress: EgressCounter,
): Promise<void> {
  let body: Record<string, unknown>;
  try {
    body = JSON.parse((await readBody(req)) || "{}") as Record<string, unknown>;
  } catch {
    json(res, 400, { error: { type: "invalid_json" } });
    return;
  }
  if (policy === undefined) {
    json(res, 503, { error: { type: "policy_unconfigured" } });
    return;
  }
  let profile;
  let tenantId: string | undefined;
  try {
    const parsedProfile = parseProfile(req.headers);
    profile = parsedProfile.profile;
    // ★ 不要只取 .profile —— tenantId 丢在这里就等于幂等缓存跨租户共享（评审 PR #25）。
    tenantId = parsedProfile.tenantId;
  } catch (err) {
    if (err instanceof ProfileError) {
      json(res, err.status, { error: { type: err.code, message: err.message } });
      return;
    }
    throw err;
  }
  const outcome = dispatch(profile, decide(policy, profile), registered, egress);
  if (outcome.status !== 200 || outcome.providerId === undefined) {
    json(res, outcome.status, outcome.body);
    return;
  }
  const target = registered.find((r) => r.card.provider.id === outcome.providerId);
  if (!target) {
    json(res, 503, { error: { type: "no_candidate" } });
    return;
  }

  const controller = new AbortController();
  req.on("close", () => controller.abort());
  const requestId = req.headers["x-idoris-request-id"];
  const result = await proxy.forward(
    target.card.endpoint,
    undefined,
    body,
    {
      stream: body.stream === true,
      ...(typeof requestId === "string" ? { requestId } : {}),
      ...(tenantId !== undefined ? { tenantId } : {}),
      signal: controller.signal,
    },
  );

  if (result.stream !== null) {
    res.writeHead(result.status, { "content-type": "text/event-stream" });
    const reader = result.stream.getReader();
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        if (value !== undefined) res.write(Buffer.from(value));
      }
    } catch {
      // 客户端断开：reader.read 抛错或 controller 已 abort
    } finally {
      res.end();
    }
    return;
  }
  res.writeHead(result.status, { "content-type": "application/json" });
  res.end(result.text);
}
