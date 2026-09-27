import { describe, it } from "vitest";

/**
 * R0 阅读接口规范（docs/interfaces/iDoris-Agent24-边界与接口规范.md）与
 * packages/router 实际代码时发现的、规范与 TS 参考实现之间的落差。
 *
 * 按任务要求：发现冲突只标 it.todo 并写清楚冲突点，不改 TS 实现——
 * 这些不是"待写的测试"，是"待和规范/实现对齐的已知落差"的备忘录。
 */
describe("已知的规范 vs TS 现状落差（it.todo，不改实现）", () => {
  it.todo(
    "§3.11 统一错误体要求 {error:{type,rule_id,reason_code,evidence,remediation}}；" +
      "TS 现状绝大多数错误体只有 {error:{type,message?}}——PR #46 给 subscription_relay_failed " +
      "加上了 reason_code（唯一的例外），但 rule_id/evidence/remediation 全线都还没有，" +
      "internal_error/invalid_body 等新错误 type 也一样只有 {type,message?}。",
  );

  it.todo(
    "§3.11 错误 type 枚举列了 local_only_unavailable/budget_exceeded/tenant_missing/policy_violation/" +
      "unsupported_field/context_too_long/upstream_*/oom；TS 实际还会返回不在这个枚举里的 type，例如 " +
      "policy_unconfigured(503，现状经生产 CLI 已不可达，但库层 startRouter() 分支还在)、" +
      "invalid_privacy(400)、invalid_header(400)、invalid_json(400)、invalid_body(400，PR #46 新增)、" +
      "internal_error(500，PR #46 新增)、no_candidate(503)、capabilities_unavailable(503)、" +
      "client_closed(499)、subscription_relay_failed(502)、subscription_source_not_loopback(403)。" +
      "需要规范补齐枚举，或者 TS 把这些 type 改名对齐规范。",
  );

  it.todo(
    "§3.2 要求非 loopback 请求必须带 `Authorization: Bearer idk_<key>`；" +
      "packages/router/src/server.ts 目前完全没有任何鉴权校验，任何请求（含伪造成非 loopback 来源的）都会被放行。" +
      "因为 BIND_HOST 硬编码 127.0.0.1，短期内没有真实的非 loopback 攻击面，但虚拟 key 层尚未实现是事实，" +
      "不应该被本套件的『全绿』掩盖。",
  );

  it.todo(
    "规范设想 iDoris 要能对慢/挂起的上游做 fail-closed；" +
      "packages/router/src/proxy.ts 的 ChatProxy 对上游请求没有设置任何超时，只有『客户端主动断开连接』" +
      "才会 abort 转发给上游的那个请求——如果上游一直不回应而客户端也不断开，iDoris 会一直挂着等，" +
      "不会主动熔断。conformance/tests/upstream-behavior.test.ts 的『慢响应』用例只验证了" +
      "『没有被提前误杀』，没有验证『上游长期挂起时最终会不会超时』，因为现状就是不会。",
  );

  it.todo(
    "§3.1/§3.4/§3.5/§3.8/§3.9 列出的 POST /v1/messages、/v1/embeddings、/v1/rerank、/v1/systemone、" +
      "/v1/inspect、/v1/feedback、/v1/trajectories、GET /admin/api/v1/* 均未实现（分别排到 M4/M5/M6/M8）；" +
      "packages/router/src/server.ts 目前只有 /health、/v1/models、/capabilities、/v1/chat/completions 四个端点。",
  );
});
