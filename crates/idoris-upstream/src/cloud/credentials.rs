//! 云上游的密钥来源。环境变量那一种沿用既有的 [`EnvCredentialSource`](crate::remote::EnvCredentialSource)；
//! 这里补一个**从文件读**的（systemd / 容器 secret 一类：文件权限由部署方设成只有服务账号能读）。
//!
//! 密钥**不进日志、不进错误信息、不进 Debug 输出**：本模块的类型只在 Debug 里写文件路径（路径不是密钥），
//! 读取失败一律返回固定的 [`UpstreamError::AuthFailed`]，不带文件内容也不带 IO 错误文本。

use std::path::PathBuf;

use async_trait::async_trait;

use crate::error::UpstreamError;
use crate::remote::CredentialSource;

/// 从文件读取密钥。每次调用都重新读（轮换密钥不用重启）；去掉首尾空白；必须是单行的可见 ASCII
/// （否则它进不了 `Authorization` 头，也防止头注入）。
#[derive(Debug, Clone)]
pub struct FileCredentialSource {
    path: PathBuf,
}

impl FileCredentialSource {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

/// 密钥格式检查：非空、可见 ASCII、没有空白与控制字符。
pub fn is_well_formed_key(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|b| b.is_ascii_graphic())
}

#[async_trait]
impl CredentialSource for FileCredentialSource {
    async fn api_key(&self, _provider: &str) -> Result<String, UpstreamError> {
        let text = std::fs::read_to_string(&self.path).map_err(|_| UpstreamError::auth_failed())?;
        let key = text.trim();
        if is_well_formed_key(key) {
            Ok(key.to_string())
        } else {
            Err(UpstreamError::auth_failed())
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::io::Write;

    use super::*;

    fn key_file(contents: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    #[tokio::test]
    async fn reads_and_trims_a_key_and_rereads_on_every_call() {
        let file = key_file("sk-test-abc123\n");
        let source = FileCredentialSource::new(file.path());
        assert_eq!(source.api_key("p").await.unwrap(), "sk-test-abc123");
        std::fs::write(file.path(), "  sk-rotated  \r\n").unwrap();
        assert_eq!(source.api_key("p").await.unwrap(), "sk-rotated");
    }

    #[tokio::test]
    async fn a_missing_empty_or_malformed_file_is_auth_failed_without_any_detail() {
        let missing = FileCredentialSource::new("/nonexistent/idoris-cloud-key");
        let err = missing.api_key("p").await.unwrap_err();
        assert!(err.is_auth_failed());
        assert!(!err.to_string().contains("nonexistent"));
        for bad in ["", "   \n", "two words", "line1\nline2", "tab\tkey", "ключ"] {
            let file = key_file(bad);
            let err = FileCredentialSource::new(file.path())
                .api_key("p")
                .await
                .unwrap_err();
            assert!(err.is_auth_failed(), "{bad:?}");
            assert!(!err.to_string().contains("ключ"));
        }
    }

    #[test]
    fn key_format_rules() {
        assert!(is_well_formed_key("sk-ABC_123.xyz~+/="));
        assert!(!is_well_formed_key(""));
        assert!(!is_well_formed_key("a b"));
        assert!(!is_well_formed_key("a\nb"));
        assert!(!is_well_formed_key("é"));
    }
}
