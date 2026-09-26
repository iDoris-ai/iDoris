/**
 * @idoris/contracts — 契约的 TS 类型 + zod schema。
 *
 * 真源：packages/contracts/schema/*.schema.json（JSON Schema，D-3）；
 * 产物：packages/contracts/src/generated/*.ts（由 scripts/gen-contracts.mjs 生成）。
 * 契约来源：docs/06 §10 + docs/agent/spec.md 数据模型章节。
 */
export * from "./provider.js";
export * from "./load-policy.js";
export * from "./component-card.js";
export * from "./routing-policy.js";
export * from "./task-profile.js";
export * from "./validate.js";
export * from "./tenant.js";
export * from "./adapter-manifest.js";
export * from "./adapter-gate.js";
