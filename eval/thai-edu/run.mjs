#!/usr/bin/env node
/**
 * 泰语 edu 评测运行器。
 *
 *   node eval/thai-edu/run.mjs --dry-run
 *   EVAL_BASE_URL=http://127.0.0.1:8088/v1 EVAL_API_KEY=... EVAL_MODEL=... node eval/thai-edu/run.mjs
 *
 * 密钥只从环境变量读取，不写进结果文件，不打印。
 */

import { mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";

import {
  buildJudgeUserPayload,
  buildRequestBody,
  parseExtraBody,
  evaluateRunPlan,
  formatSummary,
  judgeScore,
  loadCases,
  parseChatResponse,
  parseConcurrency,
  parseJudgeVerdict,
  parseTimeoutMs,
  redact,
  resultStamp,
  safeModelSlug,
  scoreCase,
  selectCases,
  selectionMeta,
  shouldEnforceCoverage,
  summarize,
  validateCases,
} from "./lib.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const DEFAULT_CASES = path.join(HERE, "cases");
const JUDGE_PROMPT_PATH = path.join(HERE, "judge-prompt.txt");

export function printHelp() {
  return [
    "用法: node eval/thai-edu/run.mjs [--dry-run] [--cases 目录] [--out 目录]",
    "       [--only 维度,维度] [--ids id,id] [--limit N] [--timeout 秒] [--concurrency N]",
    "",
    "环境变量（只有真正发请求时才需要）:",
    "  EVAL_BASE_URL   OpenAI 兼容根路径，含 /v1，例如 http://127.0.0.1:8088/v1",
    "  EVAL_API_KEY    Bearer 令牌。不会写入结果，也不会打印。",
    "  EVAL_MODEL      模型名，用于请求体和结果文件名。",
    "  EVAL_EXTRA_BODY 可选。JSON 对象，合并进请求体（不能覆盖 model/messages/stream/tools/tool_choice）。",
    "  EVAL_TIMEOUT_MS 单条超时毫秒。命令行 --timeout（秒）优先。默认 120 秒。",
    "  JUDGE_BASE_URL  可选。与 JUDGE_API_KEY、JUDGE_MODEL 同时设置才启用 judge。",
    "  JUDGE_API_KEY   可选。只从环境变量读取，不落盘、不打印。",
    "  JUDGE_MODEL     可选。judge 模型名。",
    "",
    "--dry-run       不发请求。仍对目录内全部用例做全量覆盖校验，忽略子集过滤。",
    "--only          只跑这些维度（逗号分隔）。",
    "--ids           只跑这些 id（逗号分隔），与 --only 取交集。",
    "--limit         筛选后再取前 N 条（文件顺序）。",
    "--timeout       单条请求超时，单位秒。默认 120。失败会再试 1 次。",
    "--concurrency   并发条数，默认 1。",
    "退出码: 0 校验通过或已写出完整结果；130 收到中断但已写已完成结果；1 参数、校验或环境错误。",
    "用例没答对不会让进程失败。子集结果会标明，不能当作全量。",
  ].join("\n");
}

export async function main(argv, env = process.env, io = console, options = {}) {
  const stop = options.stopController ?? new AbortController();
  let removeSignals = () => {};
  if (options.installSignals) {
    let hits = 0;
    const onSignal = () => {
      hits += 1;
      stop.abort();
      if (hits >= 2) process.exit(130);
    };
    process.on("SIGINT", onSignal);
    process.on("SIGTERM", onSignal);
    removeSignals = () => {
      process.off("SIGINT", onSignal);
      process.off("SIGTERM", onSignal);
    };
  }
  try {
    return await run(argv, env, io, stop.signal);
  } finally {
    removeSignals();
  }
}

async function run(argv, env, io, stopSignal) {
  let values;
  try {
    ({ values } = parseArgs({
      args: argv,
      options: {
        "dry-run": { type: "boolean", default: false },
        cases: { type: "string", default: DEFAULT_CASES },
        out: { type: "string", default: path.join(HERE, "results") },
        only: { type: "string" },
        ids: { type: "string" },
        limit: { type: "string" },
        timeout: { type: "string" },
        concurrency: { type: "string" },
        help: { type: "boolean", default: false },
      },
      strict: true,
    }));
  } catch (error) {
    const report = error instanceof Error ? error.message : String(error);
    io.error(report);
    return { exitCode: 1, report };
  }

  if (values.help) {
    io.log(printHelp());
    return { exitCode: 0, report: printHelp() };
  }

  let loaded;
  try {
    loaded = await loadCases(values.cases);
  } catch (error) {
    const report = error instanceof Error ? error.message : String(error);
    io.error(report);
    return { exitCode: 1, report };
  }

  const timeout = parseTimeoutMs(values.timeout, env.EVAL_TIMEOUT_MS);
  if (timeout.error) {
    io.error(timeout.error);
    return { exitCode: 1, report: timeout.error };
  }
  const concurrency = parseConcurrency(values.concurrency);
  if (concurrency.error) {
    io.error(concurrency.error);
    return { exitCode: 1, report: concurrency.error };
  }

  const selection = selectCases(loaded.cases, {
    only: values.only,
    ids: values.ids,
    limit: values.limit,
  });
  if (selection.errors.length > 0) {
    const report = selection.errors.join("\n");
    io.error(report);
    return { exitCode: 1, report };
  }

  if (values["dry-run"]) {
    const errors = validateCases(loaded.cases);
    let report = formatDryRun(loaded.files, loaded.cases, errors);
    if (selection.only || selection.ids || selection.limit != null) {
      report += "\ndry-run 不应用子集过滤，仍对目录内全部用例做全量覆盖校验。";
    }
    io.log(report);
    return { exitCode: errors.length === 0 ? 0 : 1, report };
  }

  const meta = selectionMeta({
    casesDir: values.cases,
    defaultDir: DEFAULT_CASES,
    only: selection.only,
    ids: selection.ids,
    limit: selection.limit,
  });
  const enforceCoverage = shouldEnforceCoverage({
    casesDir: values.cases,
    defaultDir: DEFAULT_CASES,
    only: selection.only,
    ids: selection.ids,
    limit: selection.limit,
  });
  const planErrors = evaluateRunPlan({
    chosen: selection.cases,
    coverageCases: loaded.cases,
    enforceCoverage,
  });
  if (planErrors.length > 0) {
    const report = formatDryRun(loaded.files, selection.cases, planErrors);
    io.error(report);
    return { exitCode: 1, report };
  }

  const judgeEnv = readJudgeEnv(env);
  if (judgeEnv.error) {
    io.error(judgeEnv.error);
    return { exitCode: 1, report: judgeEnv.error };
  }

  const baseUrl = env.EVAL_BASE_URL?.trim();
  const apiKey = env.EVAL_API_KEY;
  const model = env.EVAL_MODEL?.trim();
  if (!baseUrl || !apiKey || !model) {
    const report = "缺少 EVAL_BASE_URL、EVAL_API_KEY 或 EVAL_MODEL。未发请求。";
    io.error(report);
    return { exitCode: 1, report };
  }

  const extra = parseExtraBody(env.EVAL_EXTRA_BODY);
  if (extra.error) {
    io.error(extra.error);
    return { exitCode: 1, report: extra.error };
  }

  const secrets = collectSecrets([apiKey, judgeEnv.apiKey], [baseUrl, judgeEnv.baseUrl]);
  const judgePrompt = judgeEnv.enabled ? (await readFile(JUDGE_PROMPT_PATH, "utf8")).trim() : null;
  const started = new Date();
  const slots = new Array(selection.cases.length);
  let consecutiveSetupFailures = 0;
  let setupAborted = false;

  const runOne = async (item) => {
    if (stopSignal.aborted) return null;
    if (setupAborted) return pendingSkip(item, "skipped_after_setup_failures");
    const startedMs = Date.now();
    try {
      const response = await withRetry(
        () => postChat({
          baseUrl,
          apiKey,
          body: buildRequestBody(item, model, extra.extraBody),
          timeoutMs: timeout.timeoutMs,
          stopSignal,
          secrets,
        }),
        stopSignal,
      );
      consecutiveSetupFailures = 0;
      const machine = scoreCase(item, response);
      let judged = machine;
      if (machine.pending && judgeEnv.enabled) {
        try {
          judged = await scoreWithJudge({
            judgeEnv,
            judgePrompt,
            item,
            response,
            timeoutMs: timeout.timeoutMs,
            stopSignal,
            secrets,
            machineForbidHit: machine.machine_forbid_hit ?? null,
          });
        } catch (error) {
          if (error?.code === "INTERRUPTED" || stopSignal.aborted) {
            return toItem(item, machine, response, Date.now() - startedMs, null);
          }
          throw error;
        }
      }
      return toItem(item, judged, response, Date.now() - startedMs, null);
    } catch (error) {
      if (error?.code === "INTERRUPTED" || stopSignal.aborted) return null;
      const message = redact(error instanceof Error ? error.message : String(error), secrets);
      const setup = /HTTP (401|403)\b|ECONNREFUSED|ENOTFOUND|fetch failed/u.test(message);
      if (setup && concurrency.concurrency === 1) {
        consecutiveSetupFailures += 1;
        if (consecutiveSetupFailures >= 3) setupAborted = true;
      } else if (concurrency.concurrency === 1) {
        consecutiveSetupFailures = 0;
      }
      return toItem(item, { pass: null, pending: false, score_source: "error", reason: "error" }, null, Date.now() - startedMs, message);
    }
  };

  if (concurrency.concurrency === 1) {
    for (let index = 0; index < selection.cases.length; index += 1) {
      if (stopSignal.aborted) break;
      slots[index] = await runOne(selection.cases[index]);
      if (stopSignal.aborted && slots[index] == null) break;
    }
  } else {
    await runPool(selection.cases, concurrency.concurrency, async (item, index) => {
      slots[index] = await runOne(item);
    }, stopSignal);
  }

  const interrupted = stopSignal.aborted;
  const items = slots.filter((item) => item != null);
  const finished = new Date();
  const completeFullSet = meta.represents_full_set && !interrupted;
  const summary = summarize(items, {
    scope: completeFullSet ? "full" : "subset",
    represents_full_set: completeFullSet,
    interrupted,
    filters: meta,
  });
  const summaryText = formatSummary(summary);
  const payload = {
    model,
    scope: completeFullSet ? "full" : "subset",
    represents_full_set: completeFullSet,
    selection: meta,
    interrupted,
    judge: { enabled: judgeEnv.enabled, model: judgeEnv.enabled ? judgeEnv.model : null },
    started_at: started.toISOString(),
    extra_body: extra.extraBody,
    finished_at: finished.toISOString(),
    case_files: loaded.files,
    case_count: selection.cases.length,
    completed_count: items.length,
    summary,
    summary_text: summaryText,
    pending_rubric: items.filter((item) => item.pending).map(pendingView),
    items: items.map(publicItem),
  };

  let outPath;
  try {
    await mkdir(values.out, { recursive: true });
    const fileName = `${safeModelSlug(model)}-${resultStamp(finished)}.json`;
    outPath = path.join(values.out, fileName);
    await writeFile(outPath, `${JSON.stringify(payload, null, 2)}\n`, { mode: 0o600 });
  } catch (error) {
    const report = redact(error instanceof Error ? error.message : String(error), secrets);
    io.error(report);
    return { exitCode: 1, report };
  }

  const judgeStats = summary.overall;
  const lines = [
    interrupted ? "收到中断，已写入已完成的结果。" : null,
    summaryText,
    `结果文件: ${outPath}`,
    `已完成: ${items.length} / ${selection.cases.length}`,
    `待人工: ${payload.pending_rubric.length} 条`,
    `judge: ${judgeStats.judge_scored} 条（通过 ${judgeStats.judge_passed} / 未过 ${judgeStats.judge_failed} / 不确定 ${judgeStats.judge_uncertain}）`,
  ].filter(Boolean);
  const report = lines.join("\n");
  io.log(report);
  return { exitCode: interrupted ? 130 : 0, report, outPath, payload };
}

function readJudgeEnv(env) {
  const baseUrl = env.JUDGE_BASE_URL?.trim() || "";
  const apiKey = env.JUDGE_API_KEY ?? "";
  const model = env.JUDGE_MODEL?.trim() || "";
  const present = [baseUrl, apiKey, model].filter((value) => value.length > 0).length;
  if (present === 0) return { enabled: false, baseUrl: "", apiKey: "", model: "" };
  if (present !== 3) {
    return { error: "JUDGE_BASE_URL、JUDGE_API_KEY、JUDGE_MODEL 必须同时设置，或同时不设置。未发请求。" };
  }
  return { enabled: true, baseUrl, apiKey, model };
}

function collectSecrets(keys, urls) {
  const secrets = [];
  for (const key of keys) {
    if (typeof key === "string" && key.length > 0) secrets.push(key);
  }
  for (const raw of urls) {
    if (!raw) continue;
    try {
      const url = new URL(raw);
      if (url.password) secrets.push(url.password);
      if (url.username) secrets.push(url.username);
    } catch {
      // 非法 URL 留给请求阶段报错，不把原始字符串打出来。
    }
  }
  return secrets;
}

async function scoreWithJudge({ judgeEnv, judgePrompt, item, response, timeoutMs, stopSignal, secrets, machineForbidHit }) {
  try {
    const judged = await withRetry(
      () => postChat({
        baseUrl: judgeEnv.baseUrl,
        apiKey: judgeEnv.apiKey,
        body: {
          model: judgeEnv.model,
          temperature: 0,
          stream: false,
          max_tokens: 300,
          messages: [
            { role: "system", content: judgePrompt },
            { role: "user", content: JSON.stringify(buildJudgeUserPayload(item, response)) },
          ],
        },
        timeoutMs,
        stopSignal,
        secrets,
      }),
      stopSignal,
    );
    const parsed = parseJudgeVerdict(judged.content);
    return judgeScore(parsed.verdict, parsed.reason, machineForbidHit);
  } catch (error) {
    if (error?.code === "INTERRUPTED" || stopSignal.aborted) throw error;
    const message = redact(error instanceof Error ? error.message : String(error), secrets);
    return judgeScore("uncertain", `judge 请求失败: ${message}`, machineForbidHit);
  }
}

async function withRetry(fn, stopSignal) {
  let last;
  for (let attempt = 0; attempt < 2; attempt += 1) {
    if (stopSignal.aborted) {
      const error = new Error("interrupted");
      error.code = "INTERRUPTED";
      throw error;
    }
    try {
      return await fn();
    } catch (error) {
      if (error?.code === "INTERRUPTED" || stopSignal.aborted) {
        const interrupted = new Error("interrupted");
        interrupted.code = "INTERRUPTED";
        throw interrupted;
      }
      last = error;
    }
  }
  throw last;
}

async function runPool(items, concurrency, worker, stopSignal) {
  let cursor = 0;
  const workers = Math.min(concurrency, items.length);
  async function loop() {
    for (;;) {
      if (stopSignal.aborted) return;
      const index = cursor;
      cursor += 1;
      if (index >= items.length) return;
      await worker(items[index], index);
    }
  }
  await Promise.all(Array.from({ length: workers }, () => loop()));
}

async function postChat({ baseUrl, apiKey, body, timeoutMs, stopSignal, secrets }) {
  let endpoint;
  try {
    endpoint = new URL("chat/completions", baseUrl.endsWith("/") ? baseUrl : `${baseUrl}/`);
  } catch {
    throw new Error("请求 URL 不合法");
  }
  const timeout = AbortSignal.timeout(timeoutMs);
  const signal = AbortSignal.any([timeout, stopSignal]);
  try {
    const response = await fetch(endpoint, {
      method: "POST",
      headers: {
        authorization: `Bearer ${apiKey}`,
        "content-type": "application/json",
      },
      body: JSON.stringify(body),
      signal,
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
    if (stopSignal.aborted) {
      const interrupted = new Error("interrupted");
      interrupted.code = "INTERRUPTED";
      throw interrupted;
    }
    if (timeout.aborted || error?.name === "AbortError" || error?.name === "TimeoutError") {
      throw new Error(`超时 ${timeoutMs}ms`);
    }
    throw error;
  }
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
  return toItem(item, { pass: null, pending: false, score_source: "skipped", reason }, null, null, reason);
}

function toItem(item, judged, response, latencyMs, error) {
  const scoreSource = error ? judged.score_source ?? "error" : judged.score_source ?? (judged.pending ? "human" : "auto");
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
    score_source: scoreSource,
    judge_verdict: judged.judge_verdict ?? null,
    reason: judged.reason,
    machine_forbid_hit: judged.machine_forbid_hit ?? null,
    latency_ms: latencyMs,
    usage: response?.usage ?? { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0 },
    output: response ? { content: response.content, tool_calls: response.toolCalls } : null,
    error,
  };
}

function pendingView(item) {
  return {
    id: item.id,
    dimension: item.dimension,
    grade_band: item.grade_band,
    expected_behavior: item.expected_behavior ?? null,
    safety_topic: item.safety_topic ?? null,
    rubric: item.rubric,
    prompt: item.prompt,
    output: item.output,
    machine_forbid_hit: item.machine_forbid_hit,
  };
}

function publicItem(item) {
  return {
    id: item.id,
    dimension: item.dimension,
    grade_band: item.grade_band,
    scoring: item.scoring,
    score_source: item.score_source,
    judge_verdict: item.judge_verdict,
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

const isDirect = process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isDirect) {
  main(process.argv.slice(2), process.env, console, { installSignals: true })
    .then((result) => {
      process.exit(result.exitCode);
    })
    .catch((error) => {
      const message = error instanceof Error ? error.message : String(error);
      console.error(message);
      process.exit(1);
    });
}
