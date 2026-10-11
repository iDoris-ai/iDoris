M5-04 — credential rules, stacked on M5-03. docs/15 X-3 requires **two independent rule sets**:

- Set A, known prefixes (`sk-`, `AKIA/ASIA`, `gh?_`, `github_pat_`, `glpat-`, `xox?-`, `AIza`, `hf_`) with minimum body lengths → `api_key` S3.
- Set B, prefix-agnostic: ≥ 32 chars, Shannon entropy ≥ 4.0, adjacent char-class switch rate ≥ 0.5 → `high_entropy_secret` S3. Calibrated offline: ~91–93% recall on random alnum tokens, 0 hits on camel/snake identifiers, paths, URLs, UUIDs, sha256 hex.
- PEM private keys BEGIN…END, or to end of input when truncated → `private_key` S3.
- Credential file paths (`.ssh/`, `id_*`, `.aws/credentials`, `.netrc`, `.env` as filename, `.pem/.p12/.pfx/.jks`, keychains) → `credential_path` **S2** (owner: S2 vs S3?).

A test proves independence (real-format key hit by both sets; prefix-less random only by B; short AWS id only by A). Fake key fixtures are split with `concat!`. Every hand-run mutation turns the suite red.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
