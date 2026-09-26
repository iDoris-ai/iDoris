import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, describe, expect, it } from "vitest";
import {
  SUBSCRIPTION_SANDBOX_PROFILE_ID,
  SubscriptionRegistrationError,
  SubscriptionRelay,
  SubscriptionRelayError,
  buildCliArgs,
  createSubscriptionBackend,
  decideSubscriptionRegistration,
  deployModeFromEnv,
  isPersonalDeployMode,
  openAIChatCompletion,
  resolveSubscriptionSandbox,
  sanitizeEnv,
} from "../subscription/index.js";

const PROFILE_ENV = {
  IDORIS_DEPLOY_MODE: "personal",
  IDORIS_ENABLE_SUBSCRIPTION: "1",
  IDORIS_SUBSCRIPTION_SANDBOX: SUBSCRIPTION_SANDBOX_PROFILE_ID,
};

describe("resolveSubscriptionSandbox - fail-closed", () => {
  it("refuses when no sandbox profile is declared", () => {
    expect(() => resolveSubscriptionSandbox({})).toThrowError(/IDORIS_SUBSCRIPTION_SANDBOX/);
  });
  it("refuses an unknown sandbox profile", () => {
    try {
      resolveSubscriptionSandbox({ IDORIS_SUBSCRIPTION_SANDBOX: "whatever" });
      throw new Error("should have thrown");
    } catch (err) {
      expect(err).toBeInstanceOf(Error);
      expect((err as { code?: string }).code).toBe("SANDBOX_PROFILE_UNKNOWN");
    }
  });
  it("refuses an unknown CLI", () => {
    expect(() =>
      resolveSubscriptionSandbox({ IDORIS_SUBSCRIPTION_SANDBOX: SUBSCRIPTION_SANDBOX_PROFILE_ID, IDORIS_SUBSCRIPTION_CLI: "gemini" }),
    ).toThrowError(/claude\|codex/);
  });
  it("accepts the declared profile and defaults to claude", () => {
    const p = resolveSubscriptionSandbox({ IDORIS_SUBSCRIPTION_SANDBOX: SUBSCRIPTION_SANDBOX_PROFILE_ID });
    expect(p.tools).toBe("off");
    expect(p.workspace).toBe("read_only");
    expect(p.cli).toBe("claude");
  });
  it("accepts codex as the CLI", () => {
    const p = resolveSubscriptionSandbox({
      IDORIS_SUBSCRIPTION_SANDBOX: SUBSCRIPTION_SANDBOX_PROFILE_ID,
      IDORIS_SUBSCRIPTION_CLI: "codex",
    });
    expect(p.cli).toBe("codex");
  });
});

describe("deploy mode + registration decision", () => {
  it("empty/unset deploy mode is personal; unknown values are not", () => {
    expect(deployModeFromEnv({})).toBe("personal");
    expect(deployModeFromEnv({ IDORIS_DEPLOY_MODE: "  " })).toBe("personal");
    expect(deployModeFromEnv({ IDORIS_DEPLOY_MODE: " TENANT " })).toBe("tenant");
    expect(isPersonalDeployMode({})).toBe(true);
    expect(isPersonalDeployMode({ IDORIS_DEPLOY_MODE: "community" })).toBe(false);
    expect(isPersonalDeployMode({ IDORIS_DEPLOY_MODE: "city" })).toBe(false);
  });

  it("refuses tenant / community / city", () => {
    for (const mode of ["tenant", "community", "city"]) {
      const d = decideSubscriptionRegistration({ IDORIS_DEPLOY_MODE: mode, IDORIS_ENABLE_SUBSCRIPTION: "1", IDORIS_SUBSCRIPTION_SANDBOX: SUBSCRIPTION_SANDBOX_PROFILE_ID });
      expect(d.action).toBe("refuse");
      expect(d.reason).toContain("deploy_mode=" + mode);
    }
  });

  it("skips when explicitly disabled", () => {
    const d = decideSubscriptionRegistration({ IDORIS_DEPLOY_MODE: "personal", IDORIS_DISABLE_SUBSCRIPTION: "1" });
    expect(d.action).toBe("skip");
  });

  it("skips when not enabled (fail-closed default, not an error)", () => {
    const d = decideSubscriptionRegistration({ IDORIS_DEPLOY_MODE: "personal" });
    expect(d.action).toBe("skip");
  });

  it("refuses enable-without-sandbox", () => {
    const d = decideSubscriptionRegistration({ IDORIS_DEPLOY_MODE: "personal", IDORIS_ENABLE_SUBSCRIPTION: "1" });
    expect(d.action).toBe("refuse");
    expect(d.reason).toContain("SANDBOX");
  });

  it("registers only with personal + enable + sandbox", () => {
    const d = decideSubscriptionRegistration(PROFILE_ENV);
    expect(d.action).toBe("register");
    expect(d.sandbox?.id).toBe(SUBSCRIPTION_SANDBOX_PROFILE_ID);
  });
});

