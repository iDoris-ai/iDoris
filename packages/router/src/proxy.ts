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
  /** 幂等缓存的条目上限（默认 1000）。超出后按插入序淘汰最旧的。 */
  maxCacheEntries?: number;
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
  private readonly maxCacheEntries: number;
  private readonly cache = new Map<string, { at: number; status: number; text: string }>();

  constructor(deps: ProxyDeps = {}) {
    this.fetchImpl = deps.fetchImpl ?? ((globalThis as unknown as { fetch: FetchLike }).fetch);
    this.now = deps.now ?? ((): number => Date.now());
    this.sleep = deps.sleep ?? defaultSleep;
    this.windowMs = deps.idempotencyWindowMs ?? 60_000;
    this.retryDelays = deps.retryDelaysMs ?? [250, 1000];
    this.maxCacheEntries = deps.maxCacheEntries ?? 1000;
  }

  /**
   * 写入缓存并回收 —— **不能只写不清**（评审 PR #25 第 2 项，真实复现）。
   *
   * 原实现只有 `cache.set()`，没有任何过期回收或上限：构造 1000 个各自不同、
   * 全部立即过期的 requestId，`cache.size` 依然是 1000，一个都没回收 ——
   * 这是**廉价的内存 DoS 面**（`X-iDoris-Request-Id` 由调用方自填，刷不同值即可）。
   *
   * ⚠️ 只在读命中时删过期条目是**不够的**：从不被再读的键永远不会被访问到。
   * 所以回收必须挂在**写**路径上。
   */
  private remember(key: string, value: { at: number; status: number; text: string }): void {
    // 先 delete 再 set：Map 对已存在的键做 set **不会**把它移到末尾，
    // 那会打破下面 prune() 依赖的「插入序 == 过期序」不变式。
    this.cache.delete(key);
    this.cache.set(key, value);
    this.prune();
  }

  private prune(): void {
    // 所有条目共用同一个 windowMs，且 remember() 维持了插入序 == 过期序，
    // 所以从头扫到第一个未过期的即可停 —— 摊销 O(已过期数)，不是每次 O(n)。
    const cutoff = this.now() - this.windowMs;
    for (const [k, v] of this.cache) {
      if (v.at > cutoff) break;
      this.cache.delete(k);
    }
    // 兜住「全都没过期但数量爆了」这种情况：按插入序淘汰最旧的。
    while (this.cache.size > this.maxCacheEntries) {
      const oldest = this.cache.keys().next();
      if (oldest.done === true) break;
      this.cache.delete(oldest.value);
    }
  }

  /** 仅供测试断言回收行为；生产代码不读它。 */
  cacheSizeForTest(): number {
    return this.cache.size;
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
          this.remember(cacheKey(tenantScope, url, opts.requestId), { at: this.now(), status: res.status, text });
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
