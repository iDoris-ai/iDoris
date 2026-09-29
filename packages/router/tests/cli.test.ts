import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, rmSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import type { ComponentCard } from "@idoris/contracts";
import {
  DEFAULT_COMPONENTS_DIR,
  DEFAULT_PORT,
  DEFAULT_ROUTING_POLICY,
  REPO_ROOT,
  parsePort,
  resolveRepoPath,
  serve,
  type ServeDeps,
} from "../src/serve.js";
import { startRouter, type Router } from "../src/server.js";
import type { Registered } from "../src/registry.js";

const testsDir = dirname(fileURLToPath(import.meta.url));
// 子进程测试跑编译产物 dist/cli.js，不是 src/cli.ts + --experimental-transform-types：
// 后者依赖的实验特性在 Node 22/24/26 上行为不一致（Node 26 直接 bad option 崩掉，
// Node 22 加载时会打一行含 " at any time" 的 ExperimentalWarning，污染 stderr 断言）。
// CI 的 `pnpm build` 已经在 `pnpm test` 之前跑过，dist/ 在测试时总是存在；本地手跑
// 这个文件前也需要先 `pnpm --filter @idoris/router build`。
const distCliPath = join(testsDir, "..", "dist", "cli.js");
const goodFixtures = join(testsDir, "fixtures", "good");
/** 双保险：子进程一律禁用 process warning，避免任何未来的警告污染 stderr 断言。 */
const NO_WARNINGS_ENV = { NODE_NO_WARNINGS: "1" };

/** FU-13：IDORIS_PORT 校验——非法值必须直接失败，绝不静默回落到默认端口。 */
describe("parsePort", () => {
  it("未设置 / 空字符串 → 生产默认端口", () => {
    expect(DEFAULT_PORT).toBe(8740);
    expect(parsePort(undefined)).toBe(8740);
    expect(parsePort("")).toBe(8740);
    expect(parsePort("   ")).toBe(8740);
  });

  it("合法整数原样返回", () => {
    expect(parsePort("9001")).toBe(9001);
    expect(parsePort("1")).toBe(1);
    expect(parsePort("65535")).toBe(65535);
  });

  it("非整数 → 抛错（不是回落默认值）", () => {
    expect(() => parsePort("abc")).toThrow(/IDORIS_PORT/);
    expect(() => parsePort("8080x")).toThrow(/IDORIS_PORT/);
  });

  it("小数 → 抛错（不悄悄截断成整数）", () => {
    expect(() => parsePort("8080.5")).toThrow(/IDORIS_PORT/);
  });

  it("负对照：越界（0、负数、>65535）→ 抛错", () => {
    expect(() => parsePort("0")).toThrow();
    expect(() => parsePort("-1")).toThrow();
    expect(() => parsePort("70000")).toThrow();
  });
});

/** M2：相对路径按仓库根目录解析，不是 cwd（LaunchAgent 常见 cwd=/）。 */
describe("resolveRepoPath / REPO_ROOT", () => {
  it("REPO_ROOT 指向真实仓库根（config/routing-policy.yaml 在它下面真实存在）", () => {
    expect(existsSync(join(REPO_ROOT, "config", "routing-policy.yaml"))).toBe(true);
    expect(resolveRepoPath("config/routing-policy.yaml")).toBe(join(REPO_ROOT, "config/routing-policy.yaml"));
  });

  it("相对路径按 REPO_ROOT 解析，不受 process.cwd() 影响", () => {
    const original = process.cwd();
    try {
      process.chdir(tmpdir());
      expect(resolveRepoPath("config/components")).toBe(join(REPO_ROOT, "config/components"));
    } finally {
      process.chdir(original);
    }
  });

  it("负对照：绝对路径原样返回，不会被 REPO_ROOT 再拼一层", () => {
    expect(resolveRepoPath("/tmp/x")).toBe("/tmp/x");
  });
});

function makeCard(id: string, endpoint: string): ComponentCard {
  return {
    provider: {
      id,
      family: "local",
      tier: "local",
      capabilities: ["chat"],
      privacy_class: "local_only",
      cost: { input_per_m: 0, output_per_m: 0 },
      locality: "loopback",
    },
    form: "http_service",
    endpoint,
    version_pin: "test@1",
    privacy_class: "local_only",
    allowed_egress: ["none"],
    fallback_policy: "fail_closed",
    fail_closed: true,
  } as unknown as ComponentCard;
}

