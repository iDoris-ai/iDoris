/**
 * 契约版本号（不是某个 npm 包的 version，是 iDoris 对外 HTTP 契约的版本，
 * 通过 `GET /health` 的 `contract_version` 字段回报给调用方，供 Agent24 等
 * 接入方在启动时做兼容性校验）。
 *
 * 取值依据：iDoris × Agent24《分工边界与接口数据规范》
 * （docs/interfaces/iDoris-Agent24-边界与接口规范.md）标注的版本号，直接沿用。
 * - v1.0（2026-09-27 定稿）：本契约第一次定稿并进入生产实现（M4 接入前置）。
 * - v1.0.1：PR #46 复审后的加法修订（响应头/错误体细节修正，不改变已发布的
 *   字段语义），规范文档同步升到 v1.0.1。
 *
 * 后续演进按同文档 G-5「加法优先、零回归」处理：
 * - 加法新增字段/端点/枚举值 → 升 minor；
 * - 破坏性变更（删字段、改语义、收紧枚举）→ 升 major，并需双方协商同意。
 *
 * **注意**：这是手写常量，不经过 `scripts/gen-contracts.mjs` 生成管线，
 * `pnpm check:contract-drift` 目前管不到它（见 docs/agent/tasks.md FU-21）。
 */
export const CONTRACT_VERSION = "1.0.1";
