# Storing secrets locally and brokering them to an AI agent on macOS

**Verified 2026-09-19.** ✅ = quote read on a fetched primary source · ⚠️ = secondary/OSS doc · ❓ = unverified. Versions from registries, not memory.

## Headline recommendation

Run a **separate-UID broker in a *user* login context (LaunchAgent), not a root LaunchDaemon.** Store secrets in the **data protection keychain**, wrapped by an **unextractable Secure Enclave P-256 key with no `userPresence` flag**, so the broker unwraps at startup with **no prompt** and no master key on disk. The router never receives a value: it holds `secret://` handles, sends them as **placeholders** to a **loopback HTTPS injection proxy** that matches host+method+path, evaluates policy **before** decrypting, and substitutes the real value on the upstream socket. Audit a **keyed HMAC fingerprint**, never the value, and fail closed if the audit sink is unwritable.

One constraint drives everything: ✅ **Apple TN3137** — *"Programs that run outside of a user context, like a `launchd` daemon, must target the file-based keychain. The data protection keychain is only available to programs running in a user context."* Biometrics, Secure Enclave protection, and access groups all need the data protection keychain, so they are **mutually exclusive with a system LaunchDaemon.**

## 1. macOS-native storage

| Mechanism | Gives you | Hard limit |
|---|---|---|
| **File-based keychain** | Only option for a root LaunchDaemon ✅ | ACL (`SecAccess`) model; ✅ Apple: on macOS *"Data Protection isn't used directly to enforce these guarantees"* — `kSecAttrAccessible` is inert |
| **Data protection keychain** (10.15+) | Accessibility classes, access groups, biometrics, Secure Enclave ✅ | ✅ **User context only**; access groups need **code-signing entitlements + an Apple team prefix + a provisioning profile in an app-like bundle** ✅ |
| **Secure Enclave** | ✅ Private key never in memory; P-256; sign + ECDH only; **cannot import existing keys** | ✅ Apple silicon / T1+ only; SE keys require the data protection keychain |
| **`LAContext` biometric gating** | ✅ `kSecAccessControlBiometryCurrentSet` invalidates on enrolment change | ✅ Needs a UI session. Headless must set `interactionNotAllowed = true` / `kSecUseAuthenticationUIFail` → `errSecInteractionNotAllowed`. **Biometrics and unattended operation are mutually exclusive.** |

✅ **CVE-2025-24204** (⚠️ secondary) — `/usr/bin/gcore` held `com.apple.system-task-ports.read` in macOS 15.0, permitting any-process memory dumps **with SIP on**; the researcher recovered the **login-keychain master key** from `securityd`. Fixed 15.3.

## 2. Cross-language libraries

| Tool | Version | Daemon-viable? | Note |
|---|---|---|---|
| Rust `keyring` | ✅ **4.2.0** (2026-08-29) | ⚠️ partial | v4 rewrite: API in `keyring-core`, backend in `apple-native-keyring-store` (`keychain` or `protected`) |
| Rust `security-framework` | ✅ **3.7.0** (2026-02-20) | ✅ FFI only | Exposes `SecItem*`, `SecAccessControl`, `Token::SecureEnclave` — **the right layer for the broker** |
| Node `keytar` | ✅ 7.9.0 (**2022-02-17**) | ❌ | ✅ Repo is **"Public archive"**; dead |
| Node `@napi-rs/keyring` | ✅ **2.1.0** (2026-09-13) | ⚠️ | N-API over keyring-rs |
| 1Password `op` | ✅ 2.39.0 (date ❓) | ✅ **service-account token** | Biometric path needs the desktop app ⇒ GUI session |
| Bitwarden `bws` | ✅ `@bitwarden/cli` **2026.9.0** | ✅ **`BWS_ACCESS_TOKEN`** | `bw` PM CLI is ❌ after reboot (needs master password) |
| `age` | ✅ **v1.3.2** (2026-08-29) | ⚠️ key file | ✅ *"age does not have a global keyring"* — not a broker |
| `sops` | ✅ **v3.13.3** (2026-07-23) | ⚠️ key file/KMS | **No macOS Keychain backend**; the KEK lands in env/file |
| HashiCorp Vault | ✅ **v2.1.1** (2026-09-17) | ✅ Agent + non-interactive auth | ✅ `vault server -dev` is in-memory, *"never… in production"* |
| `gopass` / `pass` | ✅ v1.17.2 / 1.7.4 | ❌ | ✅ gpg-agent cache empty after reboot → pinentry |

