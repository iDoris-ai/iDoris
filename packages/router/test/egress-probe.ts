import { Socket } from "node:net";

const LOOPBACK_HOSTS = new Set(["127.0.0.1", "::1", "localhost", "0.0.0.0", "::"]);

export interface EgressProbe {
  /** 被抓到的非本机目的地（host[:port]）。 */
  outbound: string[];
  restore(): void;
}

function isLoopback(host: string): boolean {
  return LOOPBACK_HOSTS.has(host) || host.startsWith("127.");
}

/**
 * 从 `Socket.prototype.connect` 的实参里解析出目的地。
 *
 * ⚠️ **这里是原版失明的真正位置（评审 PR #23，根因经实测修正）**。
 * 评审把盲区归因于「undici 不经过这个函数」；实测（Node v24.21 / darwin / vitest）
 * 显示**函数确实被调用了**，是本函数把它们静默丢掉的。两处：
 *
 * 1. **实参会被包成数组**。Node 内部走 `_normalizeArgs()`，于是真实到达的是
 *    `connect.apply(this, [[{host,port,…}, cb]])` —— `args[0]` 是个 **Array**。
 *    而 `typeof array === "object"` 为真，`array.host` 为 `undefined`，于是
 *    直接 `return undefined`。fetch()/http.request 全从这里漏掉。
 * 2. **`path: null` 被当成 unix socket**。`http.request` 传 `path: null`，
 *    而原判断是 `if (o.path !== undefined)` —— `null !== undefined` 为真，
 *    于是即使解开了数组也照样被丢。
 *
 * 原版的正对照用 `connect(80, "host")`，恰好命中下面 `(port, host)` 那个
 * **唯一可用的分支**，所以它一直是绿的。这正是「正对照必须走真实调用方
 * 会用的路径」的教科书案例：**用被测 API 自己做正对照，只能证明它对自己有效。**
 */
function describeTarget(args: unknown[]): { host: string; port?: number } | undefined {
  const first = args[0];

  // (1) 解开 Node `_normalizeArgs` 的数组包装，递归一层。
  if (Array.isArray(first)) return describeTarget(first as unknown[]);

  if (typeof first === "object" && first !== null) {
    const o = first as { host?: string; port?: number; path?: string | null };
    // (2) 只有**非空字符串** path 才是 unix socket；null / "" 都不是。
    if (typeof o.path === "string" && o.path.length > 0) return undefined;
    if (o.host !== undefined) return o.port === undefined ? { host: o.host } : { host: o.host, port: o.port };
    return undefined;
  }

  // connect(port, host) / connect(port, host, cb)
  if (typeof first === "number" && typeof args[1] === "string") return { host: args[1], port: first };
  // connect(path) —— unix socket，不算出网
  if (typeof first === "string") return undefined;
  return undefined;
}

/**
 * socket 层出网探针（T1.3.6）：拦截 `net.Socket.prototype.connect`，记录所有非本机目的地。
 * 它测的是**启动期部署配置**是否自己出网，与运行期路由门禁（T1.3.3）是两个洞。
 *
 * ## 为什么这里是拦截点
 *
 * Node 里 `fetch()`（undici）、`http.request`、`net.connect`、`tls.connect` 最终都会
 * 走到 `Socket.prototype.connect`，所以它是收敛点。**但这是运行时内部实现，不是
 * 契约** —— undici 换个连接路径、或某个 Node 版本走了别的分支，这个拦截点就会
 * 失明，而「零出网」的断言会**照样变绿**。
 *
 * ## 所以覆盖面必须由正对照来保证，不能由推理保证（评审 PR #23）
 *
 * 评审指出原版的正对照用的是 `new Socket().connect()` —— **和被拦截的 API 是同一个**，
 * 于是它只证明了「拦截逻辑对它自己有效」，没有证明「拦截逻辑覆盖了真实调用方会
 * 用的路径」。真实断言走的是 `fetch()`，而它有没有被覆盖，原版一个字都没测。
 *
 * 复核记录：在 Node v24.21 / darwin 上实测 `fetch()` 与 `http.request` **都**被
 * 这个拦截点抓到，评审报告的盲区在此环境不复现（评审自己也标注了「环境相关」）。
 * **但这不构成「不用修」的理由** —— 一个覆盖面依赖运行时内部实现的探针，
 * 哪天不成立时必须有东西喊出来，而不是静静变绿。故 `egress.test.ts` 为每条
 * 真实调用路径各配一个正对照：任一条没被抓到，那条正对照就红，**探针自己
 * 宣告失明**，而不是让零出网断言替它背书。
 */
export function installEgressProbe(): EgressProbe {
  const outbound: string[] = [];
  const original = Socket.prototype.connect;
  Socket.prototype.connect = function patched(this: Socket, ...args: unknown[]): Socket {
    const target = describeTarget(args);
    if (target && !isLoopback(target.host)) {
      outbound.push(target.port === undefined ? target.host : target.host + ":" + target.port);
    }
    return (original as (...a: unknown[]) => Socket).apply(this, args);
  } as typeof Socket.prototype.connect;
  return {
    outbound,
    restore: () => {
      Socket.prototype.connect = original;
    },
  };
}
