//! Provider state and the on-disk caches shared by the widget and the CLI.

use crate::paths;
use crate::util::{self, JsonExt};
use serde_json::{Map, Value, json};

pub const FIVE_HOUR_MINS: i64 = 300;
pub const WEEK_MINS: i64 = 10080;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageWindow {
    pub duration_mins: Option<i64>,
    pub used_percent: Option<f64>,
    /// Unix seconds.
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct ProviderState {
    pub windows: Vec<UsageWindow>,
    /// When the values were observed; 0 = never.
    pub updated_at_ms: i64,
    /// Shown instead of values while there are none ("확인 중…", "로그인 필요").
    pub status: Option<String>,
    /// The latest refresh failed; the values are from an earlier one.
    pub failing: bool,
    /// Higgsfield: remaining credit balance.
    pub credits: Option<f64>,
    /// Higgsfield: next monthly credit refill (unix seconds).
    pub renews_at: Option<i64>,
}

impl ProviderState {
    pub fn with_status(status: &str) -> Self {
        ProviderState { status: Some(status.to_string()), ..Default::default() }
    }

    /// Keeps the earlier values, marks them as not current, and explains the failure when
    /// there is nothing to show.
    pub fn failed(&self, status: &str) -> Self {
        let mut state = self.clone();
        state.failing = true;
        if state.windows.is_empty() && state.credits.is_none() {
            state.status = Some(status.to_string());
        }
        state
    }

    pub fn window(&self, mins: i64) -> Option<&UsageWindow> {
        self.windows.iter().find(|w| w.duration_mins == Some(mins))
    }
}

// ---------------------------------------------------------------- Codex / Higgsfield caches

pub fn load_codex() -> ProviderState {
    let Some(cache) = util::read_json_object(&paths::codex_cache()) else {
        return ProviderState::default();
    };
    let mut state = ProviderState {
        updated_at_ms: cache.i64_of("updatedAt").unwrap_or(0),
        status: cache.str_of("status").map(String::from),
        ..Default::default()
    };
    if let Some(windows) = cache.get("windows").and_then(Value::as_array) {
        for w in windows.iter().filter_map(Value::as_object) {
            state.windows.push(UsageWindow {
                duration_mins: w.i64_of("durationMins"),
                used_percent: w.f64_of("usedPercent"),
                resets_at: w.i64_of("resetsAt"),
            });
        }
    }
    state
}

pub fn save_codex(state: &ProviderState) {
    let windows: Vec<Value> = state
        .windows
        .iter()
        .map(|w| {
            json!({
                "durationMins": util::opt_i64(w.duration_mins),
                "usedPercent": util::opt_f64(w.used_percent),
                "resetsAt": util::opt_i64(w.resets_at),
            })
        })
        .collect();
    let cache = json!({ "updatedAt": state.updated_at_ms, "status": state.status, "windows": windows });
    if let Err(e) = util::write_json(&paths::codex_cache(), &cache) {
        util::log(&format!("codex cache write failed: {e}"));
    }
}

pub fn load_higgsfield() -> ProviderState {
    let Some(cache) = util::read_json_object(&paths::higgsfield_cache()) else {
        return ProviderState::default();
    };
    ProviderState {
        updated_at_ms: cache.i64_of("updatedAt").unwrap_or(0),
        credits: cache.f64_of("credits"),
        renews_at: cache.i64_of("renewsAt"),
        ..Default::default()
    }
}

/// Balance and refill time only; the account email is never written to disk.
pub fn save_higgsfield(state: &ProviderState) {
    let cache = json!({
        "updatedAt": state.updated_at_ms,
        "credits": util::opt_f64(state.credits),
        "renewsAt": util::opt_i64(state.renews_at),
    });
    if let Err(e) = util::write_json(&paths::higgsfield_cache(), &cache) {
        util::log(&format!("higgsfield cache write failed: {e}"));
    }
}

// ---------------------------------------------------------------- Claude cache

/// Cache keys, as Claude Code and claude.ai name the windows.
pub const CLAUDE_WINDOWS: [(&str, i64); 2] = [("five_hour", FIVE_HOUR_MINS), ("seven_day", WEEK_MINS)];

pub const SOURCE_WEB: &str = "claude.ai";
pub const SOURCE_STATUSLINE: &str = "Claude Code";

pub fn load_claude() -> ProviderState {
    let Some(cache) = util::read_json_object(&paths::claude_cache()) else {
        return ProviderState::default();
    };
    let mut state = ProviderState { updated_at_ms: cache.i64_of("updatedAt").unwrap_or(0), ..Default::default() };
    if let Some(windows) = cache.obj("windows") {
        for (key, mins) in CLAUDE_WINDOWS {
            let Some(entry) = windows.obj(key) else { continue };
            let Some(used) = entry.f64_of("usedPercent") else { continue };
            state.windows.push(UsageWindow { duration_mins: Some(mins), used_percent: Some(used), resets_at: entry.i64_of("resetsAt") });
        }
    }
    state
}

/// One cache entry; None when the percentage is missing.
pub fn claude_entry(used_percent: Option<f64>, resets_at: Option<&Value>) -> Option<Value> {
    let used = used_percent?.clamp(0.0, 100.0);
    let mut entry = Map::new();
    entry.insert("usedPercent".into(), util::number(used));
    if let Some(reset) = resets_at.and_then(epoch_seconds) {
        entry.insert("resetsAt".into(), reset.into());
    }
    Some(Value::Object(entry))
}

/// Unix seconds from a number (seconds or milliseconds) or an RFC 3339 string. Strings are
/// rounded to the nearest second: claude.ai reports 10:59:59.764 for an 11:00 reset.
pub fn epoch_seconds(value: &Value) -> Option<i64> {
    if let Some(raw) = value.as_f64() {
        let seconds = if raw > 100_000_000_000.0 { raw / 1000.0 } else { raw };
        return Some(seconds as i64);
    }
    let parsed = chrono::DateTime::parse_from_rfc3339(value.as_str()?.trim()).ok()?;
    Some((parsed.timestamp_millis() as f64 / 1000.0).round() as i64)
}

/// How an observed window relates to the cached one.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Older window, or lower usage reported by a stale statusline.
    Keep,
    /// Same values; only the timestamp is refreshed.
    Same,
    Take,
}

