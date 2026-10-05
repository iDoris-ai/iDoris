import { spawn } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { makeSubscriptionComponentsDir } from "./fixtures.js";

export const SUBSCRIPTION_SANDBOX_PROFILE = "idoris-subscription-no-tools-readonly-v1";

export interface SubscriptionFixture {
  componentsDir: string;
  binDir: string;
  markerPath: string;
  env: NodeJS.ProcessEnv;
  pathPrepend: string[];
  readMarker(): string;
  cleanup(): void;
}

function fakeCliScript(): string {
  return `#!/bin/sh
set -eu
marker="\${SUBSCRIPTION_FIXTURE_MARKER:?missing SUBSCRIPTION_FIXTURE_MARKER}"
payload="$(cat)"
pgid="$(ps -o pgid= -p $$ | tr -d ' ')"
printf 'spawn:%s:%s\\n' "$$" "$pgid" >> "$marker"
output_file=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "-o" ]; then output_file="$arg"; break; fi
  prev="$arg"
done
if [ -n "$output_file" ]; then
  printf 'fixture:%s' "$payload" > "$output_file"
else
  printf 'fixture:%s' "$payload"
fi
`;
}

export function makeSubscriptionFixture(cli: "claude" | "codex" = "claude"): SubscriptionFixture {
  const root = mkdtempSync(join(tmpdir(), "idoris-conformance-subcli-"));
  const binDir = join(root, "bin");
  const markerPath = join(root, "spawn-marker.log");
  const componentsDir = makeSubscriptionComponentsDir();
  mkdirSync(binDir);
  for (const name of ["claude", "codex"] as const) {
    const path = join(binDir, name);
    writeFileSync(path, fakeCliScript(), "utf8");
    chmodSync(path, 0o755);
  }
  return {
    componentsDir,
    binDir,
    markerPath,
    pathPrepend: [binDir],
    env: {
      IDORIS_DEPLOY_MODE: "personal",
      IDORIS_ENABLE_SUBSCRIPTION: "1",
      IDORIS_SUBSCRIPTION_SANDBOX: SUBSCRIPTION_SANDBOX_PROFILE,
      IDORIS_SUBSCRIPTION_CLI: cli,
      SUBSCRIPTION_FIXTURE_MARKER: markerPath,
    },
    readMarker: () => (existsSync(markerPath) ? readFileSync(markerPath, "utf8") : ""),
    cleanup: () => {
      rmSync(root, { recursive: true, force: true });
      rmSync(componentsDir, { recursive: true, force: true });
    },
  };
}

export async function probeSubscriptionFixture(
  fixture: SubscriptionFixture,
  cli: "claude" | "codex",
): Promise<string> {
  const program = join(fixture.binDir, cli);
  return await new Promise<string>((resolve, reject) => {
    const child = spawn(program, [], {
      env: { ...process.env, ...fixture.env },
      stdio: ["pipe", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk: Buffer) => { stdout += chunk.toString("utf8"); });
    child.stderr.on("data", (chunk: Buffer) => { stderr += chunk.toString("utf8"); });
    child.on("error", reject);
    child.on("exit", (code) => {
      if (code === 0) resolve(stdout);
      else reject(new Error("fake subscription CLI exited " + String(code) + ": " + stderr));
    });
    child.stdin.end("probe");
  });
}
