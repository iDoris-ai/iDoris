import { readFileSync, readdirSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { afterEach, expect, it, vi } from "vitest";
import { localComponent, makeComponentsDir, makeRoutingPolicyFile } from "../src/fixtures.js";
import { ConformanceStartupError, spawnConformanceServer } from "../src/harness.js";
import { startFakeUpstream } from "../src/fake-upstream.js";

const dirs: string[] = [];
afterEach(() => {
  vi.unstubAllEnvs();
  for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function cardText(extra: Parameters<typeof localComponent>[1] = {}): string {
  const dir = makeComponentsDir([localComponent("http://127.0.0.1:12345", extra)]);
  dirs.push(dir);
  return readFileSync(join(dir, readdirSync(dir)[0] as string), "utf8");
}
it("未指定 loadPolicy 时保留 resident 默认值，extensions 默认不生成", () => {
  const text = cardText();
  expect(text).toContain("load_policy: { mode: resident, keepalive: { pinned: true }, admission: coexist }");
  expect(text).not.toContain("extensions:");
});

it("loadPolicy=null 真正省略字段，而不是输出 null", () => {
  expect(cardText({ loadPolicy: null })).not.toContain("load_policy:");
});

it("可逐字段定制 load_policy，不覆盖成默认值", () => {
  const loadPolicy = { mode: "on_demand", keepalive: { idle_ttl_s: 7 }, admission: "requires_eviction" };
  expect(cardText({ loadPolicy })).toContain("load_policy: " + JSON.stringify(loadPolicy));
});

it.each([{}, { native: "引号\"、换行\n", _degradation: "fallback" }, { native: true }])(
  "卡及 provider extensions 原样保留，包括空对象和缺降级声明的负例：%j",
  (extensions) => {
    const text = cardText({ extensions, providerExtensions: extensions });
    expect(text).toContain("\n  extensions: " + JSON.stringify(extensions) + "\n");
    expect(text).toContain("\nextensions: " + JSON.stringify(extensions) + "\n");
  },
);

const policy = "routing_policy:\n  version: 1\n  rules: []\n  default: { tiers: [local], fail_closed: true }\n";
function policyFile(content: string): string {
  const path = makeRoutingPolicyFile(content);
  dirs.push(dirname(path));
  return path;
}

it("定制 policy 每次生成独立文件，内容不被默认策略覆盖", () => {
  const first = policyFile(policy);
  const second = policyFile("routing_policy: broken\n");
  expect(first).not.toBe(second);
  expect(readFileSync(first, "utf8")).toBe(policy);
  expect(readFileSync(second, "utf8")).toBe("routing_policy: broken\n");
});

it("cwd 与定制 policy 路径真正传入子进程（由子进程回报）", async () => {
  const path = policyFile(policy);
  vi.stubEnv("IDORIS_CONFORMANCE_ARGV", JSON.stringify([process.execPath, "-e", `
    require('node:http').createServer((req, res) => {
      res.end(JSON.stringify({ cwd: process.cwd(), policy: process.env.IDORIS_ROUTING_POLICY }));
    }).listen(Number(process.env.IDORIS_PORT), '127.0.0.1');
  `]));
  const server = await spawnConformanceServer({ componentsDir: dirname(path), routingPolicyPath: path, cwd: "/" });
  try {
    expect(await (await fetch(server.baseUrl + "/health")).json()).toEqual({ cwd: "/", policy: path });
  } finally {
    await server.stop();
  }
});

it("被测 CLI 接受定制 policy；坏 policy 仍真实非零提前退出", async () => {
  const upstream = await startFakeUpstream();
  const componentsDir = makeComponentsDir([localComponent(upstream.url, {
    extensions: { _degradation: "fallback", native: true },
    providerExtensions: { _degradation: "fallback", native: true },
  })]);
  dirs.push(componentsDir);
  try {
    const server = await spawnConformanceServer({ componentsDir, routingPolicyPath: policyFile(policy) });
    try {
      const health = await (await fetch(server.baseUrl + "/health")).json() as { components: number };
      expect(health.components).toBe(1);
    } finally {
      await server.stop();
    }
    const start = Date.now();
    const failure = await spawnConformanceServer({
      componentsDir, routingPolicyPath: policyFile("routing_policy: broken\n"), healthTimeoutMs: 10_000,
    }).then(async (unexpected) => { await unexpected.stop(); return undefined; }, (err: unknown) => err);
    expect(failure).toBeInstanceOf(ConformanceStartupError);
    expect(failure).toMatchObject({ kind: "exited_early" });
    expect((failure as ConformanceStartupError).exitCode).not.toBeNull();
    expect((failure as ConformanceStartupError).exitCode).not.toBe(0);
    expect(Date.now() - start).toBeLessThan(2_000);
  } finally {
    await upstream.close();
  }
});
