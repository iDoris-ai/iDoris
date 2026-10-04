import assert from "node:assert/strict";
import test from "node:test";

import {
  assertContractVersionsEqual,
  parseConformanceContractVersion,
  parseRustContractVersion,
  parseTsContractVersion,
} from "./contract-version.mjs";

test("parses all three committed version lock shapes", () => {
  assert.equal(parseTsContractVersion('export const CONTRACT_VERSION = "1.0.1";'), "1.0.1");
  assert.equal(parseRustContractVersion('pub const CONTRACT_VERSION: &str = "1.0.1";'), "1.0.1");
  assert.equal(
    parseConformanceContractVersion('expect(body.contract_version).toBe("1.0.1");'),
    "1.0.1",
  );
});

test("a one-sided TS version change fails closed", () => {
  assert.throws(
    () => assertContractVersionsEqual({ ts: "1.1", rust: "1.0.1", conformance: "1.0.1" }),
    /contract version drift/u,
  );
});

test("a one-sided Rust version change fails closed", () => {
  assert.throws(
    () => assertContractVersionsEqual({ ts: "1.0.1", rust: "1.1", conformance: "1.0.1" }),
    /contract version drift/u,
  );
});

test("a stale conformance wire expectation fails closed", () => {
  assert.throws(
    () => assertContractVersionsEqual({ ts: "1.1", rust: "1.1", conformance: "1.0.1" }),
    /contract version drift/u,
  );
});
