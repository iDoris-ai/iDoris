/**
 * 契约版本号（不是某个 npm 包的 version，是 iDoris 对外 HTTP 契约的版本，
 * 通过 `GET /health` 的 `contract_version` 字段回报给调用方，供 Agent24 等
 * 接入方在启动时做兼容性校验）。
 *
 * 取值依据：iDoris × Agent24《分工边界与接口数据规范》v1.0
 * （docs/interfaces/iDoris-Agent24-边界与接口规范.md，2026-09-27 定稿，
 * §0 标注「状态：✅ 定稿 v1.0」）——这是本契约第一次定稿并进入生产实现
 * （M4 接入前置）的版本号，直接沿用该文档标注的版本。
 *
 * 后续演进按同文档 G-5「加法优先、零回归」处理：
 * - 加法新增字段/端点/枚举值 → 升 minor；
 * - 破坏性变更（删字段、改语义、收紧枚举）→ 升 major，并需双方协商同意。
 */
export const CONTRACT_VERSION = "1.0.0";
