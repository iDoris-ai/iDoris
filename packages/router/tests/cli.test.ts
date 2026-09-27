import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { DEFAULT_COMPONENTS_DIR, DEFAULT_PORT, parsePort, serve, type ServeDeps } from "../src/cli.js";
import { startRouter, type Router } from "../src/server.js";

const testsDir = dirname(fileURLToPath(import.meta.url));
const helpers = join(testsDir, "helpers");
const registerPath = join(helpers, "ts-register.mjs");
const cliPath = join(testsDir, "..", "src", "cli.ts");
const goodFixtures = join(testsDir, "fixtures", "good");

/**
 * FU-13：`IDORIS_PORT` 校验——非法值必须直接失败，绝不静默回落到默认端口
 * （静默回落会让部署方以为服务在自己配的端口上，实际却在别处，下游连不上还查不出原因）。
 */
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

/** serve() 用注入的 deps 在进程内断言接线是否正确，不用真的监听等待。 */
describe("serve()（进程内注入 deps）", () => {
  function fakeDeps(logs: string[], captured: { opts?: unknown }, router: Router): ServeDeps {
    return {
      startRouter: (opts) => {
        captured.opts = opts;
        return Promise.resolve(router);
      },
      log: (m) => logs.push(m),
    };
  }

  const fakeRouter = (port: number): Router =>
    ({ host: "127.0.0.1", port, server: {}, registered: [], health: {}, close: () => Promise.resolve() }) as unknown as Router;

  it("未设置 IDORIS_COMPONENTS_DIR 时用 config/components 缺省值", async () => {
    const logs: string[] = [];
    const captured: { opts?: unknown } = {};
    const router = await serve({ IDORIS_PORT: "9010" } as NodeJS.ProcessEnv, fakeDeps(logs, captured, fakeRouter(8740)));
    expect(router.port).toBe(8740);
    expect(captured.opts).toMatchObject({ port: 9010, componentsDir: DEFAULT_COMPONENTS_DIR });
    expect(captured.opts).not.toHaveProperty("routingPolicyPath");
    expect(logs[0]).toContain("8740"); // 打印的是实际拿到的 Router 地址
  });

  it("IDORIS_COMPONENTS_DIR / IDORIS_ROUTING_POLICY 会透传给 startRouter", async () => {
    const logs: string[] = [];
    const captured: { opts?: unknown } = {};
    await serve(
      { IDORIS_PORT: "9020", IDORIS_COMPONENTS_DIR: "/tmp/x", IDORIS_ROUTING_POLICY: "/tmp/policy.yaml" } as NodeJS.ProcessEnv,
      fakeDeps(logs, captured, fakeRouter(9020)),
    );
    expect(captured.opts).toMatchObject({ port: 9020, componentsDir: "/tmp/x", routingPolicyPath: "/tmp/policy.yaml" });
  });

  it("负对照：IDORIS_PORT 非法时直接抛错，绝不去调 startRouter", async () => {
    let called = false;
    await expect(
      serve({ IDORIS_PORT: "not-a-port" } as NodeJS.ProcessEnv, {
        startRouter: () => {
          called = true;
          return Promise.reject(new Error("不该被调用"));
        },
        log: () => undefined,
      }),
    ).rejects.toThrow(/IDORIS_PORT/);
    expect(called).toBe(false);
  });

  it("正对照：用真实 startRouter 起服务，端口/组件数都是真的，且能打到 /health", async () => {
    const logs: string[] = [];
    const router = await serve({ IDORIS_PORT: "18740", IDORIS_COMPONENTS_DIR: goodFixtures } as NodeJS.ProcessEnv, {
      startRouter,
      log: (m) => logs.push(m),
    });
    try {
      expect(router.port).toBe(18740);
      expect(router.registered.length).toBe(1);
      expect(logs[0]).toContain("18740");
      const res = await fetch("http://127.0.0.1:18740/health");
      expect(res.status).toBe(200);
    } finally {
      await router.close();
    }
  });
});

/** 真实子进程：验证 main() 的退出码与「不是裸 stack trace」这两件事，光测纯函数覆盖不到。 */
describe("idoris-router serve（真实子进程）", () => {
  it("IDORIS_PORT=abc → 启动失败、退出码非 0、错误信息说人话", () => {
    const res = spawnSync(process.execPath, ["--experimental-transform-types", "--import", registerPath, cliPath, "serve"], {
      env: { ...process.env, IDORIS_PORT: "abc", IDORIS_COMPONENTS_DIR: goodFixtures },
      encoding: "utf8",
      timeout: 30_000,
    });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain("IDORIS_PORT");
    expect(res.stderr).not.toContain(" at "); // 不是甩给用户的裸 stack trace
  });

  it("负对照：不带 serve 子命令 → 打印用法、非 0 退出（不是默默什么都不做）", () => {
    const res = spawnSync(process.execPath, ["--experimental-transform-types", "--import", registerPath, cliPath], {
      encoding: "utf8",
      timeout: 30_000,
    });
    expect(res.status).not.toBe(0);
    expect(res.stderr).toContain("用法");
  });
});