/** serve() 用注入的 deps 在进程内断言接线是否正确，不用真的监听等待。 */
describe("serve()（进程内注入 deps）", () => {
  const fakeRouter = (port: number, registered: Registered[] = []): Router =>
    ({ host: "127.0.0.1", port, server: {}, registered, health: {}, close: () => Promise.resolve() }) as unknown as Router;

  function fakeDeps(overrides: Partial<ServeDeps> & { captured?: { startOpts?: unknown } } = {}): ServeDeps {
    const logs: string[] = [];
    return {
      loadComponents: overrides.loadComponents ?? (() => []),
      startRouter:
        overrides.startRouter ??
        ((opts) => {
          if (overrides.captured) overrides.captured.startOpts = opts;
          return Promise.resolve(fakeRouter(8740));
        }),
      log: overrides.log ?? ((m) => logs.push(m)),
      error: overrides.error ?? (() => undefined),
    };
  }

  it("未设置任何 IDORIS_* 时：componentsDir/routingPolicyPath 都落到仓库内置默认值", async () => {
    const captured: { startOpts?: unknown } = {};
    await serve({} as NodeJS.ProcessEnv, fakeDeps({ captured }));
    expect(captured.startOpts).toMatchObject({
      port: DEFAULT_PORT,
      componentsDir: join(REPO_ROOT, DEFAULT_COMPONENTS_DIR),
      routingPolicyPath: join(REPO_ROOT, DEFAULT_ROUTING_POLICY),
    });
  });

  it("显式设置的相对路径也按仓库根目录解析（不是 cwd）", async () => {
    const captured: { startOpts?: unknown } = {};
    await serve(
      { IDORIS_COMPONENTS_DIR: "config/components", IDORIS_ROUTING_POLICY: "config/routing-policy.yaml" } as NodeJS.ProcessEnv,
      fakeDeps({ captured }),
    );
    expect(captured.startOpts).toMatchObject({
      componentsDir: join(REPO_ROOT, "config/components"),
      routingPolicyPath: join(REPO_ROOT, "config/routing-policy.yaml"),
    });
  });

  it("绝对路径原样透传，不会被仓库根目录再拼一层", async () => {
    const captured: { startOpts?: unknown } = {};
    await serve(
      { IDORIS_COMPONENTS_DIR: "/tmp/abs-components", IDORIS_ROUTING_POLICY: "/tmp/abs-policy.yaml" } as NodeJS.ProcessEnv,
      fakeDeps({ captured }),
    );
    expect(captured.startOpts).toMatchObject({ componentsDir: "/tmp/abs-components", routingPolicyPath: "/tmp/abs-policy.yaml" });
  });

  it("M1：默认拒绝注册 mock 组件（endpoint 是 mock:// 的卡）", async () => {
    const real = makeCard("real", "http://127.0.0.1:9999");
    const mock = makeCard("mock", "mock://in-memory");
    const backend = { list: async () => [], load: async () => undefined, unload: async () => undefined, admission: async () => "coexist" as const, status: async () => ({ pressure: "ok" as const, usedGb: 0, modelMemoryMaxGb: 0, loaded: [] }), chat: async () => ({ model: "x", content: "x" }) };
    const captured: { startOpts?: unknown } = {};
    await serve({} as NodeJS.ProcessEnv, fakeDeps({ loadComponents: () => [{ card: real, backend }, { card: mock, backend }], captured }));
    const opts = captured.startOpts as { registered: Registered[] };
    expect(opts.registered.map((r) => r.card.provider.id)).toEqual(["real"]);
  });

  it("M1 回归（PR #46 复审）：provider.id 是 mock 但 endpoint 看起来正常的卡也会被拦下", async () => {
    // @idoris/adapters 的 factory.ts createBackend 只认 provider.id === "mock"
    // 来决定要不要解析成 MockBackend，完全不看 endpoint 长什么样——按 endpoint
    // 前缀过滤会被这种卡绕过（endpoint 写成人畜无害的样子，但 id 还是 mock）。
    const disguisedMock = makeCard("mock", "http://127.0.0.1:9999");
    const backend = { list: async () => [], load: async () => undefined, unload: async () => undefined, admission: async () => "coexist" as const, status: async () => ({ pressure: "ok" as const, usedGb: 0, modelMemoryMaxGb: 0, loaded: [] }), chat: async () => ({ model: "x", content: "x" }) };
    const captured: { startOpts?: unknown } = {};
    await serve({} as NodeJS.ProcessEnv, fakeDeps({ loadComponents: () => [{ card: disguisedMock, backend }], captured }));
    const opts = captured.startOpts as { registered: Registered[] };
    expect(opts.registered).toHaveLength(0);
  });

  it("M1：IDORIS_ALLOW_MOCK=1 时放行 mock 组件", async () => {
    const mock = makeCard("mock", "mock://in-memory");
    const backend = { list: async () => [], load: async () => undefined, unload: async () => undefined, admission: async () => "coexist" as const, status: async () => ({ pressure: "ok" as const, usedGb: 0, modelMemoryMaxGb: 0, loaded: [] }), chat: async () => ({ model: "x", content: "x" }) };
    const captured: { startOpts?: unknown } = {};
    await serve(
      { IDORIS_ALLOW_MOCK: "1" } as NodeJS.ProcessEnv,
      fakeDeps({ loadComponents: () => [{ card: mock, backend }], captured }),
    );
    const opts = captured.startOpts as { registered: Registered[] };
    expect(opts.registered.map((r) => r.card.provider.id)).toEqual(["mock"]);
  });

  it("负对照：IDORIS_PORT 非法时直接抛错，连 loadComponents 都不会调", async () => {
    let loadCalled = false;
    await expect(
      serve(
        { IDORIS_PORT: "not-a-port" } as NodeJS.ProcessEnv,
        fakeDeps({
          loadComponents: () => {
            loadCalled = true;
            return [];
          },
        }),
      ),
    ).rejects.toThrow(/IDORIS_PORT/);
    expect(loadCalled).toBe(false);
  });

  it("负对照：loadComponents 失败时报错信息带上解析后的绝对路径", async () => {
    await expect(
      serve(
        { IDORIS_COMPONENTS_DIR: "no/such/dir" } as NodeJS.ProcessEnv,
        fakeDeps({
          loadComponents: () => {
            throw new Error("boom");
          },
        }),
      ),
    ).rejects.toThrow(new RegExp(join(REPO_ROOT, "no/such/dir").replace(/[/\\]/g, ".")));
  });

  it("L4：EADDRINUSE 翻译成人话，提示改 IDORIS_PORT", async () => {
    const addrInUse = Object.assign(new Error("listen EADDRINUSE"), { code: "EADDRINUSE" });
    await expect(
      serve(
        { IDORIS_PORT: "9999" } as NodeJS.ProcessEnv,
        fakeDeps({
          startRouter: () => Promise.reject(addrInUse),
        }),
      ),
    ).rejects.toThrow(/9999.*IDORIS_PORT|IDORIS_PORT.*9999/);
  });

  it("正对照：用真实 startRouter + loadComponents 起服务，IDORIS_ALLOW_MOCK=1 时能打到 /health", async () => {
    const logs: string[] = [];
    const router = await serve(
      { IDORIS_PORT: "18740", IDORIS_COMPONENTS_DIR: goodFixtures, IDORIS_ALLOW_MOCK: "1" } as NodeJS.ProcessEnv,
      { startRouter, loadComponents: (await import("../src/registry.js")).loadComponents, log: (m) => logs.push(m), error: () => undefined },
    );
    try {
      expect(router.port).toBe(18740);
      expect(router.registered.length).toBe(1); // fixtures/good 唯一的卡是 mock，放行后应该在
      expect(logs.some((l) => l.includes("18740"))).toBe(true);
      const res = await fetch("http://127.0.0.1:18740/health");
      expect(res.status).toBe(200);
    } finally {
      await router.close();
    }
  });

  it("正对照：不设置 IDORIS_ALLOW_MOCK 时同一份 fixtures 会被过滤成 0 个组件，并打印跳过提示", async () => {
    const logs: string[] = [];
    const router = await serve(
      { IDORIS_PORT: "18742", IDORIS_COMPONENTS_DIR: goodFixtures } as NodeJS.ProcessEnv,
      { startRouter, loadComponents: (await import("../src/registry.js")).loadComponents, log: (m) => logs.push(m), error: () => undefined },
    );
    try {
      expect(router.registered.length).toBe(0);
      expect(logs.some((l) => l.includes("已跳过") && l.includes("mock"))).toBe(true);
    } finally {
      await router.close();
    }
  });
});

