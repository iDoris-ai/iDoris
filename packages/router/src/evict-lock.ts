export class OomError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "OomError";
  }
}

/**
 * per-backend 驱逐互斥锁（T1.3.5）。
 * `evict_to_load` 全程持锁，防止两个请求同时驱逐彼此需要的模型（活锁）；
 * 等锁超过 timeout 直接返回 `OomError`（映射为 503 oom），不无限等待。
 */
export class EvictionLock {
  private readonly tails = new Map<string, Promise<void>>();

  async withLock<T>(key: string, timeoutMs: number, fn: () => Promise<T>): Promise<T> {
    const prev = this.tails.get(key) ?? Promise.resolve();
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    const next = prev.then(() => gate);
    this.tails.set(key, next);

    let timer: ReturnType<typeof setTimeout> | undefined;
    const acquired = await Promise.race([
      prev.then(() => true),
      new Promise<boolean>((resolve) => {
        timer = setTimeout(() => resolve(false), timeoutMs);
      }),
    ]);
    if (timer !== undefined) clearTimeout(timer);

    if (!acquired) {
      release();
      if (this.tails.get(key) === next) this.tails.delete(key);
      throw new OomError("eviction lock wait for backend " + key + " exceeded " + timeoutMs + "ms");
    }

    try {
      return await fn();
    } finally {
      release();
      if (this.tails.get(key) === next) this.tails.delete(key);
    }
  }
}
