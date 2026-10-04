/** Parse and compare the three committed wire-contract version locks. */
export function parseTsContractVersion(source) {
  return matchVersion(source, /export const CONTRACT_VERSION\s*=\s*["']([^"']+)["']/u, "TS CONTRACT_VERSION");
}

export function parseRustContractVersion(source) {
  return matchVersion(source, /pub const CONTRACT_VERSION:\s*&str\s*=\s*["']([^"']+)["']/u, "Rust CONTRACT_VERSION");
}

export function parseConformanceContractVersion(source) {
  return matchVersion(source, /contract_version\)\.toBe\(["']([^"']+)["']\)/u, "conformance contract_version");
}

function matchVersion(source, pattern, label) {
  const match = source.match(pattern);
  if (match?.[1] === undefined) throw new Error(`cannot parse ${label}`);
  return match[1];
}

export function assertContractVersionsEqual({ ts, rust, conformance }) {
  if (ts !== rust || ts !== conformance) {
    throw new Error(
      `contract version drift: TS=${ts}, Rust=${rust}, conformance=${conformance}`,
    );
  }
  return ts;
}
