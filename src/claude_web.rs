//! Claude usage as claude.ai reports it, read through the signed-in Aside browser: `aside repl`
//! runs two cookie-bearing GET requests in the browser (no agent, no model). Skipped while the
//! browser is closed, so polling never launches it.

use crate::model::{self, CLAUDE_WINDOWS};
use crate::paths;
use crate::poller::Source;
use crate::process;
use crate::util::{self, JsonExt};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct WebStatus {
    /// An Aside CLI exists, so claude.ai is a usage source.
    pub available: bool,
    pub ok: bool,
    /// Bar text while nothing is cached.
    pub status: Option<String>,
}

const MARKER: &str = "AMETER ";

const SCRIPT: &str = "await (async () => {
  const out = {};
  try {
    const r = await fetch('https://claude.ai/api/organizations');
    out.orgStatus = r.status;
    if (r.ok) {
      const list = await r.json();
      const caps = (o) => (o && o.capabilities) || [];
      const orgs = Array.isArray(list) ? list : [];
      const org = orgs.find((o) => caps(o).includes('claude_max') || caps(o).includes('claude_pro'))
        || orgs.find((o) => caps(o).includes('chat'));
      if (org) {
        const u = await fetch('https://claude.ai/api/organizations/' + org.uuid + '/usage');
        out.usageStatus = u.status;
        if (u.ok) { const j = await u.json(); out.five_hour = j.five_hour || null; out.seven_day = j.seven_day || null; }
      }
    }
  } catch (e) { out.error = String((e && e.message) || e); }
  console.log('AMETER ' + JSON.stringify(out));
})();";

pub fn find_cli() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("AMETER_ASIDE_PATH").map(PathBuf::from) {
        return configured.is_file().then_some(configured);
    }
    paths::path_dirs()
        .into_iter()
        .map(|d| d.join("aside.exe"))
        .chain(std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Aside").join("CLI").join("current").join("aside.exe")))
        .find(|p| p.is_file())
}

/// Turns the script's result line into cache entries, or a (status, log message) problem.
pub fn parse_result(result: &Map<String, Value>) -> Result<Map<String, Value>, (&'static str, String)> {
    if let Some(error) = result.str_of("error") {
        return Err(("연결 실패", format!("claude.ai 요청 실패: {error}")));
    }
    match result.i64_of("orgStatus").unwrap_or(0) {
        200 => {}
        401 | 403 => return Err(("로그인 필요", "Aside 브라우저에서 claude.ai에 로그인하세요".into())),
        other => return Err(("연결 실패", format!("claude.ai 응답 {other}"))),
    }
    let Some(usage_status) = result.i64_of("usageStatus") else {
        return Err(("구독 없음", "claude.ai 계정에 Pro/Max 구독이 없습니다".into()));
    };
    if usage_status != 200 {
        return Err(("연결 실패", format!("claude.ai 사용량 응답 {usage_status}")));
    }
    let mut observed = Map::new();
    for (key, _) in CLAUDE_WINDOWS {
        let Some(window) = result.obj(key) else { continue };
        if let Some(entry) = model::claude_entry(window.f64_of("utilization"), window.get("resets_at")) {
            observed.insert(key.into(), entry);
        }
    }
    if observed.is_empty() {
        return Err(("데이터 없음", "claude.ai 응답에 사용량이 없습니다".into()));
    }
    Ok(observed)
}

/// The Aside browser, not the CLI (both are aside.exe).
fn browser_running(cli: &Path) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };

    let cli = cli.to_string_lossy().to_lowercase();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_null() || snapshot == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return true; // cannot tell; let the request decide
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut found = false;
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while more && !found {
        let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
        if String::from_utf16_lossy(&entry.szExeFile[..len]).eq_ignore_ascii_case("aside.exe") {
            let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, entry.th32ProcessID) };
            if process.is_null() {
                found = true; // inaccessible: assume it is the browser
            } else {
                let mut buffer = [0u16; 1024];
                let mut size = buffer.len() as u32;
                let ok = unsafe { QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buffer.as_mut_ptr(), &mut size) } != 0;
                unsafe { CloseHandle(process) };
                found = !ok || String::from_utf16_lossy(&buffer[..size as usize]).to_lowercase() != cli;
            }
        }
        more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    found
}