/// Statusline input arrives from several Claude Code sessions and idle sessions re-render old
/// numbers, so for that source a window only moves forward: a later reset wins, and inside one
/// window the higher usage wins. claude.ai is the server's own reading (`authoritative`): it
/// replaces values inside the current window, but never goes back to an older window.
pub fn compare_window(existing: Option<&Map<String, Value>>, incoming: &Map<String, Value>, authoritative: bool) -> Verdict {
    let Some(existing) = existing else { return Verdict::Take };
    let old_reset = existing.i64_of("resetsAt");
    let new_reset = incoming.i64_of("resetsAt");
    if let (Some(old), Some(new)) = (old_reset, new_reset) {
        // The same window when the reset times agree within two minutes.
        if new > old + 120 {
            return Verdict::Take;
        }
        if new < old - 120 {
            return Verdict::Keep;
        }
    }
    let old_used = existing.f64_of("usedPercent").unwrap_or(0.0);
    let new_used = incoming.f64_of("usedPercent").unwrap_or(0.0);
    let same_reset = old_reset == new_reset;
    if authoritative {
        return if new_used == old_used && same_reset { Verdict::Same } else { Verdict::Take };
    }
    if new_used > old_used {
        Verdict::Take
    } else if new_used < old_used {
        Verdict::Keep
    } else if new_reset.is_some() && !same_reset {
        Verdict::Take
    } else {
        Verdict::Same
    }
}

