import { readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { localComponent, makeComponentsDir } from "../src/fixtures.js";
import { ConformanceStartupError, routingPolicyFixturePath, spawnConformanceServer } from "../src/harness.js";

const dirs: string[] = [];
afterEach(() => { for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true }); });

for (const field of ["extensions", "providerExtensions"] as const) {
  describe(field + " 降级声明", () => {
    const cases: { name: string; extensions?: Record<string, unknown>; accepted: boolean }[] = [
      { name: "省略", accepted: true },
      { name: "空对象", extensions: {}, accepted: true },
      { name: "仅声明", extensions: { _degradation: "fallback" }, accepted: true },
      { name: "特性带声明", extensions: { native: true, _degradation: " 降级为普通请求 " }, accepted: true },
      { name: "NEL 不属于 JS trim 空白", extensions: { native: true, _degradation: "\u0085" }, accepted: true },
      { name: "缺声明", extensions: { native: true }, accepted: false },
      { name: "空声明", extensions: { native: true, _degradation: "" }, accepted: false },
      { name: "纯空白声明", extensions: { native: true, _degradation: " \t\r\n\u00a0\u3000" }, accepted: false },
      { name: "BOM 声明", extensions: { native: true, _degradation: "\ufeff" }, accepted: false },
      ...[null, false, 7, [], {}].map((value) => ({
        name: "非字符串 " + JSON.stringify(value), extensions: { native: true, _degradation: value }, accepted: false,
      })),
    ];
    it.each(cases)("$name", async ({ extensions, accepted }) => {
      // 另一层始终合法，不能用卡级声明替 provider 级声明（或反过来）。
      const card = localComponent("http://127.0.0.1:12345", {
        extensions: { native: true, _degradation: "fallback" },
        providerExtensions: { native: true, _degradation: "fallback" },
        [field]: extensions,
      });
      const componentsDir = makeComponentsDir([card]);
      dirs.push(componentsDir);
      // YAML 1.1 会规范化裸 NEL；使用转义使两个解析器得到相同的声明字符串。
      const path = join(componentsDir, readdirSync(componentsDir)[0] as string);
      writeFileSync(path, readFileSync(path, "utf8").replaceAll("\u0085", "\\u0085"), "utf8");
      const start = Date.now();
      const result = await spawnConformanceServer({
        componentsDir, routingPolicyPath: routingPolicyFixturePath, healthTimeoutMs: 10_000,
      }).then(async (server) => {
        try {
          const health: unknown = await (await fetch(server.baseUrl + "/health")).json();
          expect(health).toMatchObject({ components: 1 });
          return undefined;
        } finally {
          await server.stop();
        }
      }, (err: unknown) => err);
      if (accepted) {
        expect(result).toBeUndefined();
      } else {
        expect(result).toBeInstanceOf(ConformanceStartupError);
        expect(result).toMatchObject({ kind: "exited_early" });
        expect((result as ConformanceStartupError).exitCode).not.toBeNull();
        expect((result as ConformanceStartupError).exitCode).not.toBe(0);
        expect(Date.now() - start).toBeLessThan(2_000);
      }
    });
  });
}
