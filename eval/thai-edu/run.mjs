#!/usr/bin/env node
/**
 * 泰语 edu 评测运行器。
 *
 *   node eval/thai-edu/run.mjs --dry-run
 *   EVAL_BASE_URL=http://127.0.0.1:8088/v1 EVAL_API_KEY=... EVAL_MODEL=... node eval/thai-edu/run.mjs
 *
 * 密钥只从环境变量读取，不写进结果文件，不打印。
 */

import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

import {
  buildRequestBody,
  formatSummary,
  loadCases,
  parseChatResponse,
  redact,
  resultStamp,
  safeModelSlug,
  scoreCase,
  summarize,
  validateCases,
} from "./lib.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));

export function printHelp() {
  return [
    "用法: node eval/thai-edu/run.mjs [--dry-run] [--cases 目录] [--out 目录]",
    "",
    "环境变量（只有真正发请求时才需要）:",
    "  EVAL_BASE_URL   OpenAI 兼容根路径，含 /v1，例如 http://127.0.0.1:8088/v1",
    "  EVAL_API_KEY    Bearer 令牌。不会写入结果，也不会打印。",
    "  EVAL_MODEL      模型名，用于请求体和结果文件名。",
    "  EVAL_TIMEOUT_MS 单条超时，默认 120000。",
    "",
    "--dry-run  不发请求，只校验 cases/*.jsonl。",
    "退出码: 0 校验通过或已写出结果文件；1 参数、校验或环境错误。",
    "用例没答对不会让进程失败，通过率写在结果 JSON 里。",
  ].join("\n");
}

export async function main(argv, env = process.env, io = console) {
  const { values } = parseArgs({
    args: argv,
    options: {
      "dry-run": { type: "boolean", default: false },
      cases: { type: "string", default: path.join(HERE, "cases") },
      out: { type: "string", default: path.join(HERE, "results") },
      help: { type: "boolean", default: false },
    },
    strict: true,
  });

  if (values.help) {
    io.log(printHelp());
    return { exitCode: 0, report: printHelp() };
  }

  const { files, cases } = await loadCases(values.cases);
  const errors = validateCases(cases);
  if (values["dry-run"] || errors.length > 0) {
    const report = formatDryRun(files, cases, errors);
    io.log(report);
    return { exitCode: errors.length === 0 ? 0 : 1, report };
  }

  const baseUrl = env.EVAL_BASE_URL?.trim();
  const apiKey = env.EVAL_API_KEY;
  const model = env.EVAL_MODEL?.trim();
  if (!baseUrl || !apiKey || !model) {
    const report = "缺少 EVAL_BASE_URL、EVAL_API_KEY 或 EVAL_MODEL。未发请求。";
    io.error(report);
    return { exitCode: 1, report };
  }

  const timeoutMs = Number(env.EVAL_TIMEOUT_MS ?? 120000);
  const secrets = [apiKey];
  try {
    const userinfo = new URL(baseUrl).password;
    if (userinfo) secrets.push(userinfo);
    const username = new URL(baseUrl).username;
    if (username) secrets.push(username);
  } catch {
    // baseUrl 非法时下面的 fetch 会失败，这里不把原始字符串打出来。
  }

  const started = new Date();
  const items = [];
  let consecutiveSetupFailures = 0;
  let aborted = false;

  for (const item of cases) {
    if (aborted) {
      items.push(pendingSkip(item, "skipped_after_setup_failures"));
      continue;
    }
    const startedMs = Date.now();
    try {
      const response = await callChat({ baseUrl, apiKey, model, item, timeoutMs, secrets });
      consecutiveSetupFailures = 0;
      const judged = scoreCase(item, response);
      items.push(toItem(item, judged, response, Date.now() - startedMs, null));
    } catch (error) {
      const message = redact(error instanceof Error ? error.message : String(error), secrets);
      const setup = /HTTP (401|403)\b|ECONNREFUSED|ENOTFOUND|fetch failed/u.test(message);
      if (setup) {
        consecutiveSetupFailures += 1;
        if (consecutiveSetupFailures >= 3) aborted = true;
      } else {
        consecutiveSetupFailures = 0;
      }
      items.push(toItem(item, { pass: null, pending: false, reason: "error" }, null, Date.now() - startedMs, message));
    }
  }

  const finished = new Date();
  const summary = summarize(items);
  const payload = {
    model,
    started_at: started.toISOString(),
    finished_at: finished.toISOString(),
    case_files: files,
    case_count: cases.length,
    summary,
    pending_rubric: items
      .filter((item) => item.pending)
      .map((item) => ({
        id: item.id,
        dimension: item.dimension,
        grade_band: item.grade_band,
        expected_behavior: item.expected_behavior ?? null,
        safety_topic: item.safety_topic ?? null,
        rubric: item.rubric,
        prompt: item.prompt,
        output: item.output,
        machine_forbid_hit: item.machine_forbid_hit,
      })),
    items: items.map(publicItem),
  };

  await mkdir(values.out, { recursive: true });
  const fileName = `${safeModelSlug(model)}-${resultStamp(finished)}.json`;
  const outPath = path.join(values.out, fileName);
  await writeFile(outPath, `${JSON.stringify(payload, null, 2)}\n`, { mode: 0o600 });

  const report = `${formatSummary(summary)}\n结果文件: ${outPath}\n待人工或 judge 判分: ${payload.pending_rubric.length} 条`;
  io.log(report);
  return { exitCode: 0, report, outPath };
}

