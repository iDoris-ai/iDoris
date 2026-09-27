import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import { startRouter, type Router } from "../src/server.js";

const testsDir = dirname(fileURLToPath(import.meta.url));
const fixtures = join(testsDir, "fixtures", "good");
const routerPkg = JSON.parse(readFileSync(join(testsDir, "..", "package.json"), "utf8")) as { version: string };

// M3（PR #46 复审）：故意写死期望值、不 import CONTRACT_VERSION 常量本身——
// 断言如果引用的是同一个常量，契约版本改错了（比如漏升级、多打个字符）测试
// 依然会自己跟自己保持"一致"而全绿，量纲对不上"契约版本是否等于规范文档
// 标注的那个值"这件事。规范当前定的版本见
// docs/interfaces/iDoris-Agent24-边界与接口规范.md（v1.0.1）。
const EXPECTED_CONTRACT_VERSION = "1.0.1";

let running: Router | undefined;

afterEach(async () => {
  if (running) {
    await running.close();
    running = undefined;
  }
});

const url = (r: Router, path: string): string => "http://127.0.0.1:" + r.port + path;

interface HealthBody {
  status: string;
  service: string;
  version: string;
  contract_version: string;
  instance_id: string;
  components: number;
}

/**
 * T4.1：`GET /health` 回报服务身份（接口规范 §3.1/§3.12），
 * Agent24 启动时靠这几个字段做兼容性校验，字段名/取值都不能只是「看起来像」。
 */
describe("GET /health 服务身份", () => {
  it("字段齐全：service/version/contract_version/instance_id/components", async () => {
    running = await startRouter({ componentsDir: fixtures });
    const body = (await (await fetch(url(running, "/health"))).json()) as HealthBody;
    expect(body.status).toBe("ok");
    expect(body.service).toBe("idoris");
    // version 取的是 @idoris/router 包 package.json 里的真实值，不是写死的字符串。
    expect(body.version).toBe(routerPkg.version);
    expect(body.contract_version).toBe(EXPECTED_CONTRACT_VERSION);
    expect(typeof body.instance_id).toBe("string");
    expect(body.instance_id.length).toBeGreaterThan(0);
    expect(body.components).toBe(1); // fixtures/good 下只有一张组件卡
  });

  it("instance_id 在同一个 Router 实例的两次请求间保持不变", async () => {
    running = await startRouter({ componentsDir: fixtures });
    const first = (await (await fetch(url(running, "/health"))).json()) as HealthBody;
    const second = (await (await fetch(url(running, "/health"))).json()) as HealthBody;
    expect(second.instance_id).toBe(first.instance_id);
  });

  it("负对照：两个不同的 Router 实例各自持有不同的 instance_id（不是写死的常量）", async () => {
    const a = await startRouter({ componentsDir: fixtures });
    const b = await startRouter({ componentsDir: fixtures });
    try {
      const bodyA = (await (await fetch(url(a, "/health"))).json()) as HealthBody;
      const bodyB = (await (await fetch(url(b, "/health"))).json()) as HealthBody;
      expect(bodyA.instance_id).not.toBe(bodyB.instance_id);
    } finally {
      await a.close();
      await b.close();
    }
  });
});
