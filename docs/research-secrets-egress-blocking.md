# Secrets at Egress — Primary-Source Research (2026-09-19)

Versions verified via GitHub releases, PyPI, npm, crates.io.

| Tool | Version + date | Detection approach | Library? | Documented FP notes |
|---|---|---|---|---|
| **gitleaks** ✅ | v8.30.1 (2026-03-21) | TOML `[[rules]]`: regex + Shannon `entropy` + `keywords` prefilter + `[[rules.allowlists]]`. 222 rules (counted) | **Yes, Go** ✅ `NewDetectorDefaultConfig()` → `DetectBytes([]byte)`. No official Rust/Node binding | Entropy FPs; open issue #1830 ✅ |
| **trufflehog** ✅ | v3.97.5 (2026-09-16) | Regex + **active API verification**; 904 detector dirs (counted); verified/unverified/unknown | README: "no guarantees… on the stability of the public APIs" ✅ → CLI-only. **AGPL-3.0** | Verification *eliminates* FPs but costs network |
| **detect-secrets** ✅ | v1.5.0 (2024-05-06) | Plugins: regex, **entropy** (Base64 4.5 / Hex 3.0), `KeywordDetector`; filters | **Yes, Python** ✅ `SecretsCollection.scan_file()`; `--string` | Caveats section ✅ |
| **ggshield** ✅ | v1.54.0 (2026-08-26) | Cloud API (500+ types); **real-time AI-assistant hooks** | CLI + GitGuardian API (MIT CLI) | Cloud, not local |
| **git-secrets** ✅ | AWS Labs | egrep regex only, no entropy; `--scan -` reads stdin | No — bash + git hooks | "not guaranteed to catch them **all**" ✅ |
| **secrets-patterns-db** ✅ | 1600+ regexes, pushed 2025-08-06 (stale) | Rule *source*; converts to gitleaks/trufflehog | Rule data (CC-BY-SA-4.0) | Confidence levels, ReDoS-tested ✅ |
| **Presidio** ✅ | data-privacy-stack, pushed 2026-09-17, MIT, 10.9k★ | Regex + NER + checksum + context | **Yes, Python + REST/Docker** ✅ | Own warning; eval ⚠️ |
| **open-guardian** ✅ | Rust 0.6.1 (2026-09-12), Apache-2.0, 77★ | "egress DLP proxy … context DLP for tool output" | Rust crate | none |
| **LeakShield** ✅ | pushed 2026-07-13, Apache-2.0, 5★, **pre-alpha** | Go gateway + Python gRPC inspector; Presidio + local LLM judge | Self-hosted service | none |
| **trylonai/gateway** ✅ | pushed 2025-06-25 (**stale**), 131★, license NOASSERTION | FastAPI LLM firewall, PII redaction, `/safeguard` | Service | none |

## Notes

**gitleaks — best fit for a Go gateway.** `DetectBytes` is a real per-request API. Risks: README now says ✅ **"Gitleaks is feature complete… Future releases will be security patches only"**, author moved to `betterleaks/betterleaks` (v1.8.1, 2026-08-18). Perf flags ✅: `--max-target-megabytes`, `--max-decode-depth`/`--max-archive-depth` (default 0 = off), `--diagnostics cpu,mem`, and `--timeout` **default 0 = no timeout**. `git` mode shells to `git log -p` (full history). FP evidence ✅: open #1830 — entropy detection flags plain dictionary words/placeholders ("false positives have outweigh true detections"). #1775 (`generic-api-key` on Yocto/BitBake) and #1578 (`public_key=`, `monkey=`) ⚠️ titles only.

**Node.** No official binding. `@visulima/secret-scanner` v2.0.1 (2026-08-19, MIT) ✅ is a **Rust port via NAPI** with 1,058 rules (gitleaks 222 + Kingfisher 825 + 11) and **~11 ms per 550 KB** ✅ — the only concrete latency number found. `@b12k/gitleaks` just spawns the binary ⚠️.

**trufflehog verification is an egress paradox** ✅: it "confirmed the credential is valid by testing it against the service's API" (AWS → `GetCallerIdentity`). Inline, it sends the very secret you're blocking to a third party, and is network-bound (detector timeouts ~30s), not millisecond-scale. It is AGPL-3.0, complicating embedding. Custom detectors POST matches to a webhook (more egress) ✅. Keep it out of the request path.

**detect-secrets** ✅ is diff/baseline-oriented by design, avoiding full history. Inline `pragma: allowlist secret` / `nextline secret` ✅. Caveats ✅: *"not meant to be a sure-fire solution… Only proper developer education can truly do that"*; **won't prevent "Multi-line secrets" or "Default passwords that don't trigger the KeywordDetector (e.g. `login = "hunter2"`)"**. Its README does **not** position it vs gitleaks ❓. v1.5.0 is ~2.3 years old ✅.

