import { randomUUID } from "node:crypto";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import {
  isPersonalDeployMode,
  isSubscriptionProviderId,
  openAIChatCompletion,
  SubscriptionRelayError,
  type ChatMessage,
} from "@idoris/adapters";
import { CONTRACT_VERSION, type RoutingPolicy, type TaskProfile } from "@idoris/contracts";
import {
  DefaultCapabilitiesProvider,
  type CapabilitiesProvider,
} from "./capabilities.js";
import { dispatch, type EgressCounter } from "./dispatch.js";
import { assertSubscriptionSource, EgressGuardError } from "./egress-guard.js";
import { HealthTracker } from "./health.js";
import { effectiveServedLocality, SERVED_LOCALITY_VALUES, type ServedLocality } from "./locality.js";
import { decide, loadRoutingPolicy } from "./policy.js";
import { defaultIntentDetector, resolveProfile, type IntentDetector } from "./intent.js";
import { ProfileError } from "./profile.js";
import { ChatProxy } from "./proxy.js";
import { loadComponents, type Registered } from "./registry.js";
import { readRouterVersion } from "./version.js";

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
  /** 未显式声明 X-iDoris-Intent 时的兜底识别器（T2.4.1）；缺省用内置话术路由。 */
  intentDetector?: IntentDetector;
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

/** 本包 package.json 的 version；进程内只需读一次。 */
const ROUTER_VERSION = readRouterVersion();

function json(res: ServerResponse, status: number, body: unknown): void {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
}

/** M5：每请求一行结构化审计日志，写 stderr。严禁塞请求/响应内容——只留可查的元数据。 */
interface AuditEntry {
  record_id: string;
  provider: string | null;
  served_locality: ServedLocality | null;
  status: number;
  duration_ms: number;
}

function writeAuditLog(entry: AuditEntry): void {
  process.stderr.write(JSON.stringify(entry) + "\n");
}

/** handleChat 用来把「选中了哪个 provider / 判定的 locality」回报给上面的审计日志。 */
interface RequestMeta {
  providerId?: string;
  servedLocality?: ServedLocality;
}

