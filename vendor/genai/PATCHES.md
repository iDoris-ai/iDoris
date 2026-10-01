# Vendored genai

- Version: `genai 0.6.5`, from crates.io; archive SHA-256: `1d12aba7e9dc2c4d54654566dc3dc8383b5cb52e0cfc5754989afe0480d933e3` (Cargo.lock checksum).
- The explicit example and integration-test targets were removed from the normalized manifest because their files are not needed to build the library. Their dev dependencies were also removed; the added unit tests use existing library dependencies.
- Trailing whitespace in the published files was removed to satisfy `git diff --check`.
- In `src/webc/web_stream.rs`, SSE EOF discards unterminated frames, and CRLF split across chunks is normalized without creating a false frame boundary. Unit tests cover incomplete and complete frames, delimiter EOF, CRLF splits, one-byte chunks, and lone-CR frame separators.
- In `src/webc/event_source_stream.rs`, SSE data fields retain empty values and inter-field newlines, including colonless `data` fields. Only one optional ASCII space after the colon is removed, so whitespace cannot turn a malformed terminal into `[DONE]`. Parser unit tests and `crates/idoris-upstream/tests/remote_sse_termination.rs` public-API regressions cover false terminals and valid completion.
- Run the focused tests with `cargo test -p genai --lib webc::` and `cargo test -p idoris-upstream --test remote_sse_termination` from the workspace root.
- When an upstream genai release fixes these behaviors, remove the workspace patch and vendored source, update the exact version constraint, and rerun the regression tests.