**Non-interactive after reboot:** `bws`, `op` service accounts, Vault Agent, or a root daemon reading the file-based System keychain.

## 3. The brokering pattern

Reference: ✅ **`keys-on-the-wire`** (PyPI **v1.1.1**, 2026-08-28) — a loopback mitmproxy broker; the agent holds placeholders, the proxy verifies host/method/path binding, fetches from Bitwarden Secrets Manager, substitutes on the upstream socket. Copy its two guarantees: **G5 enforcement-by-omission** (a non-permitted destination receives the placeholder verbatim, *not* an error — a 5xx would make the proxy a probe oracle) and **G6** (the audit record is `fsync`ed before modified bytes reach the upstream socket).

✅ **`onecli`** is real (`onecli.sh`, Rust gateway, MITM HTTPS): per-agent **grants**, and the key ordering — *"Policy is evaluated before credential injection, so a blocked request never decrypts or touches your secrets."* Secrets are AES-256-GCM at rest, matched by **host + path**.

✅ **Envoy `credential_injector`** documents a trap: `overwrite` **defaults to `false`**, so a caller-supplied `Authorization` header **wins**, bypassing the injected credential. Envoy calls it *"functional but has not had substantial production burn time… unknown security posture… only used where both the downstream and upstream are trusted."* ⚠️ Premise correction: **mitmproxy ships no credential-injection addon** — none of its ~30 official examples swaps a placeholder for a real secret.

Short-lived credentials: ✅ IMDSv2 issues a TTL-bounded token the caller must present; ✅ RFC 8693 and SPIFFE JWT-SVID formalise downscoping (⚠️ JWT-SVIDs are bearer tokens, *"susceptible to replay attacks"*). ✅ `gitcredentials(7)` isolates the **repo**, not the process. ✅ **`aws-vault` is abandoned** (README banner → ByteNess fork).

## 4. Secret detection at egress

| Tool | Version | Approach | Embeddable | Caveat |
|---|---|---|---|---|
| `gitleaks` | ✅ **v8.30.1** (2026-03-21) | regex + entropy + allowlists | ✅ Go `DetectBytes()` | ✅ *"feature complete… security patches only"*; author moved to `betterleaks`; FP issue #1830 |
| `trufflehog` | ✅ **v3.97.5** (2026-09-16) | regex + **live API verification**, 904 detectors | ❌ unstable APIs; **AGPL-3.0** | ✅ Inline verification **sends the secret to a third party**; network-bound, not ms-scale |
| `detect-secrets` | ✅ v1.5.0 (**2024-05-06**) | plugin/entropy, baseline-oriented | ✅ Python | ✅ *"Only proper developer education can truly [prevent leaks]"*; misses multi-line secrets |
| `@visulima/secret-scanner` | ✅ v2.0.1 (2026-08-19) | Rust→NAPI, 1,058 rules | ✅ Node | Only concrete latency found: **~11 ms / 550 KB** |
| `ggshield` | ✅ v1.54.0 (2026-08-26) | cloud API | CLI | Only mainstream **runtime-blocking** scanner (Claude Code, Cursor, Copilot) but egresses to GitGuardian |

**No vendor-agnostic FP/FN benchmark exists** — stated rather than invented. ISSTA 2026 (arXiv:2608.04523): *"secrets often lack identifiable patterns, resulting in poor precision and recall."* A random 20-char password is invisible to regex **and** entropy. Treat egress scanning as **redact-by-default + allowlist**, never completeness.
## 5. Audit without leaking

