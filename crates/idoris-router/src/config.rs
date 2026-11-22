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
    // `trim` 只用来判断是不是整个都是空白；非空的路径原样使用，不去掉首尾空白（#L1：路径末尾的空格是路径的一部分，
    // `setup` 输出的 `next.env.IDORIS_COMPONENTS_DIR` 原样交给 serve 就要能找到同一个目录）。
    if let Some(value) = explicit.filter(|value| !value.trim().is_empty()) {
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
    if let Some(value) = explicit.as_deref().filter(|value| !value.trim().is_empty()) {
        return Ok(PathBuf::from(value));
    }
    let executable =
        std::env::current_exe().map_err(|err| format!("无法定位当前可执行文件：{err}"))?;
    resolve_path(None, default_relative, &executable)
}

/// `trim` 只用来判断是不是整个都是空白；非空的值原样做成路径（首尾的空白是路径的一部分，#L1）。
fn non_blank_path(value: String) -> Option<PathBuf> {
    (!value.trim().is_empty()).then(|| PathBuf::from(value))
}

/// Read an optional Unicode path without inventing a bundled default.
pub fn optional_env_path(name: &str) -> Result<Option<PathBuf>, String> {
    std::env::var_os(name)
        .map(|value| {
            value
                .into_string()
                .map_err(|_| format!("环境变量 {name} 不是有效的 Unicode 路径"))
                .map(non_blank_path)
        })
        .transpose()
        .map(Option::flatten)
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
            PathBuf::from(" custom/components ")
        );
        assert_eq!(
            resolve_path(Some("/opt/idoris/policy.yaml"), "config/policy.yaml", exe).unwrap(),
            PathBuf::from("/opt/idoris/policy.yaml")
        );
    }

    /// #5：`optional_env_path` 与其他路径一样：`trim` 只用来判断是否整个空白，非空的值原样做成路径。
    /// 目录里有 `cfg` 与 `cfg `（末尾带空格）两个内容不同的文件：带空格的值必须读到带空格的那个。
    #[test]
    fn an_optional_path_keeps_its_surrounding_spaces_and_blank_means_unset() {
        let dir = std::env::temp_dir().join(format!("idoris-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cfg"), "without space").unwrap();
        std::fs::write(dir.join("cfg "), "with space").unwrap();
        let raw = format!("{}/cfg ", dir.display());
        let path = non_blank_path(raw.clone()).unwrap();
        assert_eq!(path, PathBuf::from(&raw));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "with space");
        let trimmed = non_blank_path(format!("{}/cfg", dir.display())).unwrap();
        assert_eq!(std::fs::read_to_string(trimmed).unwrap(), "without space");
        for blank in ["", "   ", "\t\n"] {
            assert_eq!(non_blank_path(blank.to_string()), None, "{blank:?}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// #L1：`trim` 只用来判断是不是整个都是空白；非空的路径原样保留，首尾的空格是路径的一部分。
    #[test]
    fn a_non_blank_path_is_kept_exactly_including_surrounding_spaces() {
        let exe = Path::new("/bundle/bin/idoris");
        for raw in [
            "/opt/my dir /",
            "/opt/trailing ",
            " /opt/leading",
            "\t/opt/tab\t",
        ] {
            assert_eq!(
                resolve_path(Some(raw), "config/components", exe).unwrap(),
                PathBuf::from(raw),
                "{raw:?}"
            );
        }
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
