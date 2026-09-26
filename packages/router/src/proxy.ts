export interface ReadableStreamLike {
  getReader(): {
    read(): Promise<{ done: boolean; value?: Uint8Array }>;
    cancel(reason?: unknown): Promise<void>;
  };
}

export interface FetchResponseLike {
  status: number;
  ok: boolean;
  text(): Promise<string>;
  body: ReadableStreamLike | null;
}

export interface FetchLike {
  (
    url: string,
    init: { method?: string; headers?: Record<string, string>; body?: string; signal?: AbortSignal },
  ): Promise<FetchResponseLike>;
}

export interface ProxyDeps {
  fetchImpl?: FetchLike;
  now?: () => number;
  sleep?: (ms: number) => Promise<void>;
  idempotencyWindowMs?: number;
  retryDelaysMs?: number[];
}

export interface ForwardResult {
  status: number;
  text: string;
  stream: ReadableStreamLike | null;
  retries: number;
  cached: boolean;
}

const defaultSleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * 幂等缓存键。**必须含 tenant 与 endpoint，不能只用 requestId。**
 *
 * 只用 requestId 做键会造成**真实的跨租户数据泄漏**（评审 PR #25 实测到 B 租户
 * 拿到 A 租户的响应正文）：`X-iDoris-Request-Id` 是**调用方自己填的**，两个租户
 * 撞上同一个值（碰撞、复用模板、或恶意猜测）就会共享缓存。
 *
 * endpoint 也进键：同一个 requestId 若被路由到不同 provider，不该返回另一个
 * provider 的响应体。
 *
 * 分隔符用 `\u0000`：它不能出现在 HTTP header 值里，所以不存在
 * 「tenant=`a:b` + id=`c`」与「tenant=`a` + id=`b:c`」撞键的歧义。
 */
function cacheKey(tenantScope: string, endpoint: string, requestId: string): string {
  return tenantScope + "\u0000" + endpoint + "\u0000" + requestId;
}

/**
 * `/v1/chat/completions` 转发（T1.3.4）。
 * - 非流式：最多 2 次退避重试（250ms → 1s）；可选 `X-iDoris-Request-Id` 幂等（60s 窗口）。
 * - **流式：一旦开始吐 token 就不再重试**（重试会导致重复 token）。
 * - 取消：调用方传 `signal`（客户端断开时 abort），透传给上游。
 */
export class ChatProxy {
  private readonly fetchImpl: FetchLike;
  private readonly now: () => number;
  private readonly sleep: (ms: number) => Promise<void>;
  private readonly windowMs: number;
  private readonly retryDelays: number[];
  private readonly cache = new Map<string, { at: number; status: number; text: string }>();

  constructor(deps: ProxyDeps = {}) {
    this.fetchImpl = deps.fetchImpl ?? ((globalThis as unknown as { fetch: FetchLike }).fetch);
    this.now = deps.now ?? ((): number => Date.now());
    this.sleep = deps.sleep ?? defaultSleep;
    this.windowMs = deps.idempotencyWindowMs ?? 60_000;
    this.retryDelays = deps.retryDelaysMs ?? [250, 1000];
  }

  async forward(
    endpoint: string,
    apiKey: string | undefined,
    body: Record<string, unknown>,
    opts: { stream: boolean; requestId?: string; tenantId?: string; signal?: AbortSignal },
  ): Promise<ForwardResult> {
    const url = endpoint.replace(/\/$/, "") + "/v1/chat/completions";
    const headers: Record<string, string> = { "content-type": "application/json" };
    if (apiKey !== undefined) headers.authorization = "Bearer " + apiKey;

    // deploy_mode=tenant 时 profile.ts 已保证 tenantId 存在（缺则 400 tenant_missing）；
    // personal 模式无租户维度，用固定 sentinel。两者永不共享键空间。
    const tenantScope = opts.tenantId ?? "\u0000personal";
    if (!opts.stream && opts.requestId !== undefined) {
      const hit = this.cache.get(cacheKey(tenantScope, url, opts.requestId));
      if (hit && this.now() - hit.at < this.windowMs) {
        return { status: hit.status, text: hit.text, stream: null, retries: 0, cached: true };
      }
    }

    const payload = JSON.stringify({ ...body, stream: opts.stream });
    let attempt = 0;
    for (;;) {
      try {
        const init: { method: string; headers: Record<string, string>; body: string; signal?: AbortSignal } = {
          method: "POST",
          headers,
          body: payload,
        };
        if (opts.signal !== undefined) init.signal = opts.signal;
        const res = await this.fetchImpl(url, init);
        if (!res.ok) {
          if (!opts.stream && res.status >= 500 && attempt < this.retryDelays.length) {
            await this.sleep(this.retryDelays[attempt] ?? 1000);
            attempt += 1;
            continue;
          }
          return { status: res.status, text: await res.text(), stream: null, retries: attempt, cached: false };
        }
        if (opts.stream) {
          return { status: res.status, text: "", stream: res.body, retries: attempt, cached: false };
        }
        const text = await res.text();
        if (opts.requestId !== undefined)
          this.cache.set(cacheKey(tenantScope, url, opts.requestId), { at: this.now(), status: res.status, text });
        return { status: res.status, text, stream: null, retries: attempt, cached: false };
      } catch (err) {
        if (opts.signal?.aborted === true) {
          return { status: 499, text: JSON.stringify({ error: { type: "client_closed" } }), stream: null, retries: attempt, cached: false };
        }
        if (!opts.stream && attempt < this.retryDelays.length) {
          await this.sleep(this.retryDelays[attempt] ?? 1000);
          attempt += 1;
          continue;
        }
        return { status: 502, text: JSON.stringify({ error: { type: "upstream_unavailable", message: String(err) } }), stream: null, retries: attempt, cached: false };
      }
    }
  }
}
