//! Per-user configuration and data locations shared by the GUI and CLI.
use std::path::PathBuf;

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(windows)]
pub fn home_dir() -> Option<PathBuf> {
    env_path("USERPROFILE").or_else(|| env_path("HOME"))
}

#[cfg(not(windows))]
pub fn home_dir() -> Option<PathBuf> {
    env_path("HOME")
}

/// Expand a leading `~` and resolve relative paths against the current
/// directory.
///
/// `~`, `~/dir` and `~\dir` all work; `~other` is rejected because RTT2 has no
/// cross-platform way to look up another user's home directory. Anything else
/// is returned unchanged when it is already absolute.
pub fn resolve_user_path(input: &str) -> Result<PathBuf, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("路径不能为空".into());
    }
    let path = expand_tilde(trimmed)?;
    if path.is_absolute() {
        return Ok(path);
    }
    let cwd = std::env::current_dir().map_err(|error| format!("无法获取当前目录：{error}"))?;
    Ok(cwd.join(path))
}

fn expand_tilde(input: &str) -> Result<PathBuf, String> {
    let Some(rest) = input.strip_prefix('~') else {
        return Ok(PathBuf::from(input));
    };
    if !(rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\')) {
        return Err(format!(
            "不支持 {input}：RTT2 只能展开当前用户的 ~，请改用 ~/目录 或绝对路径"
        ));
    }
    let home = home_dir().ok_or_else(|| "无法确定用户主目录，不能展开 ~".to_owned())?;
    Ok(home.join(rest.trim_start_matches(['/', '\\'])))
}

/// Per-user configuration directory for the platform.
pub fn config_dir() -> PathBuf {
    #[cfg(windows)]
    let dir = env_path("LOCALAPPDATA")
        .or_else(home_dir)
        .unwrap_or_else(std::env::temp_dir);
    #[cfg(target_os = "macos")]
    let dir = home_dir()
        .map(|home| home.join("Library").join("Application Support"))
        .unwrap_or_else(std::env::temp_dir);
    #[cfg(all(unix, not(target_os = "macos")))]
    let dir = env_path("XDG_CONFIG_HOME")
        .filter(|path| path.is_absolute())
        .or_else(|| home_dir().map(|home| home.join(".config")))
        .unwrap_or_else(std::env::temp_dir);
    #[cfg(not(any(windows, unix)))]
    let dir = std::env::temp_dir();
    dir
}

/// Directory for machine-local application state such as project history.
pub fn data_dir() -> PathBuf {
    #[cfg(windows)]
    let dir = env_path("LOCALAPPDATA")
        .or_else(home_dir)
        .unwrap_or_else(std::env::temp_dir);
    #[cfg(target_os = "macos")]
    let dir = home_dir()
        .map(|home| home.join("Library").join("Application Support"))
        .unwrap_or_else(std::env::temp_dir);
    #[cfg(all(unix, not(target_os = "macos")))]
    let dir = env_path("XDG_DATA_HOME")
        .filter(|path| path.is_absolute())
        .or_else(|| home_dir().map(|home| home.join(".local").join("share")))
        .unwrap_or_else(std::env::temp_dir);
    #[cfg(not(any(windows, unix)))]
    let dir = std::env::temp_dir();
    dir
}

pub fn app_config_dir() -> PathBuf {
    config_dir().join("RTT2")
}

pub fn app_data_dir() -> PathBuf {
    data_dir().join("RTT2")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_and_relative_paths_resolve_against_home_and_cwd() {
        let home = home_dir().expect("tests need HOME or USERPROFILE");
        assert_eq!(resolve_user_path("~").unwrap(), home);
        assert_eq!(resolve_user_path(" ~/a/b ").unwrap(), home.join("a/b"));
        assert_eq!(resolve_user_path("~\\a\\b").unwrap(), home.join("a\\b"));

        let cwd = std::env::current_dir().unwrap();
        assert_eq!(resolve_user_path("fw/x.elf").unwrap(), cwd.join("fw/x.elf"));

        let absolute = std::env::temp_dir().join("rtt2-path-test.elf");
        let absolute = absolute.display().to_string();
        assert_eq!(
            resolve_user_path(&absolute).unwrap(),
            PathBuf::from(&absolute)
        );

        assert!(resolve_user_path("").is_err());
        assert!(resolve_user_path("   ").is_err());
        assert!(resolve_user_path("~someone/x").is_err());
    }

    #[test]
    fn app_directories_are_absolute_or_fall_back_to_temp() {
        for directory in [app_config_dir(), app_data_dir()] {
            assert!(directory.is_absolute(), "{}", directory.display());
            assert!(
                directory.file_name().is_some_and(|name| name == "RTT2"),
                "{}",
                directory.display()
            );
        }
    }
}
