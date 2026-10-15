//! L0 凭据类规则（docs/15 §1.2）：私钥、API key、高熵串、凭据文件路径。
//!
//! X-3 要求两套**独立**规则集，单一规则源就是单点失效：
//!
//! - A 组：已知前缀（`sk-`、`AKIA`、`ghp_` …）→ `api_key`。精确、可解释，
//!   但只认识列出来的格式。
//! - B 组：与前缀无关的高熵检测 → `high_entropy_secret`。长度 ≥ 32、
//!   Shannon 熵 ≥ 4.0 bit/字符、相邻字符类别切换率 ≥ 0.5。
//!   切换率是区分随机串和驼峰/蛇形标识符、路径、URL 的主要信号
//!   （标识符按单词切换，随机串几乎每个字符都切换）。
//!
//! 两组各自判定：符合前缀格式的短 key 可能只命中 A，未列出的随机串可能
//! 只命中 B；满足两组条件的 key 才有双重覆盖。

use idoris_contracts::inspect::{FindingKind, Sensitivity};

use super::Hit;

/// A 组：`(前缀, 前缀之后至少多少个 token 字符)`。
const KEY_PREFIXES: &[(&str, usize)] = &[
    ("sk-", 20),
    ("AKIA", 16),
    ("ASIA", 16),
    ("ghp_", 36),
    ("gho_", 36),
    ("ghu_", 36),
    ("ghs_", 36),
    ("ghr_", 36),
    ("github_pat_", 22),
    ("glpat-", 20),
    ("xoxb-", 10),
    ("xoxp-", 10),
    ("AIza", 35),
    ("hf_", 30),
];
/// 出现在路径 token 里就说明内容在引用凭据文件。
const CREDENTIAL_PATHS: &[&str] = &[
    ".ssh/",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    ".aws/credentials",
    ".netrc",
    ".pgpass",
    ".docker/config.json",
    ".kube/config",
    ".keychain",
    ".pem",
    ".p12",
    ".pfx",
    ".jks",
];

pub(super) fn scan(units: &[u16], hits: &mut Vec<Hit>) {
    scan_private_keys(units, hits);
    for (start, end) in runs(units, |c| {
        c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
    }) {
        if has_key_prefix(&units[start..end]) {
            hits.push(hit(FindingKind::ApiKey, start, end, Sensitivity::S3));
        }
    }
    // '=' separates an assignment name from its value; only trailing base64
    // padding belongs to the value being scored.
    for (start, run_end) in runs(units, |c| c.is_ascii_alphanumeric() || b"+/_-".contains(&c)) {
        let mut end = run_end;
        while end < units.len() && end - run_end < 2 && units[end] == u16::from(b'=') {
            end += 1;
        }
        if looks_random(&units[start..end]) {
            hits.push(hit(
                FindingKind::HighEntropySecret,
                start,
                end,
                Sensitivity::S3,
            ));
        }
    }
    for (start, end) in runs(units, |c| {
        c.is_ascii_alphanumeric() || b"._~/-\\".contains(&c)
    }) {
        // Fold separators only for matching; hit coordinates stay in the input.
        let token: Vec<u16> = units[start..end]
            .iter()
            .map(|&u| {
                if u == u16::from(b'\\') {
                    u16::from(b'/')
                } else {
                    u
                }
            })
            .collect();
        if CREDENTIAL_PATHS
            .iter()
            .any(|p| find(&token, 0, p).is_some())
            || is_env_file(&token)
        {
            hits.push(hit(
                FindingKind::CredentialPath,
                start,
                end,
                Sensitivity::S2,
            ));
        }
    }
}

fn hit(kind: FindingKind, start: usize, end: usize, sensitivity: Sensitivity) -> Hit {
    Hit {
        kind,
        start,
        length: end - start,
        sensitivity,
    }
}

fn ascii(u: u16) -> Option<u8> {
    u8::try_from(u).ok().filter(u8::is_ascii)
}

/// 满足 `member` 的最大 ASCII 连续段 `[start, end)`。
fn runs(units: &[u16], member: impl Fn(u8) -> bool) -> Vec<(usize, usize)> {
    let is_member = |u: u16| ascii(u).is_some_and(&member);
    let mut out = Vec::new();
    let mut i = 0;
    while i < units.len() {
        if !is_member(units[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < units.len() && is_member(units[i]) {
            i += 1;
        }
        out.push((start, i));
    }
    out
}

/// `pattern`（ASCII）在 `units[from..]` 里第一次出现的位置。
fn find(units: &[u16], from: usize, pattern: &str) -> Option<usize> {
    let pattern: Vec<u16> = pattern.encode_utf16().collect();
    units
        .get(from..)?
        .windows(pattern.len())
        .position(|window| window == pattern.as_slice())
        .map(|offset| from + offset)
}

/// `-----BEGIN … PRIVATE KEY…-----` 到对应的 `-----END …-----`；没有 END
/// （被截断）就一直算到输入末尾——宁可多算也不能把半截私钥当普通文本。
fn scan_private_keys(units: &[u16], hits: &mut Vec<Hit>) {
    let mut from = 0;
    while let Some(begin) = find(units, from, "-----BEGIN ") {
        let header_end = find(units, begin + 11, "-----").map_or(units.len(), |end| end + 5);
        from = header_end;
        if find(&units[begin..header_end], 0, "PRIVATE KEY").is_none() {
            continue;
        }
        let end = find(units, header_end, "-----END ")
            .and_then(|end| find(units, end + 9, "-----").map(|close| close + 5))
            .unwrap_or(units.len());
        hits.push(hit(FindingKind::PrivateKey, begin, end, Sensitivity::S3));
        from = end;
    }
}

fn has_key_prefix(token: &[u16]) -> bool {
    KEY_PREFIXES.iter().any(|&(prefix, min_body)| {
        token.len() >= prefix.len() + min_body
            && token
                .iter()
                .zip(prefix.bytes())
                .all(|(&u, b)| u == u16::from(b))
    })
}

/// B 组判定，见模块文档。
fn looks_random(token: &[u16]) -> bool {
    if token.len() < 32 {
        return false;
    }
    let class = |u: u16| match ascii(u) {
        Some(c) if c.is_ascii_lowercase() => 0usize,
        Some(c) if c.is_ascii_uppercase() => 1,
        Some(c) if c.is_ascii_digit() => 2,
        _ => 3,
    };
    let mut counts = [0u32; 128];
    for &u in token {
        counts[usize::from(u) % 128] += 1;
    }
    let switches = token
        .windows(2)
        .filter(|w| class(w[0]) != class(w[1]))
        .count();
    let n = token.len() as f64;
    let entropy: f64 = counts
        .iter()
        .filter(|&&count| count > 0)
        .map(|&count| {
            let p = f64::from(count) / n;
            -p * p.log2()
        })
        .sum();
    entropy >= 4.0 && switches * 2 >= token.len() - 1
}

/// `.env`、`.env.local` 这类文件名：前面不能紧贴标识符字符
/// （排除 `process.env.FOO`），后面只能是结尾或 `.`。
fn is_env_file(token: &[u16]) -> bool {
    let mut from = 0;
    while let Some(at) = find(token, from, ".env") {
        let before_ok = at == 0 || ascii(token[at - 1]).is_some_and(|c| c == b'/' || c == b'~');
        let after_ok = token.get(at + 4).is_none_or(|&u| u == u16::from(b'.'));
        if before_ok && after_ok {
            return true;
        }
        from = at + 1;
    }
    false
}
