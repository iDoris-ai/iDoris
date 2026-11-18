import assert from "node:assert/strict";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { main, mergeItems, compareRuns, formatComparison, parseCompareArgs, redactSensitive } from "./compare.mjs";

function item(partial) {
  return {
    dimension: "subject_math",
    grade_band: "小学低年级",
    scoring: "exact",
    pass: false,
    pending: false,
    reason: "exact_mismatch",
    latency_ms: 10,
    usage: { completion_tokens: 4 },
    ...partial,
  };
}

test("later file overwrites the same id and new ids append", () => {
  const merged = mergeItems([
    [item({ id: "a", pass: false }), item({ id: "b", pass: true })],
    [item({ id: "b", pass: false, reason: "replaced" }), item({ id: "c", pass: true })],
  ]);
  assert.deepEqual(merged.map((row) => row.id), ["a", "b", "c"]);
  assert.equal(merged[1].reason, "replaced");
  assert.equal(merged[1].pass, false);
});

test("pass rate ignores pending and errors; truncation uses per-id max tokens", () => {
  const on = [
    item({ id: "ok", pass: true, reason: "exact", latency_ms: 100, usage: { completion_tokens: 8 } }),
    item({ id: "cut", pass: false, latency_ms: 50, usage: { completion_tokens: 10 } }),
    item({ id: "wait", dimension: "age_appropriate", scoring: "rubric", pass: null, pending: true, reason: "pending_rubric", usage: { completion_tokens: 3 } }),
    item({ id: "boom", pass: null, pending: false, reason: "error", error: "terminated", latency_ms: 7, usage: { completion_tokens: 0 } }),
  ];
  const off = [
    item({ id: "ok", pass: false, latency_ms: 20, usage: { completion_tokens: 2 } }),
    item({ id: "cut", pass: true, reason: "exact", latency_ms: 30, usage: { completion_tokens: 9 } }),
    item({ id: "wait", dimension: "age_appropriate", scoring: "rubric", pass: null, pending: true, reason: "pending_rubric", usage: { completion_tokens: 1 } }),
    item({ id: "boom", pass: true, reason: "exact", latency_ms: 40, usage: { completion_tokens: 5 } }),
    item({ id: "only-off", pass: true, reason: "exact" }),
  ];
  const result = compareRuns(on, off, {
    maxTokensById: new Map([["cut", 10]]),
    defaultMaxTokens: 800,
  });
  assert.equal(result.overall.on.auto, 2);
  assert.equal(result.overall.on.passed, 1);
  assert.equal(result.overall.on.passRate, 0.5);
  assert.equal(result.overall.on.truncated, 1);
  assert.equal(result.overall.off.auto, 4);
  assert.equal(result.overall.off.passed, 3);
  assert.equal(result.overall.off.truncated, 0);
  assert.equal(result.byDimension.age_appropriate.on.auto, 0);
  assert.equal(result.byDimension.age_appropriate.on.passRate, null);
  assert.deepEqual(result.flips.onPassOffFail.map((row) => row.id), ["ok"]);
  assert.deepEqual(result.flips.onFailOffPass.map((row) => row.id), ["cut"]);
  assert.equal(result.flips.onFailOffPass[0].on.truncated, true);
  assert.equal(result.flips.onFailOffPass[0].off.truncated, false);
  assert.deepEqual(result.flips.notFlippable.map((row) => row.id), ["boom", "wait"]);
  assert.deepEqual(result.flips.onlyOff, ["only-off"]);
  assert.equal(result.overall.on.avgCompletion, (8 + 10 + 3 + 0) / 4);
});

test("judge uncertain stays out of the denominator and a judge fail counts", () => {
  const on = [
    item({ id: "j-pass", score_source: "judge", pass: true, reason: "ok" }),
    item({ id: "j-unsure", score_source: "judge", pass: null, pending: false, reason: "uncertain" }),
  ];
  const off = [
    item({ id: "j-pass", score_source: "judge", pass: false, reason: "no" }),
    item({ id: "j-unsure", score_source: "judge", pass: null, pending: false, reason: "uncertain" }),
  ];
  const result = compareRuns(on, off);
  assert.equal(result.overall.on.auto, 0);
  assert.equal(result.overall.on.passed, 1);
  assert.equal(result.overall.on.passRate, 1);
  assert.equal(result.overall.off.passed, 0);
  assert.equal(result.overall.off.passRate, 0);
  assert.deepEqual(result.flips.onPassOffFail.map((row) => row.id), ["j-pass"]);
});

test("printed reasons hide digit strings that look like phone numbers", () => {
  const phone = "0800001007";
  const on = [item({ id: "pii-x", dimension: "pii", pass: false, reason: `forbid:${phone}` })];
  const off = [item({ id: "pii-x", dimension: "pii", pass: true, reason: "exact" })];
  const report = formatComparison(compareRuns(on, off));
  assert.equal(report.includes(phone), false);
  assert.match(report, /forbid:\[已遮蔽\]/);
  assert.equal(redactSensitive("โทร 080-000-1007"), "โทร [已遮蔽]");
});

test("parseCompareArgs keeps every file after a side flag until the next flag", () => {
  const args = parseCompareArgs([
    "--on", "a.json", "b.json",
    "--off", "c.json",
    "--default-max-tokens", "80",
    "--cases", "cases",
  ]);
  assert.deepEqual(args.on, ["a.json", "b.json"]);
  assert.deepEqual(args.off, ["c.json"]);
  assert.equal(args.defaultMaxTokens, 80);
  assert.equal(args.casesDir, "cases");
  assert.throws(() => parseCompareArgs(["--on", "a.json"]), /必须同时给出/);
});

test("main merges two on-files and prints the flip tables", async () => {
  const dir = await mkdtemp(path.join(os.tmpdir(), "thai-edu-compare-"));
  try {
    const early = path.join(dir, "early.json");
    const late = path.join(dir, "late.json");
    const off = path.join(dir, "off.json");
    const payload = (rows) => JSON.stringify({ items: rows });
    await writeFile(early, payload([
      item({ id: "keep", pass: true, reason: "exact" }),
      item({ id: "flip", pass: true, reason: "exact", usage: { completion_tokens: 800 } }),
    ]));
    await writeFile(late, payload([
      item({ id: "flip", pass: false, reason: "exact_mismatch", usage: { completion_tokens: 4 } }),
    ]));
    await writeFile(off, payload([
      item({ id: "keep", pass: true, reason: "exact" }),
      item({ id: "flip", pass: true, reason: "exact", usage: { completion_tokens: 3 } }),
    ]));
    let report = "";
    const result = await main(["--on", early, late, "--off", off, "--default-max-tokens", "800"], {
      log(line) {
        report += `${line}\n`;
      },
      error(line) {
        report += `${line}\n`;
      },
    });
    assert.equal(result.exitCode, 0, report);
    assert.match(report, /思考开·自动判/);
    assert.match(report, /思考关·completion≥max_tokens/);
    assert.match(report, /思考开不过思考关过（1）/);
    assert.match(report, /\| flip \|/);
    assert.equal(result.result.flips.onFailOffPass[0].on.pass, false);
    assert.equal(result.result.overall.on.passed, 1);
    assert.equal(result.result.overall.on.auto, 2);
    assert.equal(report.includes("sk-"), false);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
