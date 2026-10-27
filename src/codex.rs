//! Codex usage from the official `codex app-server` (newline-delimited JSON-RPC over stdio).
//! A short-lived server per poll: nothing stays running between polls, so `codex.exe` is
//! never locked against npm updates.

use crate::model::{self, ProviderState, UsageWindow};
use crate::paths;
use crate::poller::Source;
use crate::process::{self, KillOnDropJob};
use crate::util::{self, JsonExt};
use serde_json::{Map, Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

/// The native codex.exe. npm installs expose only a node shim (`codex`, `codex.cmd`) that
/// CreateProcess cannot start, so the platform binary behind it is resolved.
pub fn find_exe() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("AMETER_CODEX_PATH").map(PathBuf::from) {
        return configured.is_file().then_some(configured);
    }
    let mut dirs = paths::path_dirs();
    let npm = paths::app_data().join("npm");
    if !dirs.contains(&npm) {
        dirs.push(npm);
    }
    for dir in dirs {
        let exe = dir.join("codex.exe");
        if exe.is_file() {
            return Some(exe);
        }
        if (dir.join("codex.cmd").is_file() || dir.join("codex").is_file())
            && let Some(native) = behind_npm_shim(&dir)
        {
            return Some(native);
        }
    }
    None
}

fn behind_npm_shim(prefix: &std::path::Path) -> Option<PathBuf> {
    let arm = std::env::var("PROCESSOR_ARCHITEW6432")
        .or_else(|_| std::env::var("PROCESSOR_ARCHITECTURE"))
        .is_ok_and(|a| a.eq_ignore_ascii_case("ARM64"));
    let mut targets = vec![("codex-win32-x64", "x86_64-pc-windows-msvc")];
    if arm {
        targets.insert(0, ("codex-win32-arm64", "aarch64-pc-windows-msvc"));
    }
    let package = prefix.join("node_modules").join("@openai").join("codex");
    for (pkg, triple) in targets {
        let candidates = [
            package.join("node_modules").join("@openai").join(pkg).join("vendor").join(triple).join("bin").join("codex.exe"),
            prefix.join("node_modules").join("@openai").join(pkg).join("vendor").join(triple).join("bin").join("codex.exe"),
            package.join("vendor").join(triple).join("bin").join("codex.exe"),
            package.join("vendor").join(triple).join("codex").join("codex.exe"),
        ];
        if let Some(found) = candidates.into_iter().find(|c| c.is_file()) {
            return Some(found);
        }
    }
    None
}

struct AppServer {
    child: Child,
    _job: KillOnDropJob,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
}

impl AppServer {
    fn start(exe: &std::path::Path) -> std::io::Result<AppServer> {
        let mut cmd = process::command(exe, &paths::data_dir());
        cmd.arg("app-server").stderr(std::process::Stdio::null());
        let (mut child, job) = process::spawn(cmd)?;
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        Ok(AppServer { child, _job: job, stdin, lines, next_id: 0 })
    }

    fn send(&mut self, message: &Value) -> std::io::Result<()> {
        let stdin = self.stdin.as_mut().ok_or_else(|| std::io::Error::other("stdin closed"))?;
        let mut line = message.to_string();
        line.push('\n');
        stdin.write_all(line.as_bytes())?;
        stdin.flush()
    }

