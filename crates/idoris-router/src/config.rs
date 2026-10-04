use std::path::{Path, PathBuf};

/// Resolve one config path while preserving the v0.1.2 release contract:
/// an explicit non-blank path is used exactly as supplied (relative paths
/// stay cwd-relative), while a missing/blank override uses the bundled path
/// beside the running executable.
pub fn resolve_path(
    explicit: Option<&str>,
    default_relative: &str,
    executable: &Path,
) -> Result<PathBuf, String> {
    if let Some(value) = explicit.map(str::trim).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(value));
    }
    let parent = executable
        .parent()
        .ok_or_else(|| "当前可执行文件没有父目录".to_string())?;
    Ok(parent.join(default_relative))
}

/// Read a Unicode path override and resolve its bundled default.
pub fn resolve_env(name: &str, default_relative: &str) -> Result<PathBuf, String> {
    let explicit = std::env::var_os(name)
        .map(|value| {
            value
                .into_string()
                .map_err(|_| format!("环境变量 {name} 不是有效的 Unicode 路径"))
        })
        .transpose()?;
    if let Some(value) = explicit
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(PathBuf::from(value));
    }
    let executable =
        std::env::current_exe().map_err(|err| format!("无法定位当前可执行文件：{err}"))?;
    resolve_path(None, default_relative, &executable)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn explicit_relative_and_absolute_paths_are_not_reanchored() {
        let exe = Path::new("/bundle/bin/idoris");
        assert_eq!(
            resolve_path(Some(" custom/components "), "config/components", exe).unwrap(),
            PathBuf::from("custom/components")
        );
        assert_eq!(
            resolve_path(Some("/opt/idoris/policy.yaml"), "config/policy.yaml", exe).unwrap(),
            PathBuf::from("/opt/idoris/policy.yaml")
        );
    }

    #[test]
    fn missing_or_blank_override_uses_executable_parent() {
        let exe = Path::new("/bundle/bin/idoris");
        for explicit in [None, Some(""), Some("  ")] {
            assert_eq!(
                resolve_path(explicit, "config/components", exe).unwrap(),
                PathBuf::from("/bundle/bin/config/components")
            );
        }
    }
}
