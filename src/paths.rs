//! Per-user configuration and data locations shared by the GUI and CLI.
use std::path::PathBuf;

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(windows)]
fn home_dir() -> Option<PathBuf> {
    env_path("USERPROFILE").or_else(|| env_path("HOME"))
}

#[cfg(not(windows))]
fn home_dir() -> Option<PathBuf> {
    env_path("HOME")
}

/// Per-user configuration directory for the platform.
pub fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        return env_path("LOCALAPPDATA")
            .or_else(home_dir)
            .unwrap_or_else(std::env::temp_dir);
    }
    #[cfg(target_os = "macos")]
    {
        return home_dir()
            .map(|home| home.join("Library").join("Application Support"))
            .unwrap_or_else(std::env::temp_dir);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return env_path("XDG_CONFIG_HOME")
            .filter(|path| path.is_absolute())
            .or_else(|| home_dir().map(|home| home.join(".config")))
            .unwrap_or_else(std::env::temp_dir);
    }
    #[cfg(not(any(windows, unix)))]
    {
        std::env::temp_dir()
    }
}

/// Directory for machine-local application state such as project history.
pub fn data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        return env_path("LOCALAPPDATA")
            .or_else(home_dir)
            .unwrap_or_else(std::env::temp_dir);
    }
    #[cfg(target_os = "macos")]
    {
        return home_dir()
            .map(|home| home.join("Library").join("Application Support"))
            .unwrap_or_else(std::env::temp_dir);
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return env_path("XDG_DATA_HOME")
            .filter(|path| path.is_absolute())
            .or_else(|| home_dir().map(|home| home.join(".local").join("share")))
            .unwrap_or_else(std::env::temp_dir);
    }
    #[cfg(not(any(windows, unix)))]
    {
        std::env::temp_dir()
    }
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
