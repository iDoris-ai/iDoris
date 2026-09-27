/**
 * T1.4.1 订阅中转的**沙箱档**（涉安全，fail-closed）。
 *
 * `claude` / `codex` 不是纯模型 provider，而是具备工具、工作区读写与子进程
 * 能力的 agent 程序。若直接放进可信推理路径，调用方就获得一条绕过上层审批门
 * 执行工具的通道。因此订阅中转只在**显式声明**的沙箱档下才允许构造；没有声明
 * 就拒绝注册，绝不"没沙箱也先跑起来"。
 *
 * 档位 `idoris-subscription-no-tools-readonly-v1` 做两件事：
 *   1) 用 CLI 自身的能力开关移除工具（claude: `--tools ""` + `--restricted`；
 *      codex: `--sandbox read-only`），并关闭 MCP / 用户配置 / 会话落盘；
 *   2) 把子进程 cwd 设为**只读目录（0555）**，即使工具开关被绕过，工作区写入
 *      也会被文件系统权限拒绝。
 *
 * **明确不保证**（不要把它当成内核级沙箱）：
 *   - 不限制网络目的地：CLI 必须联网读取自己已登录的凭据并访问厂商 API；
 *   - 不隔离用户凭据：CLI 仍能读取 `~/.claude` / `~/.codex` 下的登录态；
 *   - 不能阻止被攻破/被版本变更破坏的 CLI 自己写 `$HOME` 等 cwd 之外的位置。
 * 归结为：本档保证「模型侧没有工具、工作区不可写」，不保证「CLI 进程本身不联网、
 * 不读凭据、不写 $HOME」。做不到这两点，就不把它放进默认可信路径。
 */

/** 允许的沙箱档 id；想换档必须先改这里（显式、可审计）。 */
export const SUBSCRIPTION_SANDBOX_PROFILE_ID = "idoris-subscription-no-tools-readonly-v1";

export type SubscriptionCli = "claude" | "codex";

export interface SandboxProfile {
  readonly id: typeof SUBSCRIPTION_SANDBOX_PROFILE_ID;
  readonly cli: SubscriptionCli;
  /** 模型侧工具被移除。 */
  readonly tools: "off";
  /** 工作目录只读（0555），工作区写入失败。 */
  readonly workspace: "read_only";
}

export type SandboxProfileErrorCode =
  | "SANDBOX_PROFILE_MISSING"
  | "SANDBOX_PROFILE_UNKNOWN"
  | "SANDBOX_CLI_UNKNOWN";

export class SandboxProfileError extends Error {
  constructor(
    readonly code: SandboxProfileErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "SandboxProfileError";
  }
}

export interface SandboxEnv {
  readonly IDORIS_SUBSCRIPTION_SANDBOX?: string;
  readonly IDORIS_SUBSCRIPTION_CLI?: string;
  readonly [key: string]: string | undefined;
}

/**
 * 解析显式声明的沙箱档。任何缺失/未知取值都抛错（fail-closed），
 * **不提供默认档、不静默降级**。
 */
export function resolveSubscriptionSandbox(env: SandboxEnv): SandboxProfile {
  const declared = env.IDORIS_SUBSCRIPTION_SANDBOX?.trim();
  if (declared === undefined || declared === "") {
    throw new SandboxProfileError(
      "SANDBOX_PROFILE_MISSING",
      "subscription relay requires an explicit sandbox profile: set IDORIS_SUBSCRIPTION_SANDBOX=" +
        SUBSCRIPTION_SANDBOX_PROFILE_ID +
        " (no default, no unsandboxed fallback)",
    );
  }
  if (declared !== SUBSCRIPTION_SANDBOX_PROFILE_ID) {
    throw new SandboxProfileError(
      "SANDBOX_PROFILE_UNKNOWN",
      "unknown subscription sandbox profile: " + declared + " (expected " + SUBSCRIPTION_SANDBOX_PROFILE_ID + ")",
    );
  }
  const cliRaw = (env.IDORIS_SUBSCRIPTION_CLI ?? "claude").trim().toLowerCase();
  if (cliRaw !== "claude" && cliRaw !== "codex") {
    throw new SandboxProfileError("SANDBOX_CLI_UNKNOWN", "IDORIS_SUBSCRIPTION_CLI must be claude|codex, got: " + cliRaw);
  }
  return {
    id: SUBSCRIPTION_SANDBOX_PROFILE_ID,
    cli: cliRaw,
    tools: "off",
    workspace: "read_only",
  };
}
