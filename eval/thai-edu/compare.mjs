#!/usr/bin/env node
/**
 * 对比两次（或两侧各多个）泰语 edu 结果 JSON。
 *
 *   node eval/thai-edu/compare.mjs --on a.json b.json --off c.json
 *   node eval/thai-edu/compare.mjs --on a.json --off c.json --cases eval/thai-edu/cases
 *
 * 同一侧多个文件按 id 合并，后面的覆盖前面的。不发请求。
 * 通过率的算法与 lib.mjs 的 summarize 一致：分母是自动判，加上 judge 的通过和未过。
 */

import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { DIMENSIONS, loadCases } from "./lib.mjs";

const DEFAULT_MAX_TOKENS = 800;

export function scoreSource(item) {
  if (item.score_source) return item.score_source;
  if (item.pending) return "human";
  if (item.pass === true || item.pass === false) return "auto";
  if (item.error) return "error";
  return "human";
}

/** 后者覆盖前者。同一文件里重复 id 也是后者覆盖。新 id 追加在末尾。 */
export function mergeItems(groups) {
  const map = new Map();
  const order = [];
  for (const items of groups) {
    if (!Array.isArray(items)) throw new Error("结果缺少 items 数组");
    for (const item of items) {
      if (!item || typeof item.id !== "string" || item.id.length === 0) {
        throw new Error("结果条目缺少 id");
      }
      if (!map.has(item.id)) order.push(item.id);
      map.set(item.id, item);
    }
  }
  return order.map((id) => map.get(id));
}