export async function startRouter(opts: RouterOptions): Promise<Router> {
  const env = opts.env ?? process.env;
  const registered = opts.registered ?? loadComponents(opts.componentsDir, { env });
  const health = opts.health ?? new HealthTracker();
  const policy = opts.routingPolicyPath === undefined ? undefined : loadRoutingPolicy(opts.routingPolicyPath);
  const proxy = opts.proxy ?? new ChatProxy();
  const egress: EgressCounter = { count: 0 };
  const intentDetector = opts.intentDetector ?? defaultIntentDetector();
  let capabilities = opts.capabilities;
  const getCapabilities = (): CapabilitiesProvider => {
    if (capabilities === undefined) capabilities = new DefaultCapabilitiesProvider({ registered });
    return capabilities;
  };
  // T4.1：/health 的 instance_id——进程内每个 Router 实例生成一次，实例生命周期内不变。
  const instanceId = randomUUID();
  const server = createServer((req, res) => {
    void handle(req, res, registered, health, policy, proxy, egress, getCapabilities, intentDetector, env, instanceId);
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
  intentDetector: IntentDetector,
  env: NodeJS.ProcessEnv,
  instanceId: string,
): Promise<void> {
  // T4.1：每个请求生成独立的 Record-Id，写在所有响应上（含错误、含流式）——
  // 由服务端生成，跟调用方填的 X-iDoris-Request-Id（或任何调用方自己塞的
  // X-iDoris-Record-Id）都无关，调用方指定不了它。
  const recordId = randomUUID();
  res.setHeader("X-iDoris-Record-Id", recordId);

  // M5：审计日志——不管走哪条分支、成功还是报错，响应一结束（正常 finish 或
  // 客户端异常断开的 close）都打一行。用 meta 让 handleChat 事后回填 provider/locality。
  const start = Date.now();
  const meta: RequestMeta = {};
  let logged = false;
  const logOnce = (): void => {
    if (logged) return;
    logged = true;
    writeAuditLog({
      record_id: recordId,
      provider: meta.providerId ?? null,
      served_locality: meta.servedLocality ?? null,
      status: res.statusCode,
      duration_ms: Date.now() - start,
    });
  };
  res.once("finish", logOnce);
  res.once("close", logOnce);

  try {
    if (req.method === "GET" && req.url === "/health") {
      json(res, 200, {
        status: "ok",
        service: "idoris",
        version: ROUTER_VERSION,
        contract_version: CONTRACT_VERSION,
        instance_id: instanceId,
        components: registered.length,
      });
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
      await handleChat(req, res, registered, policy, proxy, egress, intentDetector, env, recordId, meta);
      return;
    }
    json(res, 404, { error: { type: "not_found" } });
  } catch (err) {
    // H2：单个请求的未预期异常绝不能带垮整个进程——`void handle(...)` 调用点不会
    // catch 拒绝的 Promise，所以这道兜底必须在这里，兜住的是"没想到的 bug"而不是
    // 已知业务错误（那些在各自分支里已经用 json() 处理完了）。
    console.error(
      "[idoris-router] 未捕获的请求处理异常（已用 500 应答，未回显给调用方）：" +
        (err instanceof Error ? (err.stack ?? err.message) : String(err)),
    );
    if (res.headersSent || res.writableEnded) {
      res.destroy();
    } else {
      json(res, 500, { error: { type: "internal_error", message: "internal server error" } });
    }
  }
}

async function handleChat(
  req: IncomingMessage,
  res: ServerResponse,
  registered: Registered[],
  policy: RoutingPolicy | undefined,
  proxy: ChatProxy,
  egress: EgressCounter,
  intentDetector: IntentDetector,
  env: NodeJS.ProcessEnv,
  recordId: string,
  meta: RequestMeta,
): Promise<void> {
  let parsedBody: unknown;
  try {
    parsedBody = JSON.parse((await readBody(req)) || "{}");
  } catch {
    json(res, 400, { error: { type: "invalid_json" } });
    return;
  }
  // H2：请求体必须是一个 JSON 对象——`null`/数组/数字都是合法 JSON，但不是我们能
  // 当 Record<string, unknown> 用的形状。不挡在这里的话，下面第一次 `body.xxx`
  // 就会在 null 上抛 TypeError，顺着 Promise 链一路冒到进程外层。
  if (typeof parsedBody !== "object" || parsedBody === null || Array.isArray(parsedBody)) {
    json(res, 400, { error: { type: "invalid_body", message: "request body must be a JSON object" } });
    return;
  }
  const body = parsedBody as Record<string, unknown>;
  if (policy === undefined) {
    json(res, 503, { error: { type: "policy_unconfigured" } });
    return;
  }
  let profile: TaskProfile;
  let tenantId: string | undefined;
  try {
    // 语义冲突的正确解法是**两者都要**，不是二选一：
    //   · resolveProfile 带来 #32 的语义意图路由兜底；
    //   · .tenantId 是 #25 修跨租户幂等缓存泄漏的那一半 —— 丢了它泄漏就回来了。
    // resolveProfile 本身透传 tenantId（intent.ts:230），所以两件事不冲突。
    // #32 原本只取 .profile 并不是 bug：它的 base 当时还没有 tenantId 这条线。
    // ★ 必须把 deployMode 从注入的 env 算出来传进去。
    // resolveProfile 的 deployMode 默认值是 currentDeployMode()，它读的是
    // **process.env**；不传就会绕过 RouterOptions.env。后果不是测试不方便：
    // 部署方若用注入 env 配 tenant 模式，会静默降级成 personal → X-iDoris-Tenant
    // 被忽略 → 跨租户共享幂等缓存。env 那条注释本来就承诺了「读取 deploy_mode」。
    const deployMode = isPersonalDeployMode(env) ? "personal" : "tenant";
    const resolved = await resolveProfile(
      req.headers,
      { messages: toMessages(body.messages) },
      intentDetector,
      deployMode,
    );
    profile = resolved.profile;
    tenantId = resolved.tenantId;
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

  // T4.1：走到这里说明确实由 target 这个后端服务；此后所有响应（成功/该后端自身
  // 报错）都带实际服务方的 Served-Locality。在此之前的错误（无候选、策略未配置、
  // 订阅来源复核拒绝……）都还没有落到具体后端，不带这个头。
  const servedLocality = effectiveServedLocality(target.card);
  res.setHeader("X-iDoris-Served-Locality", servedLocality);
  meta.providerId = target.card.provider.id;
  meta.servedLocality = servedLocality;

  // §3.11 取消传播：必须监听 res（响应对象/底层 socket）的 close，不能监听 req。
  // req 的可读流在 readBody() 把请求体完整读完之后，会因为 Node 可读流默认的
  // autoDestroy 自己很快触发一次 'close'——这跟客户端有没有真的断开连接毫无
  // 关系。到这行代码执行、挂上监听器的时候，那次"自然 close"通常已经发生过了
  // （req.destroyed/req.complete 在这里已经是 true/true），监听器补挂上去也
  // 不会再收到一次，于是真实客户端断开时 controller.abort() 基本不会被调用，
  // 取消实际上不会传播给上游。res 不一样：只要还没写完响应（!res.writableEnded），
  // res 的 'close' 只会在底层连接被提前关闭（客户端主动取消）时触发——这正是
  // R0 conformance 套件（conformance/tests/upstream-behavior.test.ts）用变异测试
  // 抓出来的真实 bug，见 packages/router/tests/cancel-propagation.test.ts。
  const controller = new AbortController();
  res.on("close", () => {
    if (!res.writableEnded) controller.abort();
  });

  // spawn_cli 型（订阅中转）：直接调后端进程，不经过 HTTP 转发。
  if (target.card.form === "spawn_cli") {
    const model = typeof body.model === "string" ? body.model : target.card.provider.id;
    const messages = toMessages(body.messages);
    try {
      const chat = await target.backend.chat({ model, messages, signal: controller.signal });
      const prompt = messages.map((m) => m.content).join("\n");
      json(res, 200, openAIChatCompletion(chat.content, model, prompt));
    } catch (err) {
      // M2/H2：对外只给固定错误码 + `reason_code`，绝不透传调用方任何 CLI
      // 原始输出。服务端日志也**只记白名单元数据**（reason_code、message——
      // relay.ts 保证 message 不含 stderr 原文、退出码、stderr 字节数、
      // stderr 的 sha256 摘要前 12 位），不记任何自由文本形式的 stderr 内容——
      // 早先"脱敏后的摘要"那版本身就靠不住（正则覆盖不到短密钥/prompt 回显）。
      const reasonCode = err instanceof SubscriptionRelayError ? err.code : "RELAY_UNKNOWN";
      const diagnostics = err instanceof SubscriptionRelayError ? err.diagnostics : undefined;
      console.error(
        "[idoris-router] 订阅中转失败：" +
          JSON.stringify({
            reason_code: reasonCode,
            message: err instanceof Error ? err.message : String(err),
            exit_code: diagnostics?.exitCode ?? null,
            stderr_bytes: diagnostics?.stderrBytes ?? null,
            stderr_sha256_12: diagnostics?.stderrDigest ?? null,
          }),
      );
      json(res, 502, {
        error: { type: "subscription_relay_failed", reason_code: reasonCode, message: "subscription relay failed" },
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
      ...(tenantId !== undefined ? { tenantId } : {}),
      signal: controller.signal,
      recordId,
      // C1：provider id 进缓存键 + 存进缓存条目，命中时用条目里记录的
      // Served-Locality 回放，不用这次重新选中的卡片现算；privacy 用于
      // fail-closed 复核（local_only 请求不能命中来源不是 loopback 的缓存）。
      providerId: target.card.provider.id,
      servedLocality,
      privacy: profile.privacy,
    },
  );

  // L1：命中幂等缓存时如实回报——调用方能看出这不是一次新的推理，而是同一个
  // X-iDoris-Request-Id 在缓存窗口内的重放，Origin-Record-Id 指回第一次落地的记录。
  if (result.cached) {
    res.setHeader("X-iDoris-Cached", "true");
    if (result.originRecordId !== undefined) res.setHeader("X-iDoris-Origin-Record-Id", result.originRecordId);
    // C1：Served-Locality 必须用缓存条目里记录的值覆盖——那才是当初真正产生
    // 这条响应时的值，不是"这次又重新算了一遍、恰好可能不一样"的值。缺失/
    // 非三值之一时按 remote fail-closed，跟 effectiveServedLocality() 的口径一致。
    const cachedLocality = result.servedLocality;
    const resolvedCachedLocality: ServedLocality =
      typeof cachedLocality === "string" && SERVED_LOCALITY_VALUES.has(cachedLocality) ? (cachedLocality as ServedLocality) : "remote";
    res.setHeader("X-iDoris-Served-Locality", resolvedCachedLocality);
    meta.servedLocality = resolvedCachedLocality;
  }

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
