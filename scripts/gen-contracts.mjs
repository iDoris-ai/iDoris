// 契约生成：JSON Schema（真源）→ zod（TS 侧产物）。
// 用法：node scripts/gen-contracts.mjs [outDir]   （默认 packages/contracts/src/generated）
import { mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { jsonSchemaToZod } from "json-schema-to-zod";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const schemaDir = join(root, "packages/contracts/schema");

const loadSchema = (file) => JSON.parse(readFileSync(join(schemaDir, file), "utf8"));

/** 把同目录内的 `$ref` 就地内联，让每个生成产物自包含（json-schema-to-zod 不解析外部 ref）。 */
function inlineRefs(node, seen = new Set()) {
  if (Array.isArray(node)) return node.map((n) => inlineRefs(n, seen));
  if (node && typeof node === "object") {
    if (typeof node.$ref === "string") {
      const target = node.$ref;
      if (!target.endsWith(".schema.json")) throw new Error(`unsupported $ref: ${target}`);
      if (seen.has(target)) throw new Error(`cyclic $ref: ${target}`);
      const sub = loadSchema(target);
      const { $schema: _s, $id: _i, title: _t, ...rest } = sub;
      return inlineRefs(rest, new Set([...seen, target]));
    }
    const out = {};
    for (const [k, v] of Object.entries(node)) out[k] = inlineRefs(v, seen);
    return out;
  }
  return node;
}

export function generateInto(outDir) {
  mkdirSync(outDir, { recursive: true });
  for (const file of readdirSync(schemaDir).filter((f) => f.endsWith(".schema.json")).sort()) {
    const raw = loadSchema(file);
    const name = raw.title.charAt(0).toLowerCase() + raw.title.slice(1) + "Schema";
    const code = jsonSchemaToZod(inlineRefs(raw), { module: "esm", name });
    writeFileSync(
      join(outDir, file.replace(/\.schema\.json$/, ".ts")),
      `// GENERATED FROM packages/contracts/schema/${file} — do not edit by hand.\n${code}\n`,
    );
  }
}

if (import.meta.url === `file://${process.argv[1]}` || process.argv[1]?.endsWith("gen-contracts.mjs")) {
  const outDir = process.argv[2] ?? join(root, "packages/contracts/src/generated");
  generateInto(outDir);
  console.log("generated into", outDir);
}
