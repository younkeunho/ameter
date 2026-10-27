//! Where ameter keeps its files. `AMETER_DATA_DIR` and `CLAUDE_CONFIG_DIR` override the defaults.

use std::env;
use std::path::PathBuf;

fn env_dir(name: &str) -> Option<PathBuf> {
    env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn local_app_data() -> PathBuf {
    env_dir("LOCALAPPDATA").unwrap_or_else(|| home().join("AppData").join("Local"))
}

pub fn app_data() -> PathBuf {
    env_dir("APPDATA").unwrap_or_else(|| home().join("AppData").join("Roaming"))
}

pub fn home() -> PathBuf {
    env_dir("USERPROFILE").unwrap_or_else(|| PathBuf::from("C:\\"))
}

/// Caches, settings, logs and backups.
pub fn data_dir() -> PathBuf {
    env_dir("AMETER_DATA_DIR").unwrap_or_else(|| local_app_data().join("ameter"))
}

pub fn claude_cache() -> PathBuf {
    data_dir().join("claude.json")
}

pub fn codex_cache() -> PathBuf {
    data_dir().join("codex.json")
}

pub fn higgsfield_cache() -> PathBuf {
    data_dir().join("higgsfield.json")
}

pub fn display_settings() -> PathBuf {
    data_dir().join("display.json")
}

/// The statusline command ameter runs after caching (the one it replaced in settings.json).
pub fn passthrough_file() -> PathBuf {
    data_dir().join("statusline-passthrough.txt")
}

/// The statusLine object ameter replaced, restored by `uninstall-claude`.
pub fn original_status_line_file() -> PathBuf {
    data_dir().join("statusline-original.json")
}

pub fn backup_dir() -> PathBuf {
    data_dir().join("backups")
}

pub fn log_file(name: &str) -> PathBuf {
    data_dir().join(format!("{name}.log"))
}

pub fn claude_settings() -> PathBuf {
    env_dir("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home().join(".claude")).join("settings.json")
}

/// Directory of the running executable; the widget and the CLI are installed side by side.
pub fn exe_dir() -> PathBuf {
    env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from)).unwrap_or_else(|| PathBuf::from("."))
}

pub fn widget_exe() -> PathBuf {
    exe_dir().join("ameter.exe")
}

pub fn cli_exe() -> PathBuf {
    exe_dir().join("ameter-cli.exe")
}

/// PATH entries, trimmed, unquoted and de-duplicated, in order.
pub fn path_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
            let text = dir.to_string_lossy().trim().trim_matches('"').to_string();
            if text.is_empty() {
                continue;
            }
            let dir = PathBuf::from(text);
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    dirs
}