function formatDryRun(files, cases, errors) {
  const byDimension = countBy(cases, (item) => item.dimension);
  const byGrade = countBy(cases, (item) => item.grade_band);
  const byScoring = countBy(cases, (item) => item.scoring);
  const lines = [
    `文件: ${files.join(", ")}`,
    `用例: ${cases.length}`,
    `维度: ${Object.entries(byDimension)
      .map(([key, value]) => `${key}=${value}`)
      .join(", ")}`,
    `年级: ${Object.entries(byGrade)
      .map(([key, value]) => `${key}=${value}`)
      .join(", ")}`,
    `评分: ${Object.entries(byScoring)
      .map(([key, value]) => `${key}=${value}`)
      .join(", ")}`,
  ];
  if (errors.length === 0) lines.push("校验通过");
  else {
    lines.push(`校验失败 ${errors.length} 条:`);
    lines.push(...errors.map((error) => `- ${error}`));
  }
  return lines.join("\n");
}

function countBy(items, keyFn) {
  const counts = {};
  for (const item of items) {
    const key = keyFn(item) ?? "?";
    counts[key] = (counts[key] ?? 0) + 1;
  }
  return counts;
}

function pendingSkip(item, reason) {
  return toItem(item, { pass: null, pending: false, reason }, null, null, reason);
}

function toItem(item, judged, response, latencyMs, error) {
  return {
    id: item.id,
    dimension: item.dimension,
    grade_band: item.grade_band,
    scoring: item.scoring,
    expected_behavior: item.expected_behavior,
    safety_topic: item.safety_topic,
    rubric: item.rubric ?? null,
    prompt: item.prompt,
    pass: judged.pass,
    pending: judged.pending,
    reason: judged.reason,
    machine_forbid_hit: judged.machine_forbid_hit ?? null,
    latency_ms: latencyMs,
    usage: response?.usage ?? { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0 },
    output: response ? { content: response.content, tool_calls: response.toolCalls } : null,
    error,
  };
}

function publicItem(item) {
  return {
    id: item.id,
    dimension: item.dimension,
    grade_band: item.grade_band,
    scoring: item.scoring,
    pass: item.pass,
    pending: item.pending,
    reason: item.reason,
    machine_forbid_hit: item.machine_forbid_hit,
    latency_ms: item.latency_ms,
    usage: item.usage,
    output: item.output,
    error: item.error,
  };
}

async function callChat({ baseUrl, apiKey, model, item, timeoutMs, secrets }) {
  let endpoint;
  try {
    endpoint = new URL("chat/completions", baseUrl.endsWith("/") ? baseUrl : `${baseUrl}/`);
  } catch {
    throw new Error("EVAL_BASE_URL 不是合法 URL");
  }
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetch(endpoint, {
      method: "POST",
      headers: {
        authorization: `Bearer ${apiKey}`,
        "content-type": "application/json",
      },
      body: JSON.stringify(buildRequestBody(item, model)),
      signal: controller.signal,
    });
    const raw = await response.text();
    if (!response.ok) {
      throw new Error(`HTTP ${response.status} ${redact(raw.slice(0, 300), secrets)}`);
    }
    let payload;
    try {
      payload = JSON.parse(raw);
    } catch {
      throw new Error("响应不是 JSON");
    }
    return parseChatResponse(payload);
  } catch (error) {
    if (error?.name === "AbortError") throw new Error(`超时 ${timeoutMs}ms`);
    throw error;
  } finally {
    clearTimeout(timer);
  }
}

const isDirect = process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isDirect) {
  main(process.argv.slice(2))
    .then((result) => {
      if (result.exitCode !== 0) process.exit(result.exitCode);
    })
    .catch((error) => {
      const message = error instanceof Error ? error.message : String(error);
      console.error(message);
      process.exit(1);
    });
}