export function completionTokens(item) {
  const value = item?.usage?.completion_tokens;
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

export function resolveMaxTokens(id, { maxTokensById, defaultMaxTokens = DEFAULT_MAX_TOKENS } = {}) {
  const value = maxTokensById?.get?.(id);
  if (Number.isInteger(value) && value > 0) return value;
  return defaultMaxTokens;
}

export function isTruncated(item, maxTokens) {
  return completionTokens(item) >= maxTokens;
}

export function summarizeItems(items, maxTokensOf) {
  const auto = items.filter((item) => scoreSource(item) === "auto");
  const judged = items.filter((item) => scoreSource(item) === "judge");
  const autoPassed = auto.filter((item) => item.pass === true).length;
  const judgePassed = judged.filter((item) => item.pass === true).length;
  const judgeFailed = judged.filter((item) => item.pass === false).length;
  const passed = autoPassed + judgePassed;
  const decided = auto.length + judgePassed + judgeFailed;
  const latencies = items.map((item) => item.latency_ms).filter((value) => typeof value === "number" && Number.isFinite(value));
  const completionSum = items.reduce((total, item) => total + completionTokens(item), 0);
  const truncated = items.filter((item) => isTruncated(item, maxTokensOf(item))).length;
  return {
    total: items.length,
    auto: auto.length,
    passed,
    passRate: decided === 0 ? null : passed / decided,
    avgLatencyMs: latencies.length === 0 ? null : latencies.reduce((a, b) => a + b, 0) / latencies.length,
    avgCompletion: items.length === 0 ? null : completionSum / items.length,
    truncated,
  };
}

function snapshot(item, maxTokensOf) {
  return {
    pass: item.pass ?? null,
    reason: item.reason ?? null,
    error: item.error ?? null,
    completion: completionTokens(item),
    latencyMs: typeof item.latency_ms === "number" ? item.latency_ms : null,
    truncated: isTruncated(item, maxTokensOf(item)),
    maxTokens: maxTokensOf(item),
  };
}

function dimensionRank(dimension) {
  const index = DIMENSIONS.indexOf(dimension);
  return index === -1 ? DIMENSIONS.length : index;
}

/**
 * @param {object[]} onItems 思考开（或 --on）合并后的条目
 * @param {object[]} offItems 思考关（或 --off）合并后的条目
 */
export function compareRuns(onItems, offItems, options = {}) {
  const maxTokensOf = (item) => resolveMaxTokens(item.id, options);
  const onMap = new Map(onItems.map((item) => [item.id, item]));
  const offMap = new Map(offItems.map((item) => [item.id, item]));
  const ids = [];
  const seen = new Set();
  for (const id of [...onMap.keys(), ...offMap.keys()]) {
    if (seen.has(id)) continue;
    seen.add(id);
    ids.push(id);
  }

  const dimensions = [...new Set(ids.map((id) => (onMap.get(id) ?? offMap.get(id)).dimension))];
  dimensions.sort((a, b) => dimensionRank(a) - dimensionRank(b) || String(a).localeCompare(String(b)));

  const rowFor = (list) => ({
    on: summarizeItems(list.map((id) => onMap.get(id)).filter(Boolean), maxTokensOf),
    off: summarizeItems(list.map((id) => offMap.get(id)).filter(Boolean), maxTokensOf),
  });

  const byDimension = {};
  for (const dimension of dimensions) {
    const inDimension = ids.filter((id) => (onMap.get(id) ?? offMap.get(id)).dimension === dimension);
    byDimension[dimension] = rowFor(inDimension);
  }

  const onPassOffFail = [];
  const onFailOffPass = [];
  const bothFail = [];
  const notFlippable = [];
  const onlyOn = [];
  const onlyOff = [];

  for (const id of ids) {
    const on = onMap.get(id);
    const off = offMap.get(id);
    const dimension = (on ?? off).dimension;
    if (!on) {
      onlyOff.push(id);
      continue;
    }
    if (!off) {
      onlyOn.push(id);
      continue;
    }
    const record = { id, dimension, on: snapshot(on, maxTokensOf), off: snapshot(off, maxTokensOf) };
    if (on.pass === true && off.pass === false) onPassOffFail.push(record);
    else if (on.pass === false && off.pass === true) onFailOffPass.push(record);
    else if (on.pass === false && off.pass === false) bothFail.push(record);
    else if (on.pass !== true && on.pass !== false || off.pass !== true && off.pass !== false) {
      notFlippable.push(record);
    }
  }

  const byId = (a, b) => dimensionRank(a.dimension) - dimensionRank(b.dimension) || a.id.localeCompare(b.id);
  onPassOffFail.sort(byId);
  onFailOffPass.sort(byId);
  bothFail.sort(byId);
  notFlippable.sort(byId);

  return {
    byDimension,
    overall: rowFor(ids),
    flips: { onPassOffFail, onFailOffPass, bothFail, notFlippable, onlyOn, onlyOff },
  };
}

function cell(value, kind) {
  if (value == null) return "—";
  if (kind === "rate") return `${(value * 100).toFixed(1)}%`;
  if (kind === "latency") return value.toFixed(0);
  if (kind === "completion") return value.toFixed(1);
  return redactSensitive(String(value));
}

/** 打印用。8 位及以上连续数字，或夹了空格、横线、括号的长数字串，换成 [已遮蔽]。 */
export function redactSensitive(value) {
  return String(value ?? "")
    .replace(/\d[\d\s\-()]{6,}\d/g, "[已遮蔽]")
    .replace(/\d{8,}/g, "[已遮蔽]");
}

function tableRow(label, pair) {
  const sides = [pair.on, pair.off];
  return [
    label,
    ...sides.flatMap((stats) => [
      cell(stats.auto),
      cell(stats.passed),
      cell(stats.passRate, "rate"),
      cell(stats.avgLatencyMs, "latency"),
      cell(stats.avgCompletion, "completion"),
      cell(stats.truncated),
    ]),
  ];
}

export function formatComparison(result, labels = {}) {
  const onLabel = labels.on ?? "思考开";
  const offLabel = labels.off ?? "思考关";
  const header = [
    "维度",
    `${onLabel}·自动判`,
    `${onLabel}·通过`,
    `${onLabel}·通过率`,
    `${onLabel}·平均延迟ms`,
    `${onLabel}·平均completion`,
    `${onLabel}·completion≥max_tokens`,
    `${offLabel}·自动判`,
    `${offLabel}·通过`,
    `${offLabel}·通过率`,
    `${offLabel}·平均延迟ms`,
    `${offLabel}·平均completion`,
    `${offLabel}·completion≥max_tokens`,
  ];
  const lines = [
    `对照：${onLabel} vs ${offLabel}`,
    "截断列 = completion_tokens ≥ 该条 max_tokens。用例没写 max_tokens 时按 800，也可用 --default-max-tokens 改。",
    "通过率分母 = 自动判 + judge 通过 + judge 未过。不含待人工、错误、judge 不确定。",
    "平均延迟只平均有数字的 latency_ms。平均 completion 把缺省当成 0，分母是该维度的全部条目。",
    "",
    `| ${header.join(" | ")} |`,
    `| ${header.map(() => "---").join(" | ")} |`,
  ];
  for (const [dimension, pair] of Object.entries(result.byDimension)) {
    lines.push(`| ${tableRow(dimension, pair).join(" | ")} |`);
  }
  lines.push(`| ${tableRow("合计", result.overall).join(" | ")} |`);

  const flips = result.flips;
  lines.push("");
  lines.push(formatFlipSection(`${onLabel}过${offLabel}不过`, flips.onPassOffFail));
  lines.push("");
  lines.push(formatFlipSection(`${onLabel}不过${offLabel}过`, flips.onFailOffPass));
  lines.push("");
  lines.push(formatFlipSection("两边都未通过（不是翻转）", flips.bothFail));
  lines.push("");
  lines.push(formatFlipSection("有一边不是通过或未通过（不是翻转）", flips.notFlippable));
  if (flips.onlyOn.length > 0 || flips.onlyOff.length > 0) {
    lines.push("");
    lines.push(`只在${onLabel}: ${flips.onlyOn.join(", ") || "—"}`);
    lines.push(`只在${offLabel}: ${flips.onlyOff.join(", ") || "—"}`);
  }
  return lines.join("\n");
}

function formatFlipSection(title, records) {
  const lines = [`${title}（${records.length}）`];
  if (records.length === 0) {
    lines.push("（无）");
    return lines.join("\n");
  }
  lines.push("| id | 维度 | 开pass | 关pass | 开reason | 关reason | 开completion | 关completion | 开截断 | 关截断 |");
  lines.push("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
  for (const record of records) {
    lines.push(`| ${[
      record.id,
      record.dimension,
      cell(record.on.pass),
      cell(record.off.pass),
      cell(record.on.reason ?? record.on.error),
      cell(record.off.reason ?? record.off.error),
      cell(record.on.completion),
      cell(record.off.completion),
      record.on.truncated ? "是" : "否",
      record.off.truncated ? "是" : "否",
    ].join(" | ")} |`);
  }
  return lines.join("\n");
}

export function parseCompareArgs(argv) {
  const on = [];
  const off = [];
  let casesDir = null;
  let defaultMaxTokens = DEFAULT_MAX_TOKENS;
  let help = false;
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === "--help" || arg === "-h") {
      help = true;
      continue;
    }
    if (arg === "--on" || arg === "--off") {
      const bucket = arg === "--on" ? on : off;
      index += 1;
      if (index >= argv.length || argv[index].startsWith("--")) {
        throw new Error(`${arg} 后面至少要有一个结果 JSON`);
      }
      while (index < argv.length && !argv[index].startsWith("--")) {
        bucket.push(argv[index]);
        index += 1;
      }
      index -= 1;
      continue;
    }
    if (arg === "--cases") {
      casesDir = argv[index + 1];
      if (!casesDir || casesDir.startsWith("--")) throw new Error("--cases 后面要有用例目录");
      index += 1;
      continue;
    }
    if (arg === "--default-max-tokens") {
      const raw = argv[index + 1];
      const parsed = Number(raw);
      if (!Number.isInteger(parsed) || parsed < 1) throw new Error("--default-max-tokens 必须是正整数");
      defaultMaxTokens = parsed;
      index += 1;
      continue;
    }
    throw new Error(`未知参数: ${arg}`);
  }
  if (!help && (on.length === 0 || off.length === 0)) {
    throw new Error("必须同时给出 --on 和 --off，各自至少一个结果 JSON");
  }
  return { on, off, casesDir, defaultMaxTokens, help };
}

