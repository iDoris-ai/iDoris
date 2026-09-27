#!/usr/bin/env node
import { pathToFileURL } from "node:url";
import { startRouter, type Router } from "./server.js";

/**
 * 生产默认端口（FU-13）。原定 8765 与 Agent24 的 node mock daemon / 桌面 dev
 * 默认端口冲突（8796/8088/11434 也都已被占用），改用 8740。
 */
export const DEFAULT_PORT = 8740;
export const DEFAULT_COMPONENTS_DIR = "config/components";

/**
 * 解析 IDORIS_PORT。**非法值直接抛错，绝不静默回落到默认端口**——
 * 静默回落会让部署方以为服务监听在自己配的端口上，实际却在 8740，
 * 下游 Agent24 按错误端口连接会一直连不上却查不到原因。
 */
export function parsePort(raw: string | undefined): number {
  if (raw === undefined || raw.trim() === "") return DEFAULT_PORT;
  const trimmed = raw.trim();
  if (!/^\d+$/.test(trimmed)) {
    throw new Error(
      `IDORIS_PORT 不是合法的端口号："${raw}"。请设置为 1-65535 之间的整数，或不设置以使用默认值 ${DEFAULT_PORT}。`,
    );
  }
  const port = Number(trimmed);
  if (port < 1 || port > 65535) {
    throw new Error(`IDORIS_PORT 超出合法范围："${raw}"。端口必须在 1-65535 之间。`);
  }
  return port;
}

function nonEmpty(v: string | undefined): string | undefined {
  return v !== undefined && v.trim() !== "" ? v : undefined;
}

export interface ServeDeps {
  startRouter: typeof startRouter;
  log: (message: string) => void;
}

const defaultServeDeps: ServeDeps = { startRouter, log: (m) => console.log(m) };

/**
 * `idoris-router serve` 的实现体。拆成可注入 deps 的函数，方便测试直接在
 * 进程内断言「用什么端口/目录起了 Router、打印了什么」，不用真的 spawn 子进程
 * 去等它监听（那类端到端验证留给下面唯一一条真实子进程测试即可）。
 */
export async function serve(env: NodeJS.ProcessEnv, deps: ServeDeps = defaultServeDeps): Promise<Router> {
  const port = parsePort(env.IDORIS_PORT);
  const componentsDir = nonEmpty(env.IDORIS_COMPONENTS_DIR) ?? DEFAULT_COMPONENTS_DIR;
  const routingPolicyPath = nonEmpty(env.IDORIS_ROUTING_POLICY);
  const router = await deps.startRouter({
    port,
    componentsDir,
    ...(routingPolicyPath !== undefined ? { routingPolicyPath } : {}),
  });
  deps.log(`idoris-router listening on http://${router.host}:${String(router.port)}`);
  return router;
}

export async function main(argv: string[] = process.argv, env: NodeJS.ProcessEnv = process.env): Promise<void> {
  const cmd = argv[2];
  if (cmd !== "serve") {
    console.error("用法：idoris-router serve");
    process.exitCode = 1;
    return;
  }
  await serve(env);
}

// 只在直接作为可执行入口跑起来时才启动；被 import（例如测试里只想用 parsePort/serve）
// 不产生任何副作用。
const isMainModule = process.argv[1] !== undefined && import.meta.url === pathToFileURL(process.argv[1]).href;
if (isMainModule) {
  main().catch((err: unknown) => {
    console.error("[idoris-router] 启动失败：" + (err instanceof Error ? err.message : String(err)));
    process.exit(1);
  });
}
