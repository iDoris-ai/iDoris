// T2.3.2 smoke:agent24 —— 验证 iDoris 这一侧的接入契约：
// 1) /v1/models 可列；2) local_only 调用落到 loopback provider。
// Agent24 是独立仓库；本脚本只验证 iDoris 侧，Agent24 侧步骤见 docs/agent/handoff-agent24.md。
import { createServer } from "node:http";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { startRouter } from "../packages/router/dist/index.js";

const NL = String.fromCharCode(10);
const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const upstream = createServer((req, res) => {
  req.on("end", () => {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ choices: [{ message: { content: "hello" } }] }));
  });
  req.resume();
});
await new Promise((r) => upstream.listen(0, "127.0.0.1", r));
const upstreamUrl = "http://127.0.0.1:" + upstream.address().port;
const dir = mkdtempSync(join(tmpdir(), "idoris-agent24-"));
writeFileSync(join(dir, "local.yaml"), [
  "provider:",
  "  id: mock",
  "  family: local",
  "  tier: local",
  "  capabilities: [chat]",
  "  privacy_class: local_only",
  "  cost: { input_per_m: 0, output_per_m: 0 }",
  "  locality: loopback",
  "form: http_service",
  "endpoint: " + JSON.stringify(upstreamUrl),
  "version_pin: \"fake@1\"",
  "privacy_class: local_only",
  "allowed_egress: [loopback]",
  "fallback_policy: fail_closed",
  "fail_closed: true",
  "load_policy: { mode: resident, keepalive: { pinned: true }, admission: coexist }",
  ""
].join(NL));
let failures = 0;
const check = (name, cond) => { if (!cond) { failures += 1; console.error("smoke:agent24 FAILED: " + name); } };
const router = await startRouter({ componentsDir: dir, routingPolicyPath: join(root, "config", "routing-policy.yaml"), port: 0 });
try {
  const list = await (await fetch("http://127.0.0.1:" + router.port + "/v1/models")).json();
  check("iDoris exposes a provider list", Array.isArray(list.data) && list.data.length > 0);
  const local = await fetch("http://127.0.0.1:" + router.port + "/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json", "x-idoris-privacy": "local_only" },
    body: JSON.stringify({ model: "m", messages: [{ role: "user", content: "hi" }] }),
  });
  const localBody = await local.json();
  check("local_only call lands on a local model", local.status === 200 && localBody.choices?.[0]?.message?.content === "hello");
} finally {
  await router.close();
  upstream.close();
}
if (failures === 0) console.log("smoke:agent24 OK: models + local_only landed locally (iDoris side of R1/R3 verified)");
else process.exitCode = 1;