/** 真实子进程：验证 main() 的退出码与「不是裸 stack trace」这两件事，光测纯函数覆盖不到。 */
describe("idoris-router serve（真实子进程）", () => {
  it("IDORIS_PORT=abc → 启动失败、退出码非 0、错误信息说人话", () => {
    const res = spawnSync(process.execPath, [distCliPath, "serve"], {
      env: { ...process.env, ...NO_WARNINGS_ENV, IDORIS_PORT: "abc", IDORIS_COMPONENTS_DIR: goodFixtures },
      encoding: "utf8",
      timeout: 30_000,
    });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain("IDORIS_PORT");
    // 不是甩给用户的裸 stack trace——真正的 V8 栈帧行长这样：`    at foo (file:1:2)`，
    // 用 `/^\s+at /m` 而不是裸子串 " at "：后者会被"Transform Types ... at any time"
    // 这类完全无关的提示句误伤（这条提示现在也不会再出现了，因为压根不用那个实验标志）。
    expect(res.stderr).not.toMatch(/^\s+at /m);
    const stderrLines = res.stderr.trim().split("\n");
    expect(stderrLines.at(-1)).toMatch(/^\[idoris-router\] 启动失败：/);
  });

  it("负对照：不带 serve 子命令 → 打印用法、非 0 退出（不是默默什么都不做）", () => {
    const res = spawnSync(process.execPath, [distCliPath], {
      env: { ...process.env, ...NO_WARNINGS_ENV },
      encoding: "utf8",
      timeout: 30_000,
    });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain("用法");
  });
});

