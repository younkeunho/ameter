//! Wires `ameter-cli claude-statusline` into Claude Code's settings.json and back out.
//! The replaced statusline keeps running behind ameter (passthrough) and is restored on uninstall.

use crate::paths;
use crate::util;
use serde_json::{Map, Value};
use std::fs;
use std::path::Path;

const MARKER: &str = "ameter-cli";

/// Claude Code runs statusLine commands through Git Bash on Windows: forward slashes, and
/// quotes only when the path has spaces.
pub fn bridge_command(cli: &Path) -> String {
    let path = cli.to_string_lossy().replace('\\', "/");
    let path = if path.contains(' ') { format!("\"{path}\"") } else { path };
    format!("{path} claude-statusline")
}

fn is_ours(command: Option<&str>) -> bool {
    command.is_some_and(|c| c.to_ascii_lowercase().contains(MARKER))
}

fn status_command(root: &Map<String, Value>) -> Option<&str> {
    root.get("statusLine")?.get("command")?.as_str()
}

pub fn is_installed() -> bool {
    util::read_json_object(&paths::claude_settings()).is_some_and(|root| is_ours(status_command(&root)))
}

/// Refuses to touch a settings file it cannot parse.
fn read_settings(path: &Path) -> Result<Map<String, Value>, String> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let text = util::read_text(path).ok_or_else(|| format!("{} 를 읽을 수 없어 아무것도 바꾸지 않았습니다.", path.display()))?;
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(format!("{} 가 올바른 JSON이 아니어서 아무것도 바꾸지 않았습니다.", path.display())),
    }
}

fn backup(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let (y, mo, d, h, mi, s, _) = util::local_time();
    let dir = paths::backup_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let target = dir.join(format!("claude-settings.{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}.json"));
    fs::copy(path, target).map(|_| ()).map_err(|e| format!("설정 백업 실패: {e}"))
}

fn write_settings(path: &Path, root: Map<String, Value>) -> Result<(), String> {
    backup(path)?;
    util::write_json(path, &Value::Object(root)).map_err(|e| format!("설정 저장 실패: {e}"))
}

/// Points statusLine at `cli`. `passthrough` replaces the command that keeps running behind
/// ameter (e.g. to unwrap another wrapper); by default it is the statusline being replaced.
pub fn install(cli: &Path, passthrough: Option<&str>) -> Result<String, String> {
    if !cli.is_file() {
        return Err(format!("ameter-cli 실행 파일이 없습니다: {}", cli.display()));
    }
    let settings_path = paths::claude_settings();
    let mut root = read_settings(&settings_path)?;
    let existing = root.get("statusLine").and_then(Value::as_object).cloned();
    let already_ours = is_ours(existing.as_ref().and_then(|s| s.get("command")).and_then(Value::as_str));

    let mut original = if already_ours { util::read_json_object(&paths::original_status_line_file()) } else { existing.clone() };
    if let Some(command) = passthrough {
        let entry = original.get_or_insert_with(Map::new);
        entry.insert("type".into(), "command".into());
        entry.insert("command".into(), command.into());
    }

    if !already_ours || passthrough.is_some() {
        let saved = original.clone().map(Value::Object).unwrap_or(Value::Null);
        util::write_json(&paths::original_status_line_file(), &saved).map_err(|e| e.to_string())?;
        let command = original.as_ref().and_then(|o| o.get("command")).and_then(Value::as_str).map(str::trim).filter(|c| !c.is_empty());
        match command {
            Some(c) => util::write_atomic(&paths::passthrough_file(), c.as_bytes()).map_err(|e| e.to_string())?,
            None => util::remove_file_if_exists(&paths::passthrough_file()).map_err(|e| e.to_string())?,
        }
    }

    let mut status_line = existing.unwrap_or_default();
    status_line.insert("type".into(), "command".into());
    status_line.insert("command".into(), bridge_command(cli).into());
    root.insert("statusLine".into(), Value::Object(status_line));
    write_settings(&settings_path, root)?;

    let wrapped = original.as_ref().and_then(|o| o.get("command")).and_then(Value::as_str).filter(|c| !c.trim().is_empty());
    Ok(match wrapped {
        Some(c) => format!("Claude Code 상태줄 연결 완료 (기존 상태줄 유지: {c})"),
        None => "Claude Code 상태줄 연결 완료".into(),
    })
}

pub fn uninstall() -> Result<String, String> {
    let settings_path = paths::claude_settings();
    if !settings_path.exists() {
        return Ok("Claude 설정 파일이 없습니다.".into());
    }
    let mut root = read_settings(&settings_path)?;
    if !is_ours(status_command(&root)) {
        return Ok("ameter 상태줄이 연결되어 있지 않습니다.".into());
    }
    let original = util::read_json_object(&paths::original_status_line_file());
    let message = match &original {
        Some(o) => format!("연결 해제, 기존 상태줄 복원: {}", o.get("command").and_then(Value::as_str).unwrap_or("")),
        None => "연결 해제 (상태줄 제거)".into(),
    };
    match original {
        Some(o) => root.insert("statusLine".into(), Value::Object(o)),
        None => root.shift_remove("statusLine"),
    };
    write_settings(&settings_path, root)?;
    let _ = util::remove_file_if_exists(&paths::passthrough_file());
    let _ = util::remove_file_if_exists(&paths::original_status_line_file());
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_command_uses_forward_slashes_and_quotes_spaces() {
        assert_eq!(bridge_command(Path::new("C:\\Tools\\ameter-cli.exe")), "C:/Tools/ameter-cli.exe claude-statusline");
        assert_eq!(
            bridge_command(Path::new("C:\\Program Files\\ameter\\ameter-cli.exe")),
            "\"C:/Program Files/ameter/ameter-cli.exe\" claude-statusline"
        );
    }

    #[test]
    fn recognizes_its_own_command() {
        assert!(is_ours(Some("C:/x/ameter-cli.exe claude-statusline")));
        assert!(!is_ours(Some("bash ~/.claude/statusline.sh")));
        assert!(!is_ours(None));
    }
}
