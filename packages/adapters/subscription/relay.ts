/**
 * T1.4.1 subprocess 订阅中转适配器。
 *
 * 把已登录的 `claude` / `codex` 订阅态封成 OpenAI-compat provider：
 *   spawn CLI → 采集输出 → 解析成 OpenAI ChatCompletion。
 *
 * 硬约束：
 *   - 120s 超时；超时/取消走 **SIGTERM → 宽限 → SIGKILL 到整个进程组**，不留孤儿；
 *   - 子进程 `detached` 独立进程组，cwd 为只读沙箱目录（见 sandbox.ts）；
 *   - stdout+stderr 合并上限（默认 256 KiB），超限即杀进程并报错；
 *   - 子进程环境剔除已知凭据/内部开关。
 *
 * **不保证**网络目的地与凭据隔离 —— CLI 必须联网读取自己的登录态。
 */
import { spawn } from "node:child_process";
import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { randomUUID } from "node:crypto";
import type {
  Admission,
  BackendStatus,
  ChatMessage,
  ChatRequest,
  ChatResponse,
  ModelBackend,
  ModelInfo,
} from "../src/backend.js";
import type { SandboxProfile, SubscriptionCli } from "./sandbox.js";

export const DEFAULT_RELAY_TIMEOUT_MS = 120_000;
/** SIGTERM 之后给 CLI 的宽限时间，超时再 SIGKILL。 */
export const RELAY_KILL_GRACE_MS = 5_000;
export const DEFAULT_MAX_OUTPUT_BYTES = 256 * 1024;

export type SubscriptionRelayErrorCode =
  | "RELAY_CLI_FAILED"
  | "RELAY_EMPTY_OUTPUT"
  | "RELAY_SPAWN_FAILED"
  | "RELAY_OUTPUT_LIMIT"
  | "RELAY_TIMEOUT"
  | "RELAY_CANCELLED";

export class SubscriptionRelayError extends Error {
  constructor(
    readonly code: SubscriptionRelayErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "SubscriptionRelayError";
  }
}

export interface OpenAIChatCompletion {
  id: string;
  object: "chat.completion";
  created: number;
  model: string;
  choices: Array<{
    index: number;
    message: { role: "assistant"; content: string };
    finish_reason: "stop";
  }>;
  usage: { prompt_tokens: number; completion_tokens: number; total_tokens: number };
}

export interface SubscriptionRelayOptions {
  /** 已显式解析的沙箱档；构造 relay 的唯一合法前提。 */
  sandbox: SandboxProfile;
  /** 覆盖 CLI 可执行文件（测试注入 fake CLI 用）。默认取 sandbox.cli。 */
  command?: string;
  /** 覆盖参数构造（测试用）。 */
  buildArgs?: (prompt: string, outputFile: string) => string[];
  /** 工作目录；默认新建一个只读临时目录。 */
  cwd?: string;
  timeoutMs?: number;
  killGraceMs?: number;
  maxOutputBytes?: number;
  /** 传给子进程的环境；默认 process.env 经 sanitizeEnv 后使用。 */
  env?: NodeJS.ProcessEnv;
}

/** 子进程环境里必须剔除的已知凭据变量（凭据范围收窄；非内核级隔离）。 */
export const CREDENTIAL_ENV_KEYS: readonly string[] = [
  "ANTHROPIC_API_KEY",
  "ANTHROPIC_AUTH_TOKEN",
  "OPENAI_API_KEY",
  "AZURE_OPENAI_API_KEY",
  "GOOGLE_API_KEY",
  "GEMINI_API_KEY",
  "AWS_ACCESS_KEY_ID",
  "AWS_SECRET_ACCESS_KEY",
  "AWS_SESSION_TOKEN",
];

/**
 * 只保留 CLI 运行所需的环境，剔除已知凭据与所有 `IDORIS_*` 内部开关。
 * 注意：这不是凭据隔离（CLI 仍可读自己的 `~/.claude` 登录态）。
 */
export function sanitizeEnv(env: NodeJS.ProcessEnv = process.env): NodeJS.ProcessEnv {
  const out: NodeJS.ProcessEnv = {};
  for (const [key, value] of Object.entries(env)) {
    if (value === undefined) continue;
    if (CREDENTIAL_ENV_KEYS.includes(key)) continue;
    if (key.startsWith("IDORIS_")) continue;
    out[key] = value;
  }
  return out;
}

/** 该沙箱档在 CLI 层实际传入的参数（不含 prompt 的环境差异见 codex 的 stdin 模式）。 */
export function buildCliArgs(cli: SubscriptionCli, prompt: string, outputFile: string): string[] {
  if (cli === "claude") {
    // --tools "" 关闭全部内置工具；--restricted 去掉可执行代码的工具并忽略用户/项目设置；
    // --strict-mcp-config 不加载任何 MCP；--no-session-persistence 不落会话盘。
    return [
      "-p",
      prompt,
      "--output-format",
      "text",
      "--tools",
      "",
      "--restricted",
      "--strict-mcp-config",
      "--no-session-persistence",
      "--permission-prompts",
      "none",
    ];
  }
  // codex：read-only 沙箱 + ephemeral（不落会话）+ 忽略用户配置与规则；
  // prompt 走 stdin（末尾 "-"），避免以 "-" 开头的文本被当成 flag。
  return [
    "exec",
    "--sandbox",
    "read-only",
    "--ephemeral",
    "--skip-git-repo-check",
    "--ignore-user-config",
    "--ignore-rules",
    "--color",
    "never",
    "-o",
    outputFile,
    "-",
  ];
}

