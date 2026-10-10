/**
 * 拉起被测服务（子进程）、等 /health 就绪、测试结束后杀掉。
 *
 * 被测服务的启动命令由 `IDORIS_CONFORMANCE_CMD`（或更可靠的 `IDORIS_CONFORMANCE_ARGV`）
 * 决定；缺省时假定 TS 参考实现已经 build 好（`packages/router/dist/cli.js`），直接用它
 * 当默认值，这样 `pnpm conformance` 不用额外配置也能跑起来。Rust 版接入时只需把
 * `IDORIS_CONFORMANCE_CMD`/`IDORIS_CONFORMANCE_ARGV` 换成 Rust 二进制的启动命令，
 * 本文件不用改一行。
 */
import { spawn, type ChildProcess } from "node:child_process";
import { fileURLToPath } from "node:url";
import { delimiter, join } from "node:path";
import { pickPort } from "./port.js";

/** conformance/ 包本身在仓库根下，往上两级就是仓库根。 */
export const repoRoot: string = fileURLToPath(new URL("../..", import.meta.url));

export const routingPolicyFixturePath: string = join(repoRoot, "conformance", "fixtures", "routing-policy.yaml");

function defaultCommand(): string {
  return process.execPath + " " + join(repoRoot, "packages", "router", "dist", "cli.js") + " serve";
}

/**
 * 极简 shell 风格分词：支持单引号/双引号包裹的参数（值内可以有空格），双引号内
 * 支持反斜杠转义。不支持变量展开、管道、通配符等真正的 shell 特性——这本来就不
 * 该是一个 shell，只是给 `IDORIS_CONFORMANCE_CMD` 一个比裸 `split(/\s+/)` 更能
 * 应付"路径里带空格"的场景。参数本身比较复杂（例如含引号或需要精确控制每一个
 * 数组元素）时，用 `IDORIS_CONFORMANCE_ARGV`（JSON 字符串数组）更可靠，见 README。
 */
export function splitCommandLine(cmd: string): string[] {
  const out: string[] = [];
  let current = "";
  let hasCurrent = false;
  let quote: "'" | '"' | undefined;
  for (let i = 0; i < cmd.length; i += 1) {
    const c = cmd[i] as string;
    if (quote !== undefined) {
      if (c === quote) {
        quote = undefined;
      } else if (quote === '"' && c === "\\" && i + 1 < cmd.length) {
        i += 1;
        current += cmd[i];
      } else {
        current += c;
      }
      continue;
    }
    if (c === "'" || c === '"') {
      quote = c;
      hasCurrent = true;
      continue;
    }
    if (/\s/.test(c)) {
      if (hasCurrent) {
        out.push(current);
        current = "";
        hasCurrent = false;
      }
      continue;
    }
    current += c;
    hasCurrent = true;
  }
  if (hasCurrent) out.push(current);
  return out;
}

interface ResolvedCommand {
  bin: string;
  args: string[];
}

/**
 * 解析被测服务的启动命令。优先级：`IDORIS_CONFORMANCE_ARGV`（JSON 字符串数组，
 * 精确、无歧义，推荐给带复杂参数的场景）> `IDORIS_CONFORMANCE_CMD`（裸字符串，
 * 用上面的极简分词器解析）> 内置默认值。
 */
function resolveCommand(): ResolvedCommand {
  const argvJson = process.env.IDORIS_CONFORMANCE_ARGV;
  if (argvJson !== undefined && argvJson.trim() !== "") {
    let parsed: unknown;
    try {
      parsed = JSON.parse(argvJson);
    } catch (err) {
      throw new Error(
        "IDORIS_CONFORMANCE_ARGV 不是合法 JSON：" + (err instanceof Error ? err.message : String(err)),
      );
    }
    if (!Array.isArray(parsed) || parsed.length === 0 || !parsed.every((x) => typeof x === "string")) {
      throw new Error('IDORIS_CONFORMANCE_ARGV 必须是非空的字符串数组，例如 ["/path/to/bin","serve"]');
    }
    const [bin, ...args] = parsed as string[];
    return { bin: bin as string, args };
  }
  const cmdline = process.env.IDORIS_CONFORMANCE_CMD ?? defaultCommand();
  const parts = splitCommandLine(cmdline);
  const bin = parts[0];
  if (bin === undefined) throw new Error("IDORIS_CONFORMANCE_CMD is empty");
  return { bin, args: parts.slice(1) };
}

/** 从环境变量里去掉所有 IDORIS_* key——不让运行 `pnpm conformance` 这个 shell 自己带的
 * 环境变量（比如开发者本机导出的 IDORIS_DEPLOY_MODE）意外泄漏进被测子进程，
 * 污染测试隔离性。调用方要显式声明的 IDORIS_* 变量走 `opts.env`，会在这一步之后
 * 重新叠加回去。 */
function withoutIdorisEnv(env: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
  const out: NodeJS.ProcessEnv = {};
  for (const [k, v] of Object.entries(env)) {
    if (!k.startsWith("IDORIS_")) out[k] = v;
  }
  return out;
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
  /** 不传沿用仓库根目录；显式指定可验证被测 CLI 的配置定位行为。 */
  cwd?: string;
  /** Test-only PATH prefixes (for controlled fake CLIs). Never reaches production code. */
  pathPrepend?: string[];
  env?: NodeJS.ProcessEnv;
  healthTimeoutMs?: number;
}