/**
 * H1 回归：cli.ts 之前用 `import.meta.url === pathToFileURL(process.argv[1]).href`
 * 判断是否直接执行，经软链调用时两边路径不相等，`main()` 从不跑、进程静默退出码 0。
 * 现在 cli.ts 无条件执行，这里用真实软链复现原来会失效的场景，证明确实能起服务。
 * 软链指向编译产物 dist/cli.js——跟真实 bin 用法（`node_modules/.bin/idoris-router`
 * 本身就是指向 dist/cli.js 的软链）完全一致，也不需要任何实验性加载标志。
 */
describe("cli.ts 经软链调用（H1 回归）", () => {
  it("通过指向 dist/cli.js 的软链启动，也能真正监听并响应 /health", async () => {
    const linkDir = mkdtempSync(join(tmpdir(), "idoris-cli-symlink-"));
    const linkPath = join(linkDir, "idoris-router-via-symlink.js");
    symlinkSync(distCliPath, linkPath);
    const child = spawn(process.execPath, [linkPath, "serve"], {
      env: { ...process.env, ...NO_WARNINGS_ENV, IDORIS_PORT: "18743", IDORIS_COMPONENTS_DIR: goodFixtures, IDORIS_ALLOW_MOCK: "1" },
    });
    let stdout = "";
    try {
      await new Promise<void>((resolve, reject) => {
        let settled = false;
        const timer = setTimeout(() => {
          if (!settled) {
            settled = true;
            reject(new Error("timeout waiting for listen line; stdout so far: " + stdout));
          }
        }, 10_000);
        child.stdout?.on("data", (c: Buffer) => {
          stdout += c.toString("utf8");
          if (!settled && stdout.includes("listening")) {
            settled = true;
            clearTimeout(timer);
            resolve();
          }
        });
        child.once("exit", (code) => {
          if (!settled) {
            settled = true;
            clearTimeout(timer);
            reject(new Error("child exited early (code " + String(code) + ") before it printed the listen line; stdout: " + stdout));
          }
        });
        child.once("error", (e) => {
          if (!settled) {
            settled = true;
            clearTimeout(timer);
            reject(e);
          }
        });
      });
      const res = await fetch("http://127.0.0.1:18743/health");
      expect(res.status).toBe(200);
    } finally {
      child.kill();
      rmSync(linkDir, { recursive: true, force: true });
    }
  });
});
