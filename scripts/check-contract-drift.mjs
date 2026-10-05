// 零漂移门：重新生成到临时目录，与已提交的 src/generated 逐字节比对。
// 用法：node scripts/check-contract-drift.mjs
import { mkdtempSync, readdirSync, readFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { generateInto } from "./gen-contracts.mjs";
import {
  assertContractVersionsEqual,
  parseConformanceContractVersion,
  parseRustContractVersion,
  parseTsContractVersion,
} from "./contract-version.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const committedDir = join(root, "packages/contracts/src/generated");
const tmp = mkdtempSync(join(tmpdir(), "idoris-contracts-"));

const tsVersion = parseTsContractVersion(
  readFileSync(join(root, "packages/contracts/src/version.ts"), "utf8"),
);
const rustVersion = parseRustContractVersion(
  readFileSync(join(root, "crates/idoris-contracts/src/version.rs"), "utf8"),
);
const conformanceVersion = parseConformanceContractVersion(
  readFileSync(join(root, "conformance/tests/response-headers.test.ts"), "utf8"),
);
assertContractVersionsEqual({ ts: tsVersion, rust: rustVersion, conformance: conformanceVersion });

try {
  generateInto(tmp);
  let drift = false;
  const isSource = (f) => f.endsWith(".ts") && !f.endsWith(".d.ts") && !f.endsWith(".d.ts.map");
  const committed = readdirSync(committedDir).filter(isSource);
  for (const f of committed) {
    const a = readFileSync(join(committedDir, f), "utf8");
    const b = existsSync(join(tmp, f)) ? readFileSync(join(tmp, f), "utf8") : "";
    if (a !== b) {
      console.error("DRIFT: " + f);
      drift = true;
    }
  }
  for (const f of readdirSync(tmp)) {
    if (isSource(f) && !committed.includes(f)) {
      console.error("MISSING committed generated file: " + f);
      drift = true;
    }
  }
  if (drift) {
    console.error("契约漂移：请运行 pnpm gen:contracts 并提交产物。");
    process.exit(1);
  }
  console.log(`no contract drift (wire version ${tsVersion})`);
} finally {
  rmSync(tmp, { recursive: true, force: true });
}