function buildPrompt(messages: readonly ChatMessage[]): string {
  const only = messages.length === 1 ? messages[0] : undefined;
  if (only !== undefined && only.role === "user") return only.content;
  return messages.map((m) => m.role + ": " + m.content).join("\n\n");
}

function estimateTokens(text: string): number {
  return text.length === 0 ? 0 : Math.max(1, Math.ceil(text.length / 4));
}

export function openAIChatCompletion(content: string, model: string, prompt: string): OpenAIChatCompletion {
  const promptTokens = estimateTokens(prompt);
  const completionTokens = estimateTokens(content);
  return {
    id: "chatcmpl-idoris-" + randomUUID(),
    object: "chat.completion",
    created: Math.floor(Date.now() / 1000),
    model,
    choices: [{ index: 0, message: { role: "assistant", content }, finish_reason: "stop" }],
    usage: { prompt_tokens: promptTokens, completion_tokens: completionTokens, total_tokens: promptTokens + completionTokens },
  };
}

interface CliRunResult {
  stdout: string;
  stderr: string;
  code: number | null;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * 订阅中转后端。构造即校验沙箱档；每次 `chat` spawn 一个 CLI。
 * 只实现非流式（CLI 非交互模式整块返回），符合 T1.4.1「不做 streaming」。
 */
export class SubscriptionRelay implements ModelBackend {
  readonly sandbox: SandboxProfile;
  /** 已 spawn 过的进程组（= detached 子进程 pid），供测试断言无孤儿。 */
  readonly processGroups: number[] = [];

  private readonly command: string;
  private readonly buildArgs: (prompt: string, outputFile: string) => string[];
  private readonly cwd: string;
  private readonly ownsCwd: boolean;
  private readonly controlDir: string;
  private readonly timeoutMs: number;
  private readonly killGraceMs: number;
  private readonly maxOutputBytes: number;
  private readonly env: NodeJS.ProcessEnv;

  constructor(opts: SubscriptionRelayOptions) {
    if (opts.sandbox.tools !== "off" || opts.sandbox.workspace !== "read_only") {
      throw new Error("refusing to construct a subscription relay outside the declared no-tools/read-only sandbox");
    }
    this.sandbox = opts.sandbox;
    this.command = opts.command ?? opts.sandbox.cli;
    this.buildArgs = opts.buildArgs ?? ((prompt, outputFile) => buildCliArgs(opts.sandbox.cli, prompt, outputFile));
    this.timeoutMs = opts.timeoutMs ?? DEFAULT_RELAY_TIMEOUT_MS;
    this.killGraceMs = opts.killGraceMs ?? RELAY_KILL_GRACE_MS;
    this.maxOutputBytes = opts.maxOutputBytes ?? DEFAULT_MAX_OUTPUT_BYTES;
    this.env = opts.env ?? process.env;

    if (opts.cwd === undefined) {
      this.cwd = mkdtempSync(join(tmpdir(), "idoris-subscription-ws-"));
      this.ownsCwd = true;
    } else {
      this.cwd = opts.cwd;
      this.ownsCwd = false;
    }
    // 强制只读：即使工具开关被绕过，工作区写入也会被权限拒绝。
    chmodSync(this.cwd, 0o555);
    this.controlDir = mkdtempSync(join(tmpdir(), "idoris-subscription-ctl-"));
  }

  modelId(): string {
    return this.sandbox.cli + "-subscription";
  }

  async list(): Promise<ModelInfo[]> {
    return [{ id: this.modelId(), memoryGb: 0 }];
  }

  async load(_id: string): Promise<void> {
    /* 订阅态无需本地加载。 */
  }

  async unload(_id: string): Promise<void> {
    /* 无本地占用。 */
  }

  async admission(_id: string): Promise<Admission> {
    return "coexist";
  }

  async status(): Promise<BackendStatus> {
    return { pressure: "ok", usedGb: 0, modelMemoryMaxGb: 0, loaded: [] };
  }

  async chat(req: ChatRequest): Promise<ChatResponse> {
    const completion = await this.complete(req.messages, req.model, req.signal);
    const content = completion.choices[0]?.message.content ?? "";
    return { model: completion.model, content };
  }

