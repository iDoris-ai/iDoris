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
    opts: { stream: boolean; requestId?: string; signal?: AbortSignal },
  ): Promise<ForwardResult> {
    const url = endpoint.replace(/\/$/, "") + "/v1/chat/completions";
    const headers: Record<string, string> = { "content-type": "application/json" };
    if (apiKey !== undefined) headers.authorization = "Bearer " + apiKey;

    if (!opts.stream && opts.requestId !== undefined) {
      const hit = this.cache.get(opts.requestId);
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
        if (opts.requestId !== undefined) this.cache.set(opts.requestId, { at: this.now(), status: res.status, text });
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
