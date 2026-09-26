// T2.5.1 smoke:external —— 有 OPENAI_API_KEY 时调通一次；无 key 打印 SKIPPED 而非失败。
import { OpenAiCompatBackend } from "../packages/adapters/dist/openai-compat/openai-compat-backend.js";

const key = process.env.OPENAI_API_KEY;
if (!key) {
  console.log("SKIPPED: OPENAI_API_KEY not set (external slot smoke skipped, not failed)");
  process.exit(0);
}
const base = process.env.OPENAI_BASE_URL ?? "https://api.openai.com";
const model = process.env.OPENAI_MODEL ?? "gpt-4o-mini";
const backend = new OpenAiCompatBackend({ baseUrl: base, apiKey: key });
try {
  const res = await backend.chat({ model, messages: [{ role: "user", content: "reply with exactly: IDORIS_EXTERNAL_OK" }] });
  if (typeof res.content !== "string" || res.content.length === 0) {
    console.error("smoke:external FAILED: empty reply");
    process.exit(1);
  }
  console.log("smoke:external OK: provider=" + base + " model=" + res.model + " reply=" + res.content.slice(0, 60));
} catch (err) {
  console.error("smoke:external FAILED: " + String(err));
  process.exit(1);
}
