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

function describeTarget(args: unknown[]): { host: string; port?: number } | undefined {
  const first = args[0];
  if (typeof first === "object" && first !== null) {
    const o = first as { host?: string; port?: number; path?: string };
    if (o.path !== undefined) return undefined; // unix socket
    if (o.host !== undefined) return o.port === undefined ? { host: o.host } : { host: o.host, port: o.port };
    return undefined;
  }
  if (typeof first === "number" && typeof args[1] === "string") return { host: args[1], port: first };
  return undefined;
}

/**
 * socket 层出网探针（T1.3.6）：拦截 net.Socket.connect，记录所有非本机目的地。
 * 它测的是**启动期部署配置**是否自己出网，与运行期路由门禁（T1.3.3）是两个洞。
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
