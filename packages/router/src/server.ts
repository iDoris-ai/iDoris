import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import {
  isSubscriptionProviderId,
  openAIChatCompletion,
  type ChatMessage,
} from "@idoris/adapters";
import type { RoutingPolicy } from "@idoris/contracts";
import {
  DefaultCapabilitiesProvider,
  type CapabilitiesProvider,
} from "./capabilities.js";
import { dispatch, type EgressCounter } from "./dispatch.js";
import { assertSubscriptionSource, EgressGuardError } from "./egress-guard.js";
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
  /** 读取 deploy_mode / 订阅开关的环境；默认 process.env。 */
  env?: NodeJS.ProcessEnv;
  /** 测试注入已注册组件（生产路径始终走 loadComponents 的 fail-closed 门禁）。 */
  registered?: Registered[];
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
  const env = opts.env ?? process.env;
  const registered = opts.registered ?? loadComponents(opts.componentsDir, { env });
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
    void handle(req, res, registered, health, policy, proxy, egress, getCapabilities, env);
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

/** 请求体 messages → 后端 ChatMessage[]（只接受 role/content 均为字符串的条目）。 */
function toMessages(value: unknown): ChatMessage[] {
  if (!Array.isArray(value)) return [];
  const out: ChatMessage[] = [];
  for (const item of value) {
    if (item === null || typeof item !== "object") continue;
    const role = (item as { role?: unknown }).role;
    const content = (item as { content?: unknown }).content;
    if (typeof role === "string" && typeof content === "string") out.push({ role, content });
  }
  return out;
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
  env: NodeJS.ProcessEnv,
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
    await handleChat(req, res, registered, policy, proxy, egress, env);
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
  env: NodeJS.ProcessEnv,
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
  try {
    profile = parseProfile(req.headers).profile;
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

  // T1.4.2：订阅 provider 的来源复核（非 loopback 一律拒，绝不外泄给个人订阅通道）。
  if (isSubscriptionProviderId(outcome.providerId)) {
    try {
      assertSubscriptionSource(req.socket.remoteAddress, env);
    } catch (err) {
      if (err instanceof EgressGuardError) {
        json(res, 403, { error: { type: "subscription_source_not_loopback", message: err.message } });
        return;
      }
      throw err;
    }
  }

  const controller = new AbortController();
  req.on("close", () => controller.abort());

  // spawn_cli 型（订阅中转）：直接调后端进程，不经过 HTTP 转发。
  if (target.card.form === "spawn_cli") {
    const model = typeof body.model === "string" ? body.model : target.card.provider.id;
    const messages = toMessages(body.messages);
    try {
      const chat = await target.backend.chat({ model, messages, signal: controller.signal });
      const prompt = messages.map((m) => m.content).join("\n");
      json(res, 200, openAIChatCompletion(chat.content, model, prompt));
    } catch (err) {
      json(res, 502, {
        error: { type: "subscription_relay_failed", message: err instanceof Error ? err.message : String(err) },
      });
    }
    return;
  }

  const requestId = req.headers["x-idoris-request-id"];
  const result = await proxy.forward(
    target.card.endpoint,
    undefined,
    body,
    {
      stream: body.stream === true,
      ...(typeof requestId === "string" ? { requestId } : {}),
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
