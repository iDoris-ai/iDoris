// T1.3.1/T1.3.4 smoke：起 Router（loopback），
// 1) /v1/models 非空；2) 非流式 chat 正常；3) 流式首 token 到达；4) 客户端断开 → 上游收到 abort。
import { createServer } from "node:http";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { startRouter } from "../packages/router/dist/index.js";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

let upstreamAborted = false;
const upstream = createServer((req, res) => {
  let body = "";
  req.on("data", (c) => (body += c));
  req.on("end", () => {
    let parsed = {};
    try { parsed = JSON.parse(body || "{}"); } catch { parsed = {}; }
    if (parsed.stream === true) {
      res.writeHead(200, { "content-type": "text/event-stream" });
      res.write('data: {"choices":[{"delta":{"content":"hello"}}]}\n\n');
      req.on("close", () => { upstreamAborted = true; });
      setTimeout(() => { try { res.end(); } catch { /* already closed */ } }, 600);
    } else {
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ choices: [{ message: { content: "hello" } }] }));
    }
  });
});
await new Promise((r) => upstream.listen(0, "127.0.0.1", r));
const upstreamUrl = "http://127.0.0.1:" + upstream.address().port;

const dir = mkdtempSync(join(tmpdir(), "idoris-smoke-"));
writeFileSync(join(dir, "upstream.yaml"), [
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
  'version_pin: "fake@1"',
  "privacy_class: local_only",
  "allowed_egress: [loopback]",
  "fallback_policy: fail_closed",
  "fail_closed: true",
  "load_policy: { mode: resident, keepalive: { pinned: true }, admission: coexist }",
  ""
].join("\n"));

let failures = 0;
const check = (name, cond) => { if (!cond) { failures += 1; console.error("smoke FAILED: " + name); } };

const router = await startRouter({ componentsDir: dir, routingPolicyPath: join(root, "config", "routing-policy.yaml"), port: 0 });
try {
  const list = await (await fetch("http://127.0.0.1:" + router.port + "/v1/models")).json();
  check("/v1/models non-empty", Array.isArray(list.data) && list.data.length > 0);

  const chat = await fetch("http://127.0.0.1:" + router.port + "/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ model: "m", messages: [{ role: "user", content: "hi" }] }),
  });
  const chatBody = await chat.json();
  check("non-stream chat returns content", chatBody.choices?.[0]?.message?.content === "hello");

  const controller = new AbortController();
  const streamRes = await fetch("http://127.0.0.1:" + router.port + "/v1/chat/completions", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ model: "m", messages: [{ role: "user", content: "hi" }], stream: true }),
    signal: controller.signal,
  });
  const reader = streamRes.body.getReader();
  const first = await reader.read();
  const firstText = first.value ? Buffer.from(first.value).toString("utf8") : "";
  check("stream first token arrives", firstText.includes("hello"));
  controller.abort();
  await new Promise((r) => setTimeout(r, 200));
  check("upstream received abort after client disconnect", upstreamAborted === true);
} finally {
  await router.close();
  upstream.close();
  if (failures === 0) console.log("smoke OK: models + non-stream + stream(first token) + abort propagated");
  else process.exitCode = 1;
}