#[derive(Default)]
pub struct ClaudeWeb {
    last_logged: String,
}

impl ClaudeWeb {
    fn read(&self, cli: &Path) -> Result<(), (&'static str, String)> {
        if !browser_running(cli) {
            return Err(("Aside 꺼짐", "Aside 브라우저가 꺼져 있습니다".into()));
        }
        let mut cmd = process::command(cli, &paths::data_dir());
        // One command-line argument; the script has no double quotes or backslashes.
        cmd.arg("repl").arg(SCRIPT.replace('\n', " "));
        let output = process::run(cmd, b"", Duration::from_secs(45)).map_err(|e| ("연결 실패", format!("aside 실행 실패: {e}")))?;
        let stdout = output.stdout_text();
        let result = stdout
            .lines()
            .rev()
            .filter_map(|l| l.trim().strip_prefix(MARKER))
            .filter_map(|json| match serde_json::from_str::<Value>(json) {
                Ok(Value::Object(map)) => Some(map),
                _ => None,
            })
            .next()
            .ok_or_else(|| {
                let detail = util::first_line(&output.stderr_text()).unwrap_or_else(|| format!("종료 코드 {}", output.code));
                ("연결 실패", format!("aside 응답 없음: {detail}"))
            })?;
        let observed = parse_result(&result)?;
        model::merge_claude(&observed, true, model::SOURCE_WEB, util::now_ms());
        Ok(())
    }
}

impl Source for ClaudeWeb {
    type Output = WebStatus;
    const NAME: &'static str = "claude-web";
    const INTERVAL: Duration = Duration::from_secs(120);

    fn poll(&mut self) -> WebStatus {
        let Some(cli) = find_cli() else { return WebStatus::default() };
        let (status, summary) = match self.read(&cli) {
            Ok(()) => (WebStatus { available: true, ok: true, status: None }, "ok".to_string()),
            Err((status, message)) => (WebStatus { available: true, ok: false, status: Some(status.into()) }, message),
        };
        if summary != self.last_logged {
            util::log(&format!("claude.ai via aside: {summary}"));
            self.last_logged = summary;
        }
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn usage_becomes_cache_entries() {
        let result = obj(json!({
            "orgStatus": 200, "usageStatus": 200,
            "five_hour": { "utilization": 19.0, "resets_at": "2026-10-27T07:40:00.312+00:00" },
            "seven_day": { "utilization": 16.0, "resets_at": "2026-10-28T11:00:00+00:00" }
        }));
        let observed = parse_result(&result).unwrap();
        assert_eq!(observed["five_hour"]["usedPercent"].as_f64(), Some(19.0));
        assert!(observed["seven_day"]["resetsAt"].as_i64().is_some());
    }

    #[test]
    fn problems_map_to_bar_statuses() {
        assert_eq!(parse_result(&obj(json!({ "orgStatus": 403 }))).unwrap_err().0, "로그인 필요");
        assert_eq!(parse_result(&obj(json!({ "orgStatus": 200 }))).unwrap_err().0, "구독 없음");
        assert_eq!(parse_result(&obj(json!({ "orgStatus": 200, "usageStatus": 500 }))).unwrap_err().0, "연결 실패");
        assert_eq!(parse_result(&obj(json!({ "error": "offline" }))).unwrap_err().0, "연결 실패");
        assert_eq!(parse_result(&obj(json!({ "orgStatus": 200, "usageStatus": 200 }))).unwrap_err().0, "데이터 없음");
    }
}