/// Merges observed windows ({key: entry}) into the Claude cache under a cross-process mutex.
/// Returns whether the observation was accepted.
pub fn merge_claude(observed: &Map<String, Value>, authoritative: bool, source: &str, now_ms: i64) -> bool {
    if observed.is_empty() {
        return false;
    }
    let _lock = NamedMutex::acquire("Local\\ameter.claude-cache", 2000);
    let path = paths::claude_cache();
    let mut cache = util::read_json_object(&path).unwrap_or_default();
    let mut windows = cache.obj("windows").cloned().unwrap_or_default();

    let mut accepted = false;
    let mut changed = false;
    for (key, incoming) in observed {
        let Some(incoming) = incoming.as_object() else { continue };
        match compare_window(windows.obj(key), incoming, authoritative) {
            Verdict::Keep => continue,
            Verdict::Same => accepted = true,
            Verdict::Take => {
                accepted = true;
                changed = true;
                windows.insert(key.clone(), Value::Object(incoming.clone()));
            }
        }
    }
    if !accepted {
        return false;
    }
    cache.insert("windows".into(), Value::Object(windows));
    cache.insert("updatedAt".into(), now_ms.into());
    cache.insert("source".into(), source.into());
    if let Err(e) = util::write_json(&path, &Value::Object(cache)) {
        util::log(&format!("claude cache write failed: {e}"));
        return false;
    }
    if changed {
        util::log(&format!("claude usage updated ({source})"));
    }
    true
}

/// Win32 named mutex, held until drop. Acquisition failure or timeout proceeds unlocked:
/// a skipped lock costs at most one lost update, a hang would freeze the statusline.
pub struct NamedMutex {
    handle: windows_sys::Win32::Foundation::HANDLE,
    owned: bool,
}

impl NamedMutex {
    pub fn acquire(name: &str, timeout_ms: u32) -> NamedMutex {
        use windows_sys::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        let name = util::wide(name);
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        let mut owned = false;
        if !handle.is_null() {
            let wait = unsafe { WaitForSingleObject(handle, timeout_ms) };
            owned = wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED;
        }
        NamedMutex { handle, owned }
    }
}

impl Drop for NamedMutex {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::ReleaseMutex;
        if self.handle.is_null() {
            return;
        }
        unsafe {
            if self.owned {
                ReleaseMutex(self.handle);
            }
            CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(used: f64, reset: i64) -> Map<String, Value> {
        claude_entry(Some(used), Some(&reset.into())).unwrap().as_object().unwrap().clone()
    }

    #[test]
    fn statusline_windows_only_move_forward() {
        let current = entry(30.0, 10_000);
        assert_eq!(compare_window(Some(&current), &entry(20.0, 10_030), false), Verdict::Keep);
        assert_eq!(compare_window(Some(&current), &entry(35.0, 10_000), false), Verdict::Take);
        assert_eq!(compare_window(Some(&current), &entry(90.0, 10_000 - 18_000), false), Verdict::Keep);
        assert_eq!(compare_window(Some(&current), &entry(2.0, 10_000 + 18_000), false), Verdict::Take);
        assert_eq!(compare_window(Some(&current), &entry(30.0, 10_000), false), Verdict::Same);
        assert_eq!(compare_window(None, &current, false), Verdict::Take);
    }

    #[test]
    fn claude_ai_replaces_inside_the_window_but_not_across() {
        let current = entry(30.0, 10_000);
        assert_eq!(compare_window(Some(&current), &entry(20.0, 10_000), true), Verdict::Take);
        assert_eq!(compare_window(Some(&current), &entry(30.0, 10_000), true), Verdict::Same);
        assert_eq!(compare_window(Some(&current), &entry(50.0, 10_000 - 18_000), true), Verdict::Keep);
    }

    #[test]
    fn epoch_seconds_reads_numbers_and_rounds_timestamps() {
        assert_eq!(epoch_seconds(&json!(1_793_086_800)), Some(1_793_086_800));
        assert_eq!(epoch_seconds(&json!(1_793_086_800_123i64)), Some(1_793_086_800));
        assert_eq!(epoch_seconds(&json!("2026-10-27T10:59:59.764+00:00")), Some(1_793_098_800));
        assert_eq!(epoch_seconds(&json!("2026-10-27T11:00:00Z")), Some(1_793_098_800));
        assert_eq!(epoch_seconds(&json!("soon")), None);
    }

    #[test]
    fn entries_clamp_and_require_a_percentage() {
        let e = claude_entry(Some(140.0), None).unwrap();
        assert_eq!(e["usedPercent"].as_f64(), Some(100.0));
        assert!(e.get("resetsAt").is_none());
        assert!(claude_entry(None, Some(&json!(5))).is_none());
    }
}
