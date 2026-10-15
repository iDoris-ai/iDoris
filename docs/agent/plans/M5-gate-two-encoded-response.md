# Gate-two encoded response credential checks

- Reuse L0's one-layer token decoders for known-credential comparison: standard
  and URL-safe base64 (padded/unpadded), hex (optional 0x), and percent encoding.
- Match the credential's bytes before decoding UTF-8, so binary wrappers cannot
  hide it. Also normalize valid decoded UTF-8 for the same escape comparisons.
  Exact comparison admits short encodings; ordinary L0 thresholds stay unchanged.
- Inspect response bodies and header names/values before delivery or caching.
  Existing aggregate UTF-16 limits, invalid-UTF-8 rejection and value-free errors
  remain in force. No extra secret-source read or secret-bearing log is added.
- This remains a buffered pure core. Transport decompression, arbitrary/nested
  encodings and streaming delivery require separate integration. Broker storage,
  grant/API wire and deployment identity remain owner decisions.
- Baseline: a Base64 echo of a synthetic low-entropy credential was allowed.
  Regressions cover body/header echoes, short values, URL-safe variants and benign
  encoded text. Validate shared L0 rules and all Rust/TS gates after the refactor.
- Final validation: 139 policy tests, full Rust/TS gates and release conformance
  pass. Removing encoded lookup, raw-byte matching or short-token support makes
  the corresponding response-block assertion fail; restored policy tests pass.
