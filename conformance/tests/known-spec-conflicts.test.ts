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
    "【真实 bug，不只是规范措辞差异】§3.11 说『取消：客户端断开连接即向上游传播取消（已实现）』，" +
      "但 packages/router/src/server.ts:330 的 `req.on(\"close\", () => controller.abort())` " +
      "监听的是**请求对象**（客户端→路由器方向）的 close，不是响应对象/socket 的 close。" +
      "请求体在走到这行之前已经被 `readBody(req)` 完整读完，Node 的 IncomingMessage 读完之后会" +
      "自己很快再触发一次 'close'，跟客户端有没有真的断开连接毫无关系；等这行代码执行、挂上监听器时，" +
      "那次『自然 close』通常已经发生过了（实测 `req.destroyed`/`req.complete` 在挂监听器之前就已经是 " +
      "true/true）。净效果：真实客户端断开时 `controller.abort()` 基本不会被调用，取消不会传播到上游。" +
      "已用变异测试验证（细节见 PR 描述，两次改动都已还原、未进最终 diff）：" +
      "(1) 把这行临时改成 `res.on(\"close\", () => { if (!res.writableEnded) controller.abort(); })` " +
      "→ conformance/tests/upstream-behavior.test.ts 的取消传播用例约 300ms 内变绿；" +
      "(2) 在这个修好的基础上再临时去掉 proxy.ts 里 `init.signal = opts.signal` 那一行 → 又变红，" +
      "证明这两处都是必要环节。建议修法：req.on(\"close\") 改成 res.on(\"close\")，" +
      "且要判断 !res.writableEnded（避免把正常收尾误判成取消）。",
  );

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
    "规范设想 iDoris 要能对慢/挂起的上游做 fail-closed；packages/router/src/proxy.ts 的 ChatProxy " +
      "对上游请求没有设置任何超时，**理论上**只在『客户端主动断开连接』时才会 abort 转发给上游的那个" +
      "请求——但见上一条：这条『客户端断开 → abort 上游』的链路本身现在也是坏的（server.ts 监听错了" +
      "对象），所以实际情况比『只在客户端断开时兜底』更差：上游一直不回应时，不管客户端断不断开，" +
      "iDoris 都会一直挂着等，不会主动熔断。conformance/tests/upstream-behavior.test.ts 的『慢响应』" +
      "用例只验证了『没有被提前误杀』，没有验证『上游长期挂起时最终会不会超时』，因为现状就是不会。",
  );

  it.todo(
    "§3.1/§3.4/§3.5/§3.8/§3.9 列出的 POST /v1/messages、/v1/embeddings、/v1/rerank、/v1/systemone、" +
      "/v1/inspect、/v1/feedback、/v1/trajectories、GET /admin/api/v1/* 均未实现（分别排到 M4/M5/M6/M8）；" +
      "packages/router/src/server.ts 目前只有 /health、/v1/models、/capabilities、/v1/chat/completions 四个端点。",
  );
});