  /** 完整 OpenAI-compat 响应（T1.4.1 的交付形状）。 */
  async complete(messages: readonly ChatMessage[], model: string, signal?: AbortSignal): Promise<OpenAIChatCompletion> {
    const prompt = buildPrompt(messages);
    const outputFile = join(this.controlDir, "last-" + randomUUID() + ".txt");
    const args = this.buildArgs(prompt, outputFile);
    const run = await this.spawnCli(args, prompt, signal);
    let text = run.stdout;
    if (this.sandbox.cli === "codex" && existsSync(outputFile)) {
      const last = readFileSync(outputFile, "utf8").trim();
      if (last !== "") text = last;
    }
    text = text.trim();
    if (run.code !== 0) {
      throw new SubscriptionRelayError(
        "RELAY_CLI_FAILED",
        "subscription CLI exited with code " + String(run.code) + ": " + run.stderr.trim().slice(0, 500),
      );
    }
    if (text === "") {
      throw new SubscriptionRelayError("RELAY_EMPTY_OUTPUT", "subscription CLI produced empty output");
    }
    return openAIChatCompletion(text, model, prompt);
  }

  /** 清理临时目录；调用方负责确保没有正在运行的 chat。 */
  dispose(): void {
    try {
      rmSync(this.controlDir, { recursive: true, force: true });
    } catch {
      /* 清理失败不影响正确性。 */
    }
    if (this.ownsCwd) {
      try {
        chmodSync(this.cwd, 0o700);
        rmSync(this.cwd, { recursive: true, force: true });
      } catch {
        /* 同上。 */
      }
    }
  }

  private signalGroup(pgid: number, signal: NodeJS.Signals): void {
    try {
      process.kill(-pgid, signal);
    } catch (err) {
      // ESRCH = 进程组已不存在；其余（EPERM 等）忽略，避免把结算路径打崩。
      void (err as NodeJS.ErrnoException).code;
    }
  }

  private spawnCli(args: string[], prompt: string, signal?: AbortSignal): Promise<CliRunResult> {
    return new Promise<CliRunResult>((resolve, reject) => {
      const usesStdin = this.sandbox.cli === "codex";
      const child = spawn(this.command, args, {
        cwd: this.cwd,
        env: sanitizeEnv(this.env),
        detached: true,
        stdio: [usesStdin ? "pipe" : "ignore", "pipe", "pipe"],
      });
      const pid = child.pid;
      if (pid === undefined) {
        // spawn 失败（如 ENOENT）时 pid 为 undefined，随后还会发出 'error' 事件；
        // 先在 `finish` 之前挂一个空监听，避免未处理的 error 逃逸成进程级异常。
        child.on("error", () => {
          /* 已在下面 reject */
        });
        reject(new SubscriptionRelayError("RELAY_SPAWN_FAILED", "failed to spawn subscription CLI " + this.command));
        return;
      }
      this.processGroups.push(pid);

      let stdout = "";
      let stderr = "";
      let outputBytes = 0;
      let settled = false;
      let timedOut = false;
      let outputExceeded = false;

      const timer = setTimeout(() => {
        timedOut = true;
        void this.terminate(pid);
      }, this.timeoutMs);

      const onAbort = (): void => {
        void this.terminate(pid);
      };
      signal?.addEventListener("abort", onAbort, { once: true });
      if (signal?.aborted === true) onAbort();

      const finish = (fn: () => void): void => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        signal?.removeEventListener("abort", onAbort);
        // 防御性清理：父进程已退出后，杀掉同组里可能残留的孙进程。
        this.signalGroup(pid, "SIGKILL");
        fn();
      };

      const onData = (chunk: Buffer, sink: "stdout" | "stderr"): void => {
        if (settled) return;
        outputBytes += chunk.length;
        if (outputBytes > this.maxOutputBytes) {
          outputExceeded = true;
          void this.terminate(pid);
          return;
        }
        if (sink === "stdout") stdout += chunk.toString("utf8");
        else stderr += chunk.toString("utf8");
      };
      child.stdout?.on("data", (chunk: Buffer) => onData(chunk, "stdout"));
      child.stderr?.on("data", (chunk: Buffer) => onData(chunk, "stderr"));

      if (usesStdin && child.stdin !== null) {
        child.stdin.end(prompt, "utf8");
      }

      child.on("error", (err) => {
        finish(() => reject(new SubscriptionRelayError("RELAY_SPAWN_FAILED", "failed to spawn " + this.command + ": " + err.message)));
      });
      child.on("close", (code) => {
        finish(() => {
          if (outputExceeded) {
            reject(
              new SubscriptionRelayError(
                "RELAY_OUTPUT_LIMIT",
                "subscription CLI output exceeded " + String(this.maxOutputBytes) + " bytes",
              ),
            );
            return;
          }
          if (timedOut) {
            reject(new SubscriptionRelayError("RELAY_TIMEOUT", "subscription CLI timed out after " + String(this.timeoutMs) + "ms"));
            return;
          }
          if (signal?.aborted === true) {
            reject(new SubscriptionRelayError("RELAY_CANCELLED", "subscription CLI cancelled by caller"));
            return;
          }
          resolve({ stdout, stderr, code });
        });
      });
    });
  }

  /** SIGTERM 到整个进程组；宽限后仍存活则 SIGKILL。 */
  private async terminate(pgid: number): Promise<void> {
    this.signalGroup(pgid, "SIGTERM");
    await sleep(this.killGraceMs);
    this.signalGroup(pgid, "SIGKILL");
  }
}
