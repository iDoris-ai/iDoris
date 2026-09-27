import { isAbsolute, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import type { ComponentCard } from "@idoris/contracts";
import { loadComponents, type Registered } from "./registry.js";
import { startRouter, type Router } from "./server.js";

/**
 * 仓库根目录：由本文件在 packages/router/src（或编译后的 dist）下的位置反推，
 * **不依赖 `process.cwd()`**（M2：LaunchAgent 常见 cwd=`/`，相对路径按 cwd 解析
 * 会直接找不到文件；src/ 和 dist/ 到仓库根的相对深度相同，两边都能算对）。
 */
function repoRootFromHere(): string {
  return join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
}

export const REPO_ROOT = repoRootFromHere();

/**
 * 生产默认端口（FU-13）。原定 8765 与 Agent24 的 node mock daemon / 桌面 dev
 * 默认端口冲突（8796/8088/11434 也都已被占用），改用 8740。
 */
export const DEFAULT_PORT = 8740;
export const DEFAULT_COMPONENTS_DIR = "config/components";
/** M1：缺省不再是"不配置就 503 policy_unconfigured"，而是仓库自带的默认路由策略。 */
export const DEFAULT_ROUTING_POLICY = "config/routing-policy.yaml";

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

/**
 * M2：相对路径一律相对**仓库根目录**解析，不是 cwd；绝对路径原样返回。
 * 导出出来是为了让报错信息里能打印"解析后的绝对路径"。
 */
export function resolveRepoPath(value: string): string {
  return isAbsolute(value) ? value : join(REPO_ROOT, value);
}

/**
 * 是否会被解析成 MockBackend——生产 CLI 默认不装它。
 *
 * PR #46 复审 M1：原判断只看 `endpoint.startsWith("mock://")`，但
 * `@idoris/adapters` 的 `createBackend`（factory.ts）**只认 `provider.id === "mock"`**，
 * 完全不看 endpoint 长什么样。按 endpoint 前缀判断，一张 `provider.id: mock`
 * 但 endpoint 写成看起来人畜无害的真实地址（如 `http://127.0.0.1:9999`）的卡就能
 * 绕过这道过滤，照样被解析成内存态的 MockBackend——现在改成跟 factory 同一个
 * 判定依据，两边永远不会走岔。
 */
function isMockCard(card: ComponentCard): boolean {
  return card.provider.id === "mock";
}

export interface ServeDeps {
  startRouter: typeof startRouter;
  loadComponents: typeof loadComponents;
  log: (message: string) => void;
  error: (message: string) => void;
}

const defaultServeDeps: ServeDeps = {
  startRouter,
  loadComponents,
  log: (m) => console.log(m),
  error: (m) => console.error(m),
};

function isAddrInUseError(err: unknown): boolean {
  return err instanceof Error && "code" in err && (err as NodeJS.ErrnoException).code === "EADDRINUSE";
}

/**
 * `idoris-router serve` 的实现体。拆成可注入 deps 的函数，方便测试直接在
 * 进程内断言「用什么端口/目录起了 Router、打印了什么」，不用真的 spawn 子进程
 * 去等它监听（那类端到端验证留给 cli.test.ts 里唯一一条真实子进程测试即可）。
 */
export async function serve(env: NodeJS.ProcessEnv, deps: ServeDeps = defaultServeDeps): Promise<Router> {
  const port = parsePort(env.IDORIS_PORT);
  const componentsDirRaw = nonEmpty(env.IDORIS_COMPONENTS_DIR) ?? DEFAULT_COMPONENTS_DIR;
  const routingPolicyRaw = nonEmpty(env.IDORIS_ROUTING_POLICY) ?? DEFAULT_ROUTING_POLICY;
  const componentsDir = resolveRepoPath(componentsDirRaw);
  const routingPolicyPath = resolveRepoPath(routingPolicyRaw);

  let allRegistered: Registered[];
  try {
    allRegistered = deps.loadComponents(componentsDir, { env });
  } catch (err) {
    throw new Error(
      `无法加载组件目录："${componentsDir}"（IDORIS_COMPONENTS_DIR=${JSON.stringify(componentsDirRaw)}，` +
        `非绝对路径按仓库根目录 "${REPO_ROOT}" 解析，不是当前工作目录 "${process.cwd()}"；` +
        "如果不是从仓库附近启动（例如 LaunchAgent），请把 IDORIS_COMPONENTS_DIR 设成绝对路径）。" +
        "原始错误：" +
        (err instanceof Error ? err.message : String(err)),
    );
  }

  // M1：生产 CLI 默认拒绝注册 mock 组件，除非显式 IDORIS_ALLOW_MOCK=1。
  const allowMock = env.IDORIS_ALLOW_MOCK === "1";
  const registered = allowMock ? allRegistered : allRegistered.filter((r) => !isMockCard(r.card));
  const skipped = allRegistered.length - registered.length;

  let router: Router;
  try {
    router = await deps.startRouter({ port, componentsDir, registered, routingPolicyPath });
  } catch (err) {
    if (isAddrInUseError(err)) {
      // L4：EADDRINUSE 是最常见的"看错误没头绪"场景，直接给出下一步动作。
      throw new Error(`端口 ${String(port)} 已被占用，请设置 IDORIS_PORT 换一个端口。`);
    }
    throw new Error(
      `启动失败（routingPolicyPath="${routingPolicyPath}"，` +
        `IDORIS_ROUTING_POLICY=${JSON.stringify(routingPolicyRaw)} 非绝对路径按仓库根目录 "${REPO_ROOT}" 解析）。` +
        "原始错误：" +
        (err instanceof Error ? err.message : String(err)),
    );
  }

  deps.log(`idoris-router listening on http://${router.host}:${String(router.port)}`);
  if (skipped > 0) {
    deps.log(`idoris-router: 已跳过 ${String(skipped)} 个 mock 组件（生产默认不注册；测试用途设 IDORIS_ALLOW_MOCK=1）`);
  }
  const componentList = router.registered.map((r) => `${r.card.provider.id}(${r.card.form})`).join(", ");
  deps.log(`idoris-router: 已注册组件 [${componentList || "无"}]`);
  return router;
}