export function printCompareHelp() {
  return [
    "用法: node eval/thai-edu/compare.mjs --on 结果.json [更多.json] --off 结果.json [更多.json]",
    "       [--cases 用例目录] [--default-max-tokens 800]",
    "",
    "同一侧多个文件按 id 合并，命令行上靠后的文件覆盖靠前的。",
    "不发请求，不读密钥。",
  ].join("\n");
}

async function readPayload(file) {
  let parsed;
  try {
    parsed = JSON.parse(await readFile(file, "utf8"));
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new Error(`读结果失败 ${file}: ${message}`);
  }
  if (!parsed || !Array.isArray(parsed.items)) throw new Error(`结果缺少 items 数组: ${file}`);
  return parsed.items;
}

export async function maxTokensFromCases(casesDir) {
  const { cases } = await loadCases(casesDir);
  const map = new Map();
  for (const item of cases) {
    if (Number.isInteger(item.max_tokens) && item.max_tokens > 0) map.set(item.id, item.max_tokens);
  }
  return map;
}

export async function main(argv, io = console) {
  let args;
  try {
    args = parseCompareArgs(argv);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    io.error(message);
    return { exitCode: 1, report: message };
  }
  if (args.help) {
    const report = printCompareHelp();
    io.log(report);
    return { exitCode: 0, report };
  }
  try {
    const onGroups = [];
    for (const file of args.on) onGroups.push(await readPayload(file));
    const offGroups = [];
    for (const file of args.off) offGroups.push(await readPayload(file));
    const maxTokensById = args.casesDir ? await maxTokensFromCases(args.casesDir) : new Map();
    const result = compareRuns(mergeItems(onGroups), mergeItems(offGroups), {
      maxTokensById,
      defaultMaxTokens: args.defaultMaxTokens,
    });
    const report = formatComparison(result);
    io.log(report);
    return { exitCode: 0, report, result };
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    io.error(message);
    return { exitCode: 1, report: message };
  }
}

const isDirect = process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (isDirect) {
  main(process.argv.slice(2)).then((result) => {
    process.exit(result.exitCode);
  });
}
