// 手写薄封装：把 JSON Schema 生成的 zod 再导出，并派生 TS 类型 + 常量数组。
// 真源：packages/contracts/schema/role.schema.json；产物：src/generated/role.ts。
//
// T4.2：角色枚举统一（docs/interfaces/iDoris-Agent24-边界与接口规范.md §3.3、§3.12）。
// 上游（Agent24）约定用 `model=idoris/<role>` 调用；具体模型名只作为信息返回，不是稳定契约。
import type { z } from "zod";
import { roleSchema } from "./generated/role.js";

export { roleSchema };
export type Role = z.infer<typeof roleSchema>;

/** 全部角色枚举值，按规范顺序（供遍历 / 校验使用）。 */
export const ROLES: readonly Role[] = roleSchema.options;

/**
 * catalog 目录条目可声明的角色：排除 `auto`——`auto` 是「交给 iDoris 选」，
 * 不是某个具体模型的静态属性，任何 catalog 条目都不应该声明它。
 */
export const CATALOG_ROLES: readonly Exclude<Role, "auto">[] = ROLES.filter(
  (r): r is Exclude<Role, "auto"> => r !== "auto",
);

export function isRole(value: string): value is Role {
  return (ROLES as readonly string[]).includes(value);
}