describe("createSubscriptionBackend", () => {
  it("refuses non-personal deploy mode", () => {
    expect(() => createSubscriptionBackend({} as never, { env: { IDORIS_DEPLOY_MODE: "tenant" } })).toThrowError(
      SubscriptionRegistrationError,
    );
  });
  it("refuses when not enabled", () => {
    try {
      createSubscriptionBackend({} as never, { env: { IDORIS_DEPLOY_MODE: "personal" } });
      throw new Error("should have thrown");
    } catch (err) {
      expect((err as SubscriptionRegistrationError).code).toBe("SUBSCRIPTION_NOT_ENABLED");
    }
  });
  it("constructs only with the explicit sandbox and disposes cleanly", () => {
    const relay = createSubscriptionBackend({} as never, { env: PROFILE_ENV });
    expect(relay).toBeInstanceOf(SubscriptionRelay);
    expect(relay.modelId()).toBe("claude-subscription");
    relay.dispose();
  });
});

describe("CLI flags remove tools (the sandbox flag list)", () => {
  it("claude: --tools '' + --restricted, and never bypassPermissions", () => {
    const args = buildCliArgs("claude", "hi", "/tmp/out.txt");
    expect(args).toContain("--tools");
    expect(args[args.indexOf("--tools") + 1]).toBe("");
    expect(args).toContain("--restricted");
    expect(args).toContain("--strict-mcp-config");
    expect(args).toContain("--no-session-persistence");
    expect(args.join(" ")).not.toContain("dangerously-skip-permissions");
  });
  it("codex: read-only sandbox + ephemeral, prompt over stdin", () => {
    const args = buildCliArgs("codex", "hi", "/tmp/out.txt");
    expect(args).toContain("--sandbox");
    expect(args[args.indexOf("--sandbox") + 1]).toBe("read-only");
    expect(args).toContain("--ephemeral");
    expect(args[args.length - 1]).toBe("-");
    expect(args).toContain("-o");
  });
});

describe("sanitizeEnv", () => {
  it("strips known credentials and all IDORIS_* switches", () => {
    const env = sanitizeEnv({ PATH: "/usr/bin", ANTHROPIC_API_KEY: "secret", OPENAI_API_KEY: "secret", IDORIS_ENABLE_SUBSCRIPTION: "1" });
    expect(env.PATH).toBe("/usr/bin");
    expect(env.ANTHROPIC_API_KEY).toBeUndefined();
    expect(env.OPENAI_API_KEY).toBeUndefined();
    expect(env.IDORIS_ENABLE_SUBSCRIPTION).toBeUndefined();
  });
});

describe("openAIChatCompletion", () => {
  it("produces an OpenAI-compatible response", () => {
    const r = openAIChatCompletion("IDORIS_RELAY_OK", "subscription", "reply");
    expect(r.object).toBe("chat.completion");
    expect(r.choices[0]?.message.content).toBe("IDORIS_RELAY_OK");
    expect(r.choices[0]?.finish_reason).toBe("stop");
    expect(r.usage.total_tokens).toBeGreaterThan(0);
  });
});

// --- 进程清理 / 输出上限：用 fake CLI，确定性地验证 SIGTERM→SIGKILL 到整个进程组 ---

const sandbox = resolveSubscriptionSandbox({ IDORIS_SUBSCRIPTION_SANDBOX: SUBSCRIPTION_SANDBOX_PROFILE_ID, IDORIS_SUBSCRIPTION_CLI: "claude" });
const tmpDirs: string[] = [];
const scratch = mkdtempSync(join(tmpdir(), "idoris-sub-unit-"));
tmpDirs.push(scratch);