✅ Vault's model is the standard: *"By default, Vault only writes a keyed hash (HMAC-SHA256) of most string values"*. Two rules follow: the logging layer must **never receive the value** (do not log-then-redact), and log the secret **identifier + version**, not the payload. ✅ Vault also **refuses to service a request it cannot audit** — fail closed.

Redaction is a best-effort deny-list, and the failures are documented: ✅ HCSEC-2025-09/CVE-2025-4166 logged KV v2 secret **values** in an error path; ✅ HCSEC-2024-18/CVE-2024-8365 — HMAC'ing of sensitive headers *"was removed"*; ✅ CVE-2024-0831 — `log_raw=true` leaked globally to **all** devices. ✅ CWE-532: *"Do not write secrets into the log files."* ✅ `curl --trace` and libcurl warn that credentials persist in memory and in *"freed data"*.

macOS: ✅ `os_log` redacts interpolated dynamic strings by default — but Apple's Device Management schema exposes `SystemLogging.System → Enable-Private-Data`, which *"enables private data logging for the entire system."* **Never log a secret to `os_log`, even as `private`.** Apple's own safe pattern is `OSLogPrivacy.Mask.hash`, which *"doesn't provide any identifying information"* yet lets you *"correlate log messages."*

## 6. Threat model

**Protects against:** ✅ the model, prompt, context cache, and KV cache containing a value — structurally, not by discipline. ✅ prompt injection inducing exfiltration (the agent has nothing to leak). ✅ secrets in logs/crash reports, if audit is fingerprint-only. ✅ upstream echo — *if* you also filter the **response** path (`keys-on-the-wire` R6). ✅ laundering through a legitimately-bound host, via per-binding method/path scope.

**Does not protect against:** ✅ **a same-machine attacker with a suitable entitlement** — CVE-2025-24204 proved any-process memory reads are possible; the broker's plaintext *is* reachable, so design for a separate UID, not in-process isolation. ✅ a same-UID attacker using the proxy as its own authenticated channel (T-3). ✅ proxy compromise (**R1 [HIGH]**: supply-chain to the proxy UID = all secrets). ✅ DNS exfiltration — an HTTP proxy cannot see it. ✅ a secret with no recognisable format; ❓ nothing distinguishes the user's own secret from a lookalike string. ✅ root. ✅ a malicious upstream abusing a credential it legitimately received — trust, not mechanism.

## Strongest weakness of the recommendation

**The broker is a single point of total compromise, and macOS gives it a weaker isolation floor than Linux.** No systemd-style sandbox exists to lean on; the broker's plaintext sits in memory, and CVE-2025-24204 shows that boundary breaks under one mis-granted Apple entitlement. Separate UIDs raise the bar from "compromise the process" to "obtain another UID" — they do **not** make it absolute, and claiming the secret "never leaves the broker" is overclaiming.

## Sources

