import { describe, expect, it } from "vitest";
import { CATALOG_ROLES, ROLES, isRole, roleSchema } from "../src/role.js";

describe("Role（T4.2 角色枚举统一）", () => {
  it("接受规范枚举内的角色", () => {
    for (const role of ["fast", "daily", "deep", "vision", "embed", "rerank", "decide", "auto"]) {
      expect(roleSchema.safeParse(role).success).toBe(true);
    }
  });

  it("拒绝旧角色名 core / temp", () => {
    expect(roleSchema.safeParse("core").success).toBe(false);
    expect(roleSchema.safeParse("temp").success).toBe(false);
  });

  it("拒绝未知角色", () => {
    expect(roleSchema.safeParse("nope").success).toBe(false);
  });

  it("ROLES 与 isRole 一致", () => {
    expect(ROLES).toEqual(["fast", "daily", "deep", "vision", "embed", "rerank", "decide", "auto"]);
    for (const role of ROLES) expect(isRole(role)).toBe(true);
    expect(isRole("core")).toBe(false);
  });

  it("CATALOG_ROLES 排除 auto（auto 是路由时机的选择，不是模型的静态属性）", () => {
    expect(CATALOG_ROLES).not.toContain("auto");
    expect(CATALOG_ROLES).toEqual(["fast", "daily", "deep", "vision", "embed", "rerank", "decide"]);
  });
});