function writeScript(name: string, body: string): string {
  const p = join(scratch, name);
  writeFileSync(p, body, "utf8");
  return p;
}

function isAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

async function waitDead(pid: number, timeoutMs = 3000): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (!isAlive(pid)) return true;
    await new Promise((r) => setTimeout(r, 50));
  }
  return !isAlive(pid);
}

function makeRelay(script: string, opts: { timeoutMs?: number; maxOutputBytes?: number; env?: NodeJS.ProcessEnv } = {}): SubscriptionRelay {
  return new SubscriptionRelay({
    sandbox,
    command: process.execPath,
    buildArgs: () => [script],
    timeoutMs: opts.timeoutMs ?? 3000,
    killGraceMs: 200,
    ...(opts.maxOutputBytes === undefined ? {} : { maxOutputBytes: opts.maxOutputBytes }),
    env: opts.env ?? process.env,
  });
}

afterAll(() => {
  for (const d of tmpDirs) rmSync(d, { recursive: true, force: true });
});

describe("SubscriptionRelay process lifecycle", () => {
  it("parses CLI stdout into a reply", async () => {
    const script = writeScript("ok.mjs", 'process.stdout.write("IDORIS_RELAY_OK");');
    const relay = makeRelay(script);
    const res = await relay.chat({ model: "m", messages: [{ role: "user", content: "hi" }] });
    expect(res.content.trim()).toBe("IDORIS_RELAY_OK");
    relay.dispose();
  });

  it("fails when the CLI exits non-zero", async () => {
    const script = writeScript("fail.mjs", 'process.stderr.write("boom"); process.exit(3);');
    const relay = makeRelay(script);
    await expect(relay.chat({ model: "m", messages: [{ role: "user", content: "hi" }] })).rejects.toMatchObject({
      code: "RELAY_CLI_FAILED",
    });
    relay.dispose();
  });

  it("kills the WHOLE process group on timeout - no orphans", async () => {
    const pidFile = join(scratch, "slow.pid");
    const script = writeScript(
      "slow.mjs",
      [
        'import { spawn } from "node:child_process";',
        'import { writeFileSync } from "node:fs";',
        'const child = spawn(process.execPath, ["-e", "setTimeout(()=>{}, 60000)"], { stdio: "ignore" });',
        "writeFileSync(process.env.FAKE_PID_FILE, String(child.pid));",
        "setTimeout(() => {}, 60000);",
      ].join("\n"),
    );
    const relay = makeRelay(script, { timeoutMs: 600, env: { ...process.env, FAKE_PID_FILE: pidFile } });
    await expect(relay.chat({ model: "m", messages: [{ role: "user", content: "hi" }] })).rejects.toMatchObject({
      code: "RELAY_TIMEOUT",
    });
    const grandchild = Number(readFileSync(pidFile, "utf8").trim());
    expect(Number.isFinite(grandchild)).toBe(true);
    const parent = relay.processGroups[relay.processGroups.length - 1];
    expect(typeof parent).toBe("number");
    expect(await waitDead(parent as number)).toBe(true);
    expect(await waitDead(grandchild)).toBe(true);
    relay.dispose();
  });

  it("kills the CLI and rejects when output exceeds the cap", async () => {
    const script = writeScript("loud.mjs", 'process.stdout.write("x".repeat(4096));');
    const relay = makeRelay(script, { maxOutputBytes: 128 });
    await expect(relay.chat({ model: "m", messages: [{ role: "user", content: "hi" }] })).rejects.toMatchObject({
      code: "RELAY_OUTPUT_LIMIT",
    });
    relay.dispose();
  });

  it("reports a spawn failure instead of hanging", async () => {
    const relay = new SubscriptionRelay({ sandbox, command: "/nonexistent/idoris-fake-cli", killGraceMs: 100 });
    await expect(relay.chat({ model: "m", messages: [{ role: "user", content: "hi" }] })).rejects.toBeInstanceOf(
      SubscriptionRelayError,
    );
    relay.dispose();
  });
});