export interface RunningServer {
  baseUrl: string;
  port: number;
  stderrSoFar(): string;
  stop(): Promise<void>;
}

export type ConformanceStartupFailureKind = "spawn_error" | "exited_early" | "health_timeout";

/** spawnConformanceServer() 失败时抛出的结构化错误，方便用例断言退出码/失败类别。 */
export class ConformanceStartupError extends Error {
  readonly kind: ConformanceStartupFailureKind;
  /** 进程真实退出码；spawn 失败或健康检查超时后被我们主动杀掉的情形下为 null。 */
  readonly exitCode: number | null;

  constructor(kind: ConformanceStartupFailureKind, exitCode: number | null, message: string) {
    super(message);
    this.name = "ConformanceStartupError";
    this.kind = kind;
    this.exitCode = exitCode;
  }
}

/**
 * 跨平台地杀掉整个进程树。子进程以 `detached: true` 起（POSIX 上等于让它当自己
 * 那个新进程组的组长），杀的时候用 `-pid` 杀整个组，而不是只杀这一个 pid——
 * 被测服务如果自己又 fork/spawn 了子子进程（例如订阅中转那类会拉起外部 CLI 的
 * 组件），只杀顶层 pid 会留下孤儿。Windows 不支持负 pid 语义，退化成普通
 * `child.kill()`。
 */
function killTree(child: ChildProcess, signal: NodeJS.Signals): void {
  const pid = child.pid;
  if (pid === undefined) return;
  try {
    if (process.platform === "win32") {
      child.kill(signal);
    } else {
      process.kill(-pid, signal);
    }
  } catch {
    // 已经退出，或者进程组已经不在了，忽略。
  }
}

/** 轮询 /health 直到 200 或超时；子进程提前退出/启动失败则立即抛错并带上 stdio 供排查。 */
export async function spawnConformanceServer(opts: SpawnOptions): Promise<RunningServer> {
  const port = await pickPort();
  let adminPort = await pickPort();
  while (adminPort === port) adminPort = await pickPort();
  const { bin, args } = resolveCommand();

  const env: NodeJS.ProcessEnv = {
    ...withoutIdorisEnv(process.env),
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
    // Rust M4/B9 serves Admin on a second listener. Give every harness child
    // its own port so parallel conformance workers never contend on 8741.
    // TS ignores this variable, so the shared harness stays implementation-neutral.
    IDORIS_ADMIN_PORT: String(adminPort),
    IDORIS_COMPONENTS_DIR: opts.componentsDir,
  };
  if (opts.pathPrepend !== undefined && opts.pathPrepend.length > 0) {
    env.PATH = [...opts.pathPrepend, env.PATH ?? ""].filter((part) => part !== "").join(delimiter);
  }
  // 注意变量名是 IDORIS_ROUTING_POLICY，不是 IDORIS_ROUTING_POLICY_PATH
  // （packages/router/src/serve.ts 的 M1 实现就叫这个名字）。
  if (opts.routingPolicyPath !== undefined) env.IDORIS_ROUTING_POLICY = opts.routingPolicyPath;
  else delete env.IDORIS_ROUTING_POLICY;

  const child: ChildProcess = spawn(bin, args, { cwd: opts.cwd ?? repoRoot, env, detached: process.platform !== "win32" });

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
  // spawn 本身失败（命令不存在 ENOENT、没有执行权限 EACCES……）不会走 'exit'，
  // 只会走 'error'；不接这个事件的话，Node 对没有监听者的 'error' 会直接抛出，
  // 把整个测试进程带崩，而不是让调用方拿到一个"启动失败"的普通 rejection。
  let spawnError: Error | undefined;
  child.on("error", (err) => {
    spawnError = err;
  });

  // 失败即杀（整棵）子进程树再抛结构化错误——**不能只抛错不杀进程**：
  // 一个卡住不响应 /health 的被测实现（未来的 Rust 版调试阶段很可能出现）
  // 会在每条用到它的用例里都泄漏一个孤儿进程，多测几次就能把本机端口/
  // 进程表占满。
  const killAndThrow = (kind: ConformanceStartupFailureKind, exitCode: number | null, message: string): never => {
    killTree(child, "SIGKILL");
    throw new ConformanceStartupError(
      kind,
      exitCode,
      message + "\n--- stdout ---\n" + stdoutBuf + "\n--- stderr ---\n" + stderrBuf,
    );
  };

  const baseUrl = "http://127.0.0.1:" + String(port);
  const deadline = Date.now() + (opts.healthTimeoutMs ?? 15_000);
  for (;;) {
    if (spawnError !== undefined) {
      killAndThrow("spawn_error", null, "被测服务启动命令执行失败（" + bin + "）：" + spawnError.message);
    }
    if (exitInfo !== undefined) {
      killAndThrow("exited_early", exitInfo.code, "被测服务在 /health 就绪前退出，code=" + String(exitInfo.code));
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
      killAndThrow("health_timeout", null, "等待 /health 就绪超时");
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }

  return {
    baseUrl,
    port,
    stderrSoFar: () => stderrBuf,
    stop: async () => {
      if (exitInfo !== undefined) return;
      killTree(child, "SIGTERM");
      await new Promise<void>((resolve) => {
        const killTimer = setTimeout(() => {
          killTree(child, "SIGKILL");
        }, 3000);
        child.once("exit", () => {
          clearTimeout(killTimer);
          resolve();
        });
      });
    },
  };
}
