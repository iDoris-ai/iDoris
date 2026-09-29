import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const testsDir = dirname(fileURLToPath(import.meta.url));
const helpers = join(testsDir, "helpers");
const childPath = join(helpers, "startup-child.mjs");
const subDir = join(testsDir, "fixtures", "subscription");

// startup-child.mjs 从 dist/index.js 导入（见该文件注释）：普通编译后的 ESM，
// 不需要任何实验性标志——避免了 --experimental-transform-types 在 Node
// 22/24/26 上行为不一致（Node 26 直接 bad option 崩掉）的问题。
function runStartup(env: NodeJS.ProcessEnv) {
  return spawnSync(process.execPath, [childPath], {
    env: { ...process.env, ...env, IDORIS_TEST_COMPONENTS_DIR: subDir, NODE_NO_WARNINGS: "1" },
    encoding: "utf8",
    timeout: 60_000,
  });
}

describe("router startup exit code (T1.4.2)", () => {
  for (const mode of ["tenant", "community", "city"]) {
    it("exits non-zero for deploy_mode=" + mode, () => {
      const res = runStartup({ IDORIS_DEPLOY_MODE: mode });
      expect(res.status).not.toBe(0);
      expect(res.stderr).toContain("deploy_mode=" + mode);
    });
  }
  it("positive control: personal + disabled starts and exits 0", () => {
    const res = runStartup({ IDORIS_DEPLOY_MODE: "personal", IDORIS_DISABLE_SUBSCRIPTION: "1" });
    expect(res.status).toBe(0);
    expect(res.stdout).toContain("STARTED");
  });
});