Apple: [TN3137 on Mac keychains](https://developer.apple.com/documentation/technotes/tn3137-on-mac-keychains) · [kSecUseDataProtectionKeychain](https://developer.apple.com/documentation/security/ksecusedataprotectionkeychain) · [kSecAttrAccessible](https://developer.apple.com/documentation/security/ksecattraccessible) · [biometryCurrentSet](https://developer.apple.com/documentation/security/secaccesscontrolcreateflags/biometrycurrentset) · [Restricting keychain item accessibility](https://developer.apple.com/documentation/security/restricting-keychain-item-accessibility) · [Sharing access to keychain items](https://developer.apple.com/documentation/security/sharing-access-to-keychain-items-among-a-collection-of-apps) · [Access Control Lists](https://developer.apple.com/documentation/security/access-control-lists) · [Protecting keys with the Secure Enclave](https://developer.apple.com/documentation/security/protecting-keys-with-the-secure-enclave) · [LAContext](https://developer.apple.com/documentation/localauthentication/lacontext) · [kSecUseAuthenticationUIFail](https://developer.apple.com/documentation/security/ksecuseauthenticationuifail) · [Logger / privacy](https://developer.apple.com/documentation/os/logger) · [os_log(3)](https://keith.github.io/xcode-man-pages/os_log_error.3.html) · [Apple Platform Security Guide, Aug 2026](https://help.apple.com/pdf/security/en_US/apple-platform-security-guide.pdf) · [CVE-2025-24204](https://www.helpnetsecurity.com/2025/09/04/macos-gcore-vulnerability-cve-2025-24204/)

Brokering: [keys-on-the-wire architecture](https://raw.githubusercontent.com/inflightsec/keys-on-the-wire/refs/heads/main/docs/architecture.md) · [onecli](https://onecli.sh/docs/how-it-works) · [Envoy credential_injector](https://www.envoyproxy.io/docs/envoy/v1.37.3/configuration/http/http_filters/credential_injector_filter) · [Envoy ext_authz](https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/ext_authz_filter) · [mitmproxy addon examples](https://docs.mitmproxy.org/stable/addons/examples/) · [aws-vault](https://github.com/99designs/aws-vault) · [gitcredentials(7)](https://git-scm.com/docs/gitcredentials) · [IMDSv2](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/configuring-instance-metadata-service.html) · [RFC 8693](https://www.rfc-editor.org/rfc/rfc8693.html) · [RFC 8252](https://www.rfc-editor.org/rfc/rfc8252.txt) · [JWT-SVID](https://raw.githubusercontent.com/spiffe/spiffe/main/standards/JWT-SVID.md)

Egress: [gitleaks v8.30.1](https://github.com/gitleaks/gitleaks/releases/tag/v8.30.1) · [issue #1830](https://github.com/gitleaks/gitleaks/issues/1830) · [trufflehog v3.97.5](https://github.com/trufflesecurity/trufflehog/releases/tag/v3.97.5) · [detect-secrets](https://raw.githubusercontent.com/Yelp/detect-secrets/master/README.md) · [ggshield for AI coding tools](https://docs.gitguardian.com/ggshield-docs/integrations/ai-coding-tools/secret-scanning-for-ai-coding-tools) · [Presidio](https://presidio.dataprivacystack.org/) · [arXiv:2608.04523](https://arxiv.org/abs/2608.04523) · [git-secrets](https://raw.githubusercontent.com/awslabs/git-secrets/master/README.rst) · [@visulima/secret-scanner](https://registry.npmjs.org/@visulima/secret-scanner)

Audit: [Vault audit devices](https://developer.hashicorp.com/vault/docs/audit) · [best practices](https://developer.hashicorp.com/vault/docs/audit/best-practices) · [schema](https://developer.hashicorp.com/vault/docs/audit/schema) · [HCSEC-2024-01](https://discuss.hashicorp.com/t/hcsec-2024-01-vault-audit-log-leak/62311) · [1Password events API](https://www.1password.dev/events-api/audit-events) · [item usage](https://www.1password.dev/events-api/item-usage-actions) · [service accounts](https://www.1password.dev/service-accounts/security) · [OWASP Logging Cheat Sheet](https://cheatsheetseries.owasp.org/cheatsheets/Logging_Cheat_Sheet.html) · [CWE-532](https://cwe.mitre.org/data/definitions/532.html) · [curl manpage](https://curl.se/docs/manpage.html) · [CloudTrail log integrity](https://docs.aws.amazon.com/awscloudtrail/latest/userguide/cloudtrail-log-file-validation-intro.html)

Libraries: [crates.io/keyring](https://crates.io/crates/keyring) · [apple-native-keyring-store](https://docs.rs/apple-native-keyring-store/latest/apple_native_keyring_store/) · [security-framework](https://crates.io/crates/security-framework) · [keytar (archived)](https://github.com/atom/node-keytar) · [1Password CLI run](https://www.1password.dev/cli/reference/commands/run.md) · [Bitwarden machine accounts](https://bitwarden.com/help/machine-accounts.md) · [age](https://filippo.io/age) · [sops v3.13.3](https://github.com/getsops/sops/releases/tag/v3.13.3) · [Vault Agent](https://developer.hashicorp.com/vault/docs/agent-and-proxy/agent) · [gopass](https://github.com/gopasspw/gopass/releases.atom) · [pass](https://www.passwordstore.org/)