    fn notify(&mut self, method: &str) -> std::io::Result<()> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": {} }))
    }

    fn call(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Map<String, Value>, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .map_err(|e| format!("codex app-server 쓰기 실패: {e}"))?;
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = match self.lines.recv_timeout(remaining) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => return Err(format!("{method} 응답 시간 초과")),
                Err(RecvTimeoutError::Disconnected) => return Err("codex app-server 종료".into()),
            };
            let Ok(Value::Object(message)) = serde_json::from_str::<Value>(&line) else { continue };
            // Notifications and server-to-client requests carry a method; skip them.
            if message.contains_key("method") || message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.obj("error") {
                return Err(format!("{method} 실패: {}", error.str_of("message").unwrap_or("알 수 없는 오류")));
            }
            return Ok(message.obj("result").cloned().unwrap_or_default());
        }
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        // EOF on stdin asks the server to exit; the job kills it if it does not.
        self.stdin.take();
        if !process::wait_timeout(&self.child, Duration::from_secs(3)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// Windows of the "codex" limit (falling back to the default `rateLimits`).
pub fn parse_limits(limits: &Map<String, Value>, now_ms: i64) -> ProviderState {
    let mut state = ProviderState { updated_at_ms: now_ms, ..Default::default() };
    let main = limits.obj("rateLimitsByLimitId").and_then(|by_id| by_id.obj("codex")).or_else(|| limits.obj("rateLimits"));
    if let Some(main) = main {
        for key in ["primary", "secondary"] {
            let Some(window) = main.obj(key) else { continue };
            let Some(used) = window.f64_of("usedPercent") else { continue };
            state.windows.push(UsageWindow {
                duration_mins: window.i64_of("windowDurationMins"),
                used_percent: Some(used.clamp(0.0, 100.0)),
                resets_at: window.i64_of("resetsAt"),
            });
        }
    }
    if state.windows.is_empty() {
        state.status = Some("한도 정보 없음".into());
    }
    state
}

pub struct Codex {
    last: ProviderState,
    logged_exe: Option<PathBuf>,
}

impl Codex {
    pub fn new(initial: ProviderState) -> Codex {
        Codex { last: initial, logged_exe: None }
    }

    fn read(&mut self) -> Result<ProviderState, (String, String)> {
        let exe = find_exe().ok_or_else(|| ("codex 없음".to_string(), "codex CLI를 찾지 못했습니다".to_string()))?;
        if self.logged_exe.as_ref() != Some(&exe) {
            util::log(&format!("codex: {}", exe.display()));
            self.logged_exe = Some(exe.clone());
        }
        let fail = |e: String| ("연결 실패".to_string(), e);
        let mut rpc = AppServer::start(&exe).map_err(|e| fail(format!("codex 실행 실패: {e}")))?;
        let client = json!({ "clientInfo": { "name": "ameter", "title": "ameter", "version": crate::VERSION } });
        rpc.call("initialize", client, Duration::from_secs(30)).map_err(fail)?;
        rpc.notify("initialized").map_err(|e| fail(e.to_string()))?;
        let account = rpc.call("account/read", json!({ "refreshToken": false }), Duration::from_secs(20)).map_err(fail)?;
        if account.get("account").is_none_or(Value::is_null) {
            return Ok(ProviderState { updated_at_ms: util::now_ms(), ..ProviderState::with_status("로그인 필요") });
        }
        let limits = rpc.call("account/rateLimits/read", json!({}), Duration::from_secs(20)).map_err(fail)?;
        Ok(parse_limits(&limits, util::now_ms()))
    }
}

impl Source for Codex {
    type Output = ProviderState;
    const NAME: &'static str = "codex";
    const INTERVAL: Duration = Duration::from_secs(60);

    fn poll(&mut self) -> ProviderState {
        self.last = match self.read() {
            Ok(state) => {
                model::save_codex(&state);
                state
            }
            Err((status, message)) => {
                util::log(&format!("codex poll failed: {message}"));
                self.last.failed(&status)
            }
        };
        self.last.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_weekly_only_pro_limits() {
        let limits = json!({
            "rateLimits": { "primary": { "usedPercent": 6, "windowDurationMins": 10080, "resetsAt": 1793600909 }, "secondary": null },
            "rateLimitsByLimitId": { "codex": { "primary": { "usedPercent": 6, "windowDurationMins": 10080, "resetsAt": 1793600909 } } }
        });
        let state = parse_limits(limits.as_object().unwrap(), 5);
        assert_eq!(state.windows, vec![UsageWindow { duration_mins: Some(10080), used_percent: Some(6.0), resets_at: Some(1793600909) }]);
        assert_eq!(state.updated_at_ms, 5);
        assert!(state.status.is_none());
    }

    #[test]
    fn falls_back_to_default_limits_and_reports_missing_data() {
        let limits = json!({ "rateLimits": { "primary": { "usedPercent": 40, "windowDurationMins": 300 }, "secondary": { "usedPercent": 10, "windowDurationMins": 10080 } } });
        assert_eq!(parse_limits(limits.as_object().unwrap(), 0).windows.len(), 2);
        assert_eq!(parse_limits(&Map::new(), 0).status.as_deref(), Some("한도 정보 없음"));
    }
}
