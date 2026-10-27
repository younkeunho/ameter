//! `ameter-cli claude-statusline`: Claude Code's statusLine command. Caches the session's
//! rate limits, then runs the statusline it replaced and relays that output unchanged.
//! Contract with Claude Code: it always exits 0 and never logs prompt content.

use crate::fmt;
use crate::model::{self, CLAUDE_WINDOWS};
use crate::paths;
use crate::process;
use crate::util::{self, JsonExt};
use serde_json::{Map, Value};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

const GUARD: &str = "AMETER_STATUSLINE_WRAPPED";
const COMMAND: &str = "AMETER_PASSTHROUGH";
const PASSTHROUGH_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run() {
    let mut input = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut input);
    if let Ok(Value::Object(json)) = serde_json::from_slice::<Value>(&input) {
        cache_rate_limits(&json, util::now_ms());
    }

    // Started by our own passthrough (a wrapped command calling ameter again): stop here.
    if std::env::var_os(GUARD).is_some_and(|v| v == "1") {
        return;
    }

    let mut stdout = std::io::stdout().lock();
    let passthrough = util::read_text(&paths::passthrough_file()).map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    match passthrough {
        Some(command) => {
            if let Some(output) = run_passthrough(&command, &input) {
                let _ = stdout.write_all(&output);
            }
        }
        None => {
            let now = util::now_sec();
            let lines = [
                fmt::terminal_line("CO", &model::load_codex(), now, true),
                fmt::terminal_line("HF", &model::load_higgsfield(), now, true),
                fmt::terminal_line("CC", &model::load_claude(), now, true),
            ];
            let _ = stdout.write_all(format!("{}\n", lines.join("\n")).as_bytes());
        }
    }
    let _ = stdout.flush();
}

/// `rate_limits` is absent until the session's first API response; then it is merged in.
pub fn cache_rate_limits(input: &Map<String, Value>, now_ms: i64) {
    let Some(rate_limits) = input.obj("rate_limits") else { return };
    let mut observed = Map::new();
    for (key, _) in CLAUDE_WINDOWS {
        let Some(window) = rate_limits.obj(key) else { continue };
        if let Some(entry) = model::claude_entry(window.f64_of("used_percentage"), window.get("resets_at")) {
            observed.insert(key.into(), entry);
        }
    }
    model::merge_claude(&observed, false, model::SOURCE_STATUSLINE, now_ms);
}

/// Runs the replaced statusline the way Claude Code does on Windows: Git Bash when installed,
/// PowerShell otherwise. The command travels in an environment variable, so user content is
/// never re-quoted on a command line.
fn run_passthrough(command: &str, input: &[u8]) -> Option<Vec<u8>> {
    let (shell, args): (PathBuf, Vec<String>) = match find_git_bash() {
        Some(bash) => (bash, vec!["-c".into(), format!("eval \"${COMMAND}\"")]),
        None => (
            PathBuf::from("powershell.exe"),
            ["-NoProfile", "-NonInteractive", "-Command", &format!("Invoke-Expression $env:{COMMAND}")].map(String::from).to_vec(),
        ),
    };
    let mut cmd = process::command(&shell, &std::env::current_dir().unwrap_or_else(|_| paths::data_dir()));
    cmd.args(&args).env(GUARD, "1").env(COMMAND, command);
    match process::run(cmd, input, PASSTHROUGH_TIMEOUT) {
        Ok(output) => {
            let _ = std::io::stderr().write_all(&output.stderr);
            Some(output.stdout)
        }
        Err(e) => {
            util::log(&format!("passthrough failed: {e}"));
            None
        }
    }
}

/// Git for Windows' bash.exe, never the WSL `bash.exe` stub that may come first on PATH.
pub fn find_git_bash() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("CLAUDE_CODE_GIT_BASH_PATH").map(PathBuf::from)
        && configured.is_file()
    {
        return Some(configured);
    }
    for dir in paths::path_dirs() {
        if !dir.join("git.exe").is_file() {
            continue;
        }
        let mut root = Some(dir.as_path());
        for _ in 0..3 {
            let Some(r) = root else { break };
            let candidate = r.join("bin").join("bash.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
            root = r.parent();
        }
    }
    let program_dirs = ["ProgramFiles", "ProgramFiles(x86)"].iter().filter_map(std::env::var_os).map(|d| PathBuf::from(d).join("Git"));
    let user_dir = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Programs").join("Git"));
    program_dirs.chain(user_dir).map(|d| d.join("bin").join("bash.exe")).find(|p| p.is_file())
}
