/**
 * 拉起被测服务（子进程）、等 /health 就绪、测试结束后杀掉。
 *
 * 被测服务的启动命令由 `IDORIS_CONFORMANCE_CMD` 决定；缺省时假定 TS 参考实现
 * 已经 build 好（`packages/router/dist/cli.js`），直接用它当默认值，这样
 * `pnpm conformance` 不用额外配置也能跑起来。Rust 版接入时只需把
 * `IDORIS_CONFORMANCE_CMD` 换成 Rust 二进制的启动命令，本文件不用改一行。
 */
import { spawn, type ChildProcess } from "node:child_process";
import { fileURLToPath } from "node:url";
import { join } from "node:path";
import { pickPort } from "./port.js";

/** conformance/ 包本身在仓库根下，往上两级就是仓库根。 */
export const repoRoot: string = fileURLToPath(new URL("../..", import.meta.url));

export const routingPolicyFixturePath: string = join(repoRoot, "conformance", "fixtures", "routing-policy.yaml");

function defaultCommand(): string {
  return process.execPath + " " + join(repoRoot, "packages", "router", "dist", "cli.js") + " serve";
}

export interface SpawnOptions {
  componentsDir: string;
  /**
   * 不传 = 走被测 CLI 自己的缺省值。TS 参考实现（`packages/router/src/serve.ts`，
   * M1）缺省会解析成仓库自带的 `config/routing-policy.yaml`，**不再是**"未配置
   * 就 503 policy_unconfigured"——那条路径现在只在库层直接调用 `startRouter()`
   * 时才够得到，走生产 CLI 已经不可达，见 `tests/default-routing-policy.test.ts`。
   */
  routingPolicyPath?: string;
  env?: NodeJS.ProcessEnv;
  healthTimeoutMs?: number;
}

export interface RunningServer {
  baseUrl: string;
  port: number;
  stderrSoFar(): string;
  stop(): Promise<void>;
}

/** 轮询 /health 直到 200 或超时；子进程提前退出则立即抛错并带上 stdio 供排查。 */
export async function spawnConformanceServer(opts: SpawnOptions): Promise<RunningServer> {
  const port = await pickPort();
  const cmdline = process.env.IDORIS_CONFORMANCE_CMD ?? defaultCommand();
  const parts = cmdline.split(/\s+/).filter((s) => s.length > 0);
  const bin = parts[0];
  if (bin === undefined) throw new Error("IDORIS_CONFORMANCE_CMD is empty");
  const args = parts.slice(1);

  const env: NodeJS.ProcessEnv = {
    ...process.env,
    // 生产 CLI 默认拒绝注册 mock 组件（M1），conformance 的 fixtures 只用
    // provider.id: omlx（真实发 HTTP 请求），理论上用不到，但显式打开更保险
    // ——万一以后哪个 fixture 改用 mock id，不会因为漏了这个 env 而莫名其妙
    // "组件数是 0"。可以被 opts.env 覆盖。
    IDORIS_ALLOW_MOCK: "1",
    // 与 packages/router/tests/cli.test.ts（main 上 #46 的最终行为）同款双保险：
    // 禁用 Node 的 process warning，避免任何未来的实验特性/弃用警告混进被测
    // 进程的 stderr，污染我们在 spawnConformanceServer 失败时打印的 stderr 排查信息。
    NODE_NO_WARNINGS: "1",
    ...opts.env,
    IDORIS_PORT: String(port),
    IDORIS_COMPONENTS_DIR: opts.componentsDir,
  };
  // 注意变量名是 IDORIS_ROUTING_POLICY，不是 IDORIS_ROUTING_POLICY_PATH
  // （packages/router/src/serve.ts 的 M1 实现就叫这个名字）。
  if (opts.routingPolicyPath !== undefined) env.IDORIS_ROUTING_POLICY = opts.routingPolicyPath;
  else delete env.IDORIS_ROUTING_POLICY;

  const child: ChildProcess = spawn(bin, args, { cwd: repoRoot, env });

  let stdoutBuf = "";
  let stderrBuf = "";
  child.stdout?.on("data", (d: Buffer) => {
    stdoutBuf += d.toString("utf8");
  });
  child.stderr?.on("data", (d: Buffer) => {
    stderrBuf += d.toString("utf8");
  });
  let exitInfo: { code: number | null } | undefined;
  child.on("exit", (code) => {
    exitInfo = { code };
  });

  // 失败即杀子进程再抛错——**不能只抛错不杀进程**：无论是"等超时"还是"提前退出",
  // 只有后者天然已经没有进程了，前者如果不主动 kill，一个卡住不响应 /health 的
  // 被测实现（未来的 Rust 版调试阶段很可能出现）会在每条用到它的用例里都泄漏一个
  // 孤儿进程，多测几次就能把本机端口/进程表占满。
  const killAndThrow = (message: string): never => {
    try {
      child.kill("SIGKILL");
    } catch {
      // 已经退出，忽略。
    }
    throw new Error(message + "\n--- stdout ---\n" + stdoutBuf + "\n--- stderr ---\n" + stderrBuf);
  };

  const baseUrl = "http://127.0.0.1:" + String(port);
  const deadline = Date.now() + (opts.healthTimeoutMs ?? 15_000);
  for (;;) {
    if (exitInfo !== undefined) {
      killAndThrow("被测服务在 /health 就绪前退出，code=" + String(exitInfo.code));
    }
    try {
      // 单次探测也要有超时：被测实现如果接受了 TCP 连接却挂着不响应（不是拒绝
      // 连接，是连上了不回），裸 fetch 没有默认超时，会让这个循环卡死，
      // 外层的 deadline 检查永远等不到执行——健康检查本身必须先对被测服务
      // fail-closed，不能假设它一定会好好回应或好好拒绝。
      const res = await fetch(baseUrl + "/health", { signal: AbortSignal.timeout(2_000) });
      if (res.status === 200) break;
    } catch {
      // 端口还没起来，或者单次探测超时：继续等，由外层 deadline 兜底。
    }
    if (Date.now() > deadline) {
      killAndThrow("等待 /health 就绪超时");
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }

  return {
    baseUrl,
    port,
    stderrSoFar: () => stderrBuf,
    stop: async () => {
      if (exitInfo !== undefined) return;
      child.kill("SIGTERM");
      await new Promise<void>((resolve) => {
        const killTimer = setTimeout(() => {
          try {
            child.kill("SIGKILL");
          } catch {
            // 已经退出，忽略。
          }
        }, 3000);
        child.once("exit", () => {
          clearTimeout(killTimer);
          resolve();
        });
      });
    },
  };
}
