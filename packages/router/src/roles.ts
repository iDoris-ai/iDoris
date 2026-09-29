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
import { isRole, ROLES, type CatalogRole, type Role } from "@idoris/contracts";
import { isEligibleForRole, type Catalog } from "@idoris/recommender";

/** `idoris/` 前缀：先 trim 整个输入，前缀本身大小写不敏感；命中前缀后角色部分精确匹配。 */
const IDORIS_MODEL_PREFIX_RE = /^idoris\//i;

export type RoleParseErrorCode = "unknown_role";

/**
 * 角色解析失败（预期错误，将来映射为 400）。
 * `reason_code` 与 docs/interfaces/iDoris-Agent24-边界与接口规范.md §3.11 的错误体命名对齐；
 * `code` 是本仓库既有的错误类惯例（参见 `ProfileError`/`EgressGuardError`），两者取值相同。
 */
export class RoleParseError extends Error {
  readonly code: RoleParseErrorCode;
  readonly reason_code: RoleParseErrorCode;

  constructor(code: RoleParseErrorCode, message: string) {
    super(message);
    this.name = "RoleParseError";
    this.code = code;
    this.reason_code = code;
  }
}

/**
 * 解析 `model` 字段里的 `idoris/<role>` 前缀。
 * - `model` 不是字符串 → 抛 {@link RoleParseError}（调用方传参类型错误，不是「按原模型名处理」）。
 * - 前缀匹配前先 `trim()`；前缀 `idoris/` 本身大小写不敏感（`IDORIS/`、`Idoris/` 都算命中）。
 * - 非 `idoris/` 前缀 → 返回 `null`（调用方应按原模型名处理，不是角色请求）。
 * - 命中前缀后，角色部分**精确匹配**（大小写敏感、不允许多余路径段）；
 *   不匹配（含空角色 `idoris/`、多段 `idoris/fast/x`、未知角色）→ 抛 {@link RoleParseError}
 *   （`code`/`reason_code`: `"unknown_role"`）。
 */
export function parseModelRole(model: unknown): Role | null {
  if (typeof model !== "string") {
    throw new RoleParseError("unknown_role", `model 必须是字符串，收到 ${typeof model}`);
  }
  const trimmed = model.trim();
  const prefixMatch = IDORIS_MODEL_PREFIX_RE.exec(trimmed);
  if (prefixMatch === null) return null;
  const rolePart = trimmed.slice(prefixMatch[0].length);
  if (!isRole(rolePart)) {
    throw new RoleParseError(
      "unknown_role",
      `未知角色 "${rolePart}"（model="${model}"）；合法角色为 ${ROLES.join("|")}`,
    );
  }
  return rolePart;
}

/**
 * 按角色从 catalog 里找可用的模型 id（按 catalog 声明顺序）。
 *
 * 签名收窄为 {@link CatalogRole}（不含 `auto`）：`auto` 是「交给 iDoris 选」的路由时机决策，
 * 不是某个模型的静态属性，也没有对应的候选列表——调用方必须先处理掉 `auto`
 * （例如落到 recommender 的常驻推荐，或另一条 auto 专属的选择逻辑），不能传给本函数。
 *
 * 筛选逻辑与 `@idoris/recommender` 的 `isEligibleForRole` **共用同一个函数**
 * （见 recommend.ts 里「常驻：daily」那段的调用），避免两处各写一套 experiment/min_ram_gb
 * 判断、悄悄漂移出两套口径。
 *
 * @param installedModelIds 缺省时不做「已安装」过滤，只按角色声明返回全部候选。
 * @param minRamGb 缺省时不做硬件门槛过滤；传入时排除 `min_ram_gb` 超出这个值的条目。
 */
export function resolveRoleToCandidates(
  role: CatalogRole,
  catalog: Catalog,
  installedModelIds?: readonly string[],
  minRamGb?: number,
): string[] {
  const installed = installedModelIds === undefined ? undefined : new Set(installedModelIds);
  const candidates: string[] = [];
  for (const model of catalog.catalog) {
    if (!isEligibleForRole(model, role, minRamGb)) continue;
    if (installed !== undefined && !installed.has(model.id)) continue;
    candidates.push(model.id);
  }
  return candidates;
}