**Spectral** ❓: no public open-source scanner repo found — `SpectralOps` publishes only integrations/examples (`spectral-goat`, GH Action, pre-commit); the core scanner appears closed-source/commercial. `keyscope` (Rust, 412★, 2025-07-24) is a key-validation tool, not inline filtering.

**ggshield** ✅ is the only mainstream scanner with true **runtime** blocking: `ggshield install` hooks Cursor, Claude Code, Copilot Chat, Codex, Mistral Vibe 2.21+, scanning interactions "in real time, blocking actions that contain secrets." Caveat: needs GitGuardian cloud auth — content goes to a third-party API (README claims only metadata stored).

**Runtime/egress-native projects (item 5).** **open-guardian** (Rust, Apache-2.0, 77★) is closest to this exact use case. **LeakShield** (Apache-2.0) has the most complete open architecture (Go gateway → gRPC inspector → Presidio + local LLM, block/mask, SSE) but self-labels **pre-alpha**. **trylonai/gateway** is real but stale since 2025-06 with unclear licensing. Also Rust: `keyclaw` (MIT, "Local MITM proxy that keeps secrets out of LLM traffic"), `scrub-cli` (gitleaks-rule redaction). Presidio ✅ is maintained (2026-09-17), has a REST API and a **LiteLLM proxy PII-masking sample**, but warns: *"there is no guarantee that Presidio will find all sensitive information. Consequently, additional systems and protections should be employed."* One third-party eval reports recall 0.74 / precision 0.51 ⚠️ (snippet; paper not fetched).

## Benchmarks (item 6)

**No vendor-agnostic benchmark of gitleaks / trufflehog / detect-secrets FP or FN rates exists that I could find — stated explicitly rather than inventing numbers.** The one peer-reviewed source ✅ is ISSTA 2026, *Checked-In Secret Detection: Strings Are All You Need* (arXiv:2608.04523): 98.74% F1 on SecretBench for its ML tool **Secretron**, and it states the limitation directly: *"Existing regex-based detection approaches suffer from fundamental limitations, as secrets often lack identifiable patterns, resulting in poor precision and recall."* That is repo scanning, not per-request egress, and is not a comparison of the tools above. Vendor comparisons (safeguard.sh, "Best Secrets Detection Tools Compared 2026") are marketing ⚠️.

## Fundamental limitation (item 7)

Primary-source support, all ✅: arXiv:2608.04523 (secrets "lack identifiable patterns" — a random 20-char password is invisible to regex and usually to entropy), detect-secrets caveats, git-secrets ("not guaranteed to catch them all… extra means of insurance"), Presidio ("no guarantee"). On **distinguishing a user's own secret from a similar-looking string**: this follows from format-based detection and is supported by gitleaks #1830 ✅, but **no primary source states it in exactly those terms** ❓. Design implication: make detection advisory/redact-by-default and allowlist known-safe values rather than claiming completeness.

## Sources

gitleaks: https://github.com/gitleaks/gitleaks/releases/tag/v8.30.1 · https://raw.githubusercontent.com/gitleaks/gitleaks/master/README.md · https://raw.githubusercontent.com/gitleaks/gitleaks/master/detect/detect.go · https://raw.githubusercontent.com/gitleaks/gitleaks/master/config/gitleaks.toml · https://github.com/gitleaks/gitleaks/issues/1830 · https://github.com/gitleaks/gitleaks/issues/1775 · https://github.com/gitleaks/gitleaks/issues/1578 · https://github.com/betterleaks/betterleaks
trufflehog: https://raw.githubusercontent.com/trufflesecurity/trufflehog/main/README.md · https://github.com/trufflesecurity/trufflehog/releases/tag/v3.97.5 · https://github.com/trufflesecurity/trufflehog/tree/main/pkg/detectors
detect-secrets: https://raw.githubusercontent.com/Yelp/detect-secrets/master/README.md · https://pypi.org/pypi/detect-secrets/json
ggshield: https://raw.githubusercontent.com/GitGuardian/ggshield/main/README.md · https://docs.gitguardian.com/ggshield-docs/integrations/ai-coding-tools/secret-scanning-for-ai-coding-tools
others: https://raw.githubusercontent.com/awslabs/git-secrets/master/README.rst · https://raw.githubusercontent.com/mazen160/secrets-patterns-db/master/README.md · https://presidio.dataprivacystack.org/ · https://github.com/data-privacy-stack/presidio · https://github.com/data-privacy-stack/presidio/tree/main/docs/samples/docker/litellm · https://raw.githubusercontent.com/Hesper-Labs/leakshield/main/README.md · https://raw.githubusercontent.com/trylonai/gateway/main/README.md · https://github.com/AnthonySmith96/open-guardian · https://crates.io/api/v1/crates/open-guardian · https://crates.io/api/v1/crates/keyclaw · https://crates.io/api/v1/crates/scrub-cli · https://github.com/SpectralOps/keyscope · https://arxiv.org/abs/2608.04523 · https://registry.npmjs.org/@visulima/secret-scanner
