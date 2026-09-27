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
  /** 命中幂等缓存时，写入该缓存条目那次请求的 Record-Id（L1：X-iDoris-Origin-Record-Id）。 */
  originRecordId?: string;
  /**
   * 命中幂等缓存时，写入**当初真正产生这条响应**那次请求的 Served-Locality
   * （C1）。调用方（server.ts）必须原样用它设置响应头，不能用"这次重新选中的
   * 卡片"现算——两次即便选中同一个 provider.id，也该以历史记录为准，不依赖
   * "现算的等于当初的"这个假设。
   */
  servedLocality?: string;
}

const defaultSleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * 幂等缓存键。**必须含 tenant、endpoint 与 provider id，不能只用 requestId。**
 *
 * 只用 requestId 做键会造成**真实的跨租户数据泄漏**（评审 PR #25 实测到 B 租户
 * 拿到 A 租户的响应正文）：`X-iDoris-Request-Id` 是**调用方自己填的**，两个租户
 * 撞上同一个值（碰撞、复用模板、或恶意猜测）就会共享缓存。
 *
 * endpoint 也进键：同一个 requestId 若被路由到不同 provider，不该返回另一个
 * provider 的响应体。**但只有 endpoint 还不够**（PR #46 复审 C1，真实复现）：
 * 两张组件卡完全可能共用同一个物理 endpoint 字符串（同一个 127.0.0.1 端口，
 * 一张卡走本地隧道声明 locality: loopback，另一张卡实际转发到云端声明
 * locality: remote）。这种配置下，光按 endpoint 做键会让"隧道卡"的本地请求
 * 命中"云端卡"之前写下的缓存，把远程产生的内容当成本地响应重放出去，
 * Served-Locality 却按当前选中的（本地）卡片计算——响应体是远程来的，
 * 头却说是 loopback。provider id 进键，从根上让两张卡的缓存互不相通。
 *
 * 分隔符用 `\u0000`：它不能出现在 HTTP header 值里，所以不存在
 * 「tenant=`a:b` + id=`c`」与「tenant=`a` + id=`b:c`」撞键的歧义。
 */
function cacheKey(tenantScope: string, endpoint: string, providerScope: string, requestId: string): string {
  return tenantScope + "\u0000" + endpoint + "\u0000" + providerScope + "\u0000" + requestId;
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
  private readonly cache = new Map<
    string,
    { at: number; status: number; text: string; recordId?: string; providerId?: string; servedLocality?: string }
  >();

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
  private remember(
    key: string,
    value: { at: number; status: number; text: string; recordId?: string; providerId?: string; servedLocality?: string },
  ): void {
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
    opts: {
      stream: boolean;
      requestId?: string;
      tenantId?: string;
      signal?: AbortSignal;
      recordId?: string;
      /** 这次请求实际选中的 provider（进缓存键，也存进缓存条目——C1）。 */
      providerId?: string;
      /** 这次请求实际的 Served-Locality（写缓存时存下来；C1 命中时原样回放，不重算）。 */
      servedLocality?: string;
      /** 这次请求的隐私级别；命中时用来做 fail-closed 复核（C1）。 */
      privacy?: "local_only" | "any";
    },
  ): Promise<ForwardResult> {
    const url = endpoint.replace(/\/$/, "") + "/v1/chat/completions";
    const headers: Record<string, string> = { "content-type": "application/json" };
    if (apiKey !== undefined) headers.authorization = "Bearer " + apiKey;

    // deploy_mode=tenant 时 profile.ts 已保证 tenantId 存在（缺则 400 tenant_missing）；
    // personal 模式无租户维度，用固定 sentinel。两者永不共享键空间。
    const tenantScope = opts.tenantId ?? "\u0000personal";
    // 没有 providerId 的调用（理论上不该发生——server.ts 现在总会传）也不能落进
    // 同一个空字符串键，用固定 sentinel 隔开，避免意外互相命中。
    const providerScope = opts.providerId ?? "\u0000no-provider";
    if (!opts.stream && opts.requestId !== undefined) {
      const hit = this.cache.get(cacheKey(tenantScope, url, providerScope, opts.requestId));
      if (hit && this.now() - hit.at < this.windowMs) {
        // C1 fail-closed：这次请求若是 local_only，而这条缓存记录当初实际的
        // Served-Locality 不是 loopback（含压根没记录到，一律按不安全处理），
        // 就不能把它当命中吐回去——宁可当作没缓存，走一遍真实请求。
        const unsafeForLocalOnly = opts.privacy === "local_only" && hit.servedLocality !== "loopback";
        if (!unsafeForLocalOnly) {
          return {
            status: hit.status,
            text: hit.text,
            stream: null,
            retries: 0,
            cached: true,
            ...(hit.recordId !== undefined ? { originRecordId: hit.recordId } : {}),
            ...(hit.servedLocality !== undefined ? { servedLocality: hit.servedLocality } : {}),
          };
        }
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
          this.remember(cacheKey(tenantScope, url, providerScope, opts.requestId), {
            at: this.now(),
            status: res.status,
            text,
            ...(opts.recordId !== undefined ? { recordId: opts.recordId } : {}),
            ...(opts.providerId !== undefined ? { providerId: opts.providerId } : {}),
            ...(opts.servedLocality !== undefined ? { servedLocality: opts.servedLocality } : {}),
          });
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
