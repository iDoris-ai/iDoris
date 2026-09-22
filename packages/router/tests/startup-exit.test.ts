import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const testsDir = dirname(fileURLToPath(import.meta.url));
const helpers = join(testsDir, "helpers");
const registerPath = join(helpers, "ts-register.mjs");
const childPath = join(helpers, "startup-child.mjs");
const subDir = join(testsDir, "fixtures", "subscription");

function runStartup(env: NodeJS.ProcessEnv) {
  return spawnSync(process.execPath, ["--experimental-transform-types", "--import", registerPath, childPath], {
    env: { ...process.env, ...env, IDORIS_TEST_COMPONENTS_DIR: subDir },
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
