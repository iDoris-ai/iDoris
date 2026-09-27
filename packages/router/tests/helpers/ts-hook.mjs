import { existsSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";
import { dirname, join, resolve as resolvePath } from "node:path";

// 让子进程 node 直接跑 TS 源码（--experimental-transform-types）：
// 1) 把 @idoris/* 指向 packages/*/src；2) 把相对 `./x.js` 解析到同目录 `x.ts`。
const here = dirname(fileURLToPath(import.meta.url));
const packagesDir = resolvePath(here, "..", "..", "..");
const ALIASES = {
  "@idoris/contracts": join(packagesDir, "contracts", "src", "index.ts"),
  "@idoris/adapters": join(packagesDir, "adapters", "src", "index.ts"),
  "@idoris/recommender": join(packagesDir, "recommender", "src", "index.ts"),
  "@idoris/tenancy": join(packagesDir, "tenancy", "src", "index.ts"),
};

export async function resolve(specifier, context, nextResolve) {
  const alias = ALIASES[specifier];
  if (alias !== undefined) return { url: pathToFileURL(alias).href, shortCircuit: true };
  if ((specifier.startsWith("./") || specifier.startsWith("../")) && context.parentURL !== undefined) {
    try {
      const abs = fileURLToPath(new URL(specifier, context.parentURL));
      if (abs.endsWith(".js")) {
        const ts = abs.slice(0, -3) + ".ts";
        if (existsSync(ts)) return { url: pathToFileURL(ts).href, shortCircuit: true };
      }
    } catch {
      // 解析失败就交回默认解析器。
    }
  }
  return nextResolve(specifier, context);
}
