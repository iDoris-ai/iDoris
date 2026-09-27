import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

/**
 * 读取本包 package.json 的 version 字段，供 `/health` 的服务身份字段使用。
 *
 * 用 fs 读取而不是 `import ... from "../package.json"`：两者在 src/（tsc 输入）
 * 与 dist/（编译产物）里到 package.json 的相对路径深度相同，运行期定位一致，
 * 且不需要处理 ESM JSON import attributes 在不同 Node/TS 组合下的兼容性。
 */
export function readRouterVersion(): string {
  try {
    const pkgPath = join(dirname(fileURLToPath(import.meta.url)), "..", "package.json");
    const pkg = JSON.parse(readFileSync(pkgPath, "utf8")) as { version?: unknown };
    return typeof pkg.version === "string" ? pkg.version : "unknown";
  } catch {
    // L3：/health 是运行期身份探针，读版本号失败（打包异常、权限问题……）不该让它整个挂掉。
    return "unknown";
  }
}
