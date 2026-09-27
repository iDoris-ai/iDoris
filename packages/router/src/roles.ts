/**
 * T4.2 — 角色解析器（docs/interfaces/iDoris-Agent24-边界与接口规范.md §3.3、§3.12）。
 *
 * 上游（Agent24）约定用 `model=idoris/<role>` 调用，角色即模型名是稳定契约：
 * `idoris/fast|daily|deep|vision|embed|rerank|decide`，加上 `idoris/auto`（交给 iDoris 选）。
 * 具体模型名只作为信息返回，不是稳定契约。
 *
 * 本文件只是**纯函数**：解析 `model` 字符串、按角色从 catalog 找候选模型 id。
 * 不接入 server.ts / dispatch（接线留到后续任务，避免和并行改 server.ts 的任务冲突）。
 */
import { isRole, type Role } from "@idoris/contracts";
import type { Catalog } from "@idoris/recommender";

const IDORIS_MODEL_PREFIX = "idoris/";

export type RoleParseErrorCode = "unknown_role";

/** 角色解析失败（预期错误，将来映射为 400）。 */
export class RoleParseError extends Error {
  constructor(
    readonly code: RoleParseErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "RoleParseError";
  }
}

/**
 * 解析 `model` 字段里的 `idoris/<role>` 前缀。
 * - 非 `idoris/` 前缀 → 返回 `null`（调用方应按原模型名处理，不是角色请求）。
 * - `idoris/<未知角色>` → 抛 {@link RoleParseError}（`code: "unknown_role"`）。
 * - `idoris/<合法角色>` → 返回该角色。
 */
export function parseModelRole(model: string): Role | null {
  if (!model.startsWith(IDORIS_MODEL_PREFIX)) return null;
  const role = model.slice(IDORIS_MODEL_PREFIX.length);
  if (!isRole(role)) {
    throw new RoleParseError(
      "unknown_role",
      `未知角色 "${role}"（model="${model}"）；合法角色为 fast|daily|deep|vision|embed|rerank|decide|auto`,
    );
  }
  return role;
}

/**
 * 按角色从 catalog 里找可用的模型 id（按 catalog 声明顺序）。
 *
 * - `role === "auto"` 没有对应的 catalog 角色（`auto` = 交给 iDoris 选，不是某个模型的
 *   静态属性），这里返回空数组——"auto" 的候选选择逻辑属于 recommender/dispatch，不在本函数职责内。
 * - `installedModelIds` 缺省时不做「已安装」过滤，只按角色声明返回全部候选。
 */
export function resolveRoleToCandidates(
  role: Role,
  catalog: Catalog,
  installedModelIds?: readonly string[],
): string[] {
  if (role === "auto") return [];
  const installed = installedModelIds === undefined ? undefined : new Set(installedModelIds);
  const candidates: string[] = [];
  for (const model of catalog.catalog) {
    // 省略 roles 字段按 daily 处理（与 recommender 的常驻推荐口径一致）；
    // 显式 roles: [] 的条目（如 load_hint: on_demand 槽）不参与任何角色。
    const roles = model.roles ?? ["daily"];
    if (!roles.includes(role)) continue;
    if (installed !== undefined && !installed.has(model.id)) continue;
    candidates.push(model.id);
  }
  return candidates;
}
