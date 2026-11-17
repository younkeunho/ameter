//! Provider state and the on-disk caches shared by the widget and the CLI.

use crate::paths;
use crate::util::{self, JsonExt};
use serde_json::{Map, Value, json};

pub const FIVE_HOUR_MINS: i64 = 300;
pub const WEEK_MINS: i64 = 10080;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WindowKind {
    #[default]
    Timed,
    Fable,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageWindow {
    pub kind: WindowKind,
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
                kind: WindowKind::Timed,
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

/// Independent weekly buckets. Duration alone cannot distinguish Fable from the overall quota.
pub const CLAUDE_WINDOWS: [(&str, WindowKind); 2] = [("seven_day", WindowKind::Timed), ("fable", WindowKind::Fable)];

pub const SOURCE_WEB: &str = "claude.ai";
pub const SOURCE_STATUSLINE: &str = "Claude Code";

pub fn load_claude() -> ProviderState {
    let Some(cache) = util::read_json_object(&paths::claude_cache()) else {
        return ProviderState::default();
    };
    let mut state = ProviderState { updated_at_ms: cache.i64_of("updatedAt").unwrap_or(0), ..Default::default() };
    if let Some(windows) = cache.obj("windows") {
        for (key, kind) in CLAUDE_WINDOWS {
            let Some(entry) = windows.obj(key) else { continue };
            let Some(used) = entry.f64_of("usedPercent") else { continue };
            state.windows.push(UsageWindow {
                kind,
                duration_mins: Some(WEEK_MINS),
                used_percent: Some(used),
                resets_at: entry.i64_of("resetsAt"),
            });
        }
    }
    state
}

/// Current Claude usage API and statusline `limits` rows. Missing buckets are explicit nulls;
/// malformed relevant data is not an observation and must not erase a known value.
pub fn parse_claude_limits(limits: &Value) -> Option<Map<String, Value>> {
    let rows = limits.as_array()?;
    let mut observed: Map<String, Value> = CLAUDE_WINDOWS.into_iter().map(|(key, _)| (key.into(), Value::Null)).collect();
    let mut exact_fable = false;
    for row in rows {
        let row = row.as_object()?;
        let (key, exact) = match row.str_of("kind")? {
            "weekly_all" => ("seven_day", false),
            "weekly_scoped" => {
                let Some(name) = row.obj("scope").and_then(|s| s.obj("model")).and_then(|m| m.str_of("display_name")) else { continue };
                let lower = name.trim().to_ascii_lowercase();
                let Some(rest) = lower.strip_prefix("fable") else { continue };
                if rest.chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
                    continue;
                }
                ("fable", rest.is_empty())
            }
            _ => continue,
        };
        let used = row.f64_of("percent").filter(|v| v.is_finite() && (0.0..=100.0).contains(v))?;
        let reset = row.get("resets_at")?;
        if !reset.is_null() && (!reset.is_string() || epoch_seconds(reset).is_none()) {
            return None;
        }
        // Prefer the unified Fable bucket, otherwise keep the first family row in server order.
        if !observed[key].is_null() && (key != "fable" || exact_fable || !exact) {
            continue;
        }
        observed.insert(key.into(), claude_entry(Some(used), Some(reset))?);
        if key == "fable" {
            exact_fable = exact;
        }
    }
    Some(observed)
}

/// One cache entry; None when the percentage is missing.
pub fn claude_entry(used_percent: Option<f64>, resets_at: Option<&Value>) -> Option<Value> {
    let used = used_percent.filter(|p| p.is_finite())?.clamp(0.0, 100.0);
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

/// Applies observed windows ({key: entry}) to the cached ones; returns (accepted, changed).
/// An entry of `null` means the source reports no such window (Anthropic dropped the limit);
/// claude.ai's word for that removes the window. A window that has already ended says nothing
/// about the current one and is ignored.
pub fn apply_windows(windows: &mut Map<String, Value>, observed: &Map<String, Value>, authoritative: bool, now_sec: i64) -> (bool, bool) {
    let mut accepted = false;
    let old_len = windows.len();
    windows.retain(|key, _| CLAUDE_WINDOWS.iter().any(|(known, _)| key == known));
    let mut changed = old_len != windows.len();
    for (key, incoming) in observed {
        if !CLAUDE_WINDOWS.iter().any(|(known, _)| key == known) {
            continue;
        }
        match incoming {
            Value::Null if authoritative => {
                accepted = true;
                changed |= windows.remove(key).is_some();
            }
            Value::Object(incoming) if !incoming.i64_of("resetsAt").is_some_and(|r| r <= now_sec) => {
                match compare_window(windows.obj(key), incoming, authoritative) {
                    Verdict::Keep => {}
                    Verdict::Same => accepted = true,
                    Verdict::Take => {
                        accepted = true;
                        changed = true;
                        windows.insert(key.clone(), Value::Object(incoming.clone()));
                    }
                }
            }
            _ => {}
        }
    }
    (accepted, changed)
}

/// Merges observed windows into the Claude cache under a cross-process mutex.
/// Returns whether the observation was accepted.
pub fn merge_claude(observed: &Map<String, Value>, authoritative: bool, source: &str, now_ms: i64) -> bool {
    if observed.is_empty() {
        return false;
    }
    let _lock = NamedMutex::acquire("Local\\ameter.claude-cache", 2000);
    let path = paths::claude_cache();
    let mut cache = util::read_json_object(&path).unwrap_or_default();
    let mut windows = cache.obj("windows").cloned().unwrap_or_default();
    let (accepted, changed) = apply_windows(&mut windows, observed, authoritative, now_ms / 1000);
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
    fn a_window_claude_ai_no_longer_reports_is_dropped() {
        let now = 20_000;
        let mut windows = Map::new();
        windows.insert("fable".into(), Value::Object(entry(0.0, now + 3600)));
        windows.insert("seven_day".into(), Value::Object(entry(18.0, now + 86_400)));
        let observed: Map<String, Value> =
            [("fable".to_string(), Value::Null), ("seven_day".to_string(), Value::Object(entry(19.0, now + 86_400)))].into_iter().collect();

        // The statusline cannot tell an absent window from missing input: nothing is removed.
        let mut from_statusline = windows.clone();
        apply_windows(&mut from_statusline, &observed, false, now);
        assert!(from_statusline.contains_key("fable"));

        assert_eq!(apply_windows(&mut windows, &observed, true, now), (true, true));
        assert!(!windows.contains_key("fable"));
        assert_eq!(windows.obj("seven_day").and_then(|w| w.f64_of("usedPercent")), Some(19.0));
    }

    #[test]
    fn an_ended_window_is_not_brought_back() {
        let now = 20_000;
        let mut windows = Map::new();
        let ended: Map<String, Value> = [("fable".to_string(), Value::Object(entry(40.0, now - 60)))].into_iter().collect();
        assert_eq!(apply_windows(&mut windows, &ended, false, now), (false, false));
        assert!(windows.is_empty());
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
        assert!(claude_entry(Some(f64::NAN), None).is_none());
    }

    #[test]
    fn current_limits_distinguish_weekly_and_fable_and_ignore_other_scopes() {
        let rows = json!([
            {"kind":"weekly_scoped","percent":99,"resets_at":null,"scope":{"model":{"display_name":"Sonnet"}}},
            {"kind":"weekly_all","percent":78,"resets_at":"2026-11-20T11:00:00Z"},
            {"kind":"weekly_scoped","percent":32,"resets_at":null,"scope":{"model":{"display_name":"Fable 5.1"}}},
            {"kind":"session","percent":95,"resets_at":null}
        ]);
        let parsed = parse_claude_limits(&rows).unwrap();
        assert_eq!(parsed["seven_day"]["usedPercent"], 78.0);
        assert_eq!(parsed["fable"]["usedPercent"], 32.0);
        assert!(parsed["fable"].get("resetsAt").is_none());
        assert!(!parsed.contains_key("five_hour"));
    }

    #[test]
    fn fable_bucket_prefers_exact_name_without_combining_model_percentages() {
        let parsed = parse_claude_limits(&json!([
            {"kind":"weekly_scoped","percent":99,"resets_at":null,"scope":{"model":{"display_name":"FableExtra"}}},
            {"kind":"weekly_scoped","percent":32,"resets_at":null,"scope":{"model":{"display_name":"fAbLe 5.1"}}},
            {"kind":"weekly_scoped","percent":42,"resets_at":null,"scope":{"model":{"display_name":"FABLE"}}},
            {"kind":"weekly_scoped","percent":52,"resets_at":null,"scope":{"model":{"display_name":"Fable 5.2"}}}
        ]))
        .unwrap();
        assert_eq!(parsed["fable"]["usedPercent"], 42.0);
        assert!(parsed["seven_day"].is_null());
    }

    #[test]
    fn missing_and_malformed_limits_do_not_become_zero() {
        for value in [
            json!(null),
            json!({}),
            json!([null]),
            json!([{"kind":"weekly_all","resets_at":null}]),
            json!([{"kind":"weekly_all","percent":-1,"resets_at":null}]),
            json!([{"kind":"weekly_all","percent":101,"resets_at":null}]),
            json!([{"kind":"weekly_all","percent":10,"resets_at":"soon"}]),
        ] {
            assert!(parse_claude_limits(&value).is_none(), "{value}");
        }
        let empty = parse_claude_limits(&json!([])).unwrap();
        assert!(empty["seven_day"].is_null() && empty["fable"].is_null());
    }

    #[test]
    fn obsolete_claude_five_hour_is_removed_and_never_reintroduced() {
        let mut windows = json!({"five_hour":{"usedPercent":55}, "seven_day":{"usedPercent":78}}).as_object().unwrap().clone();
        let observed = json!({"five_hour":{"usedPercent":20}, "seven_day":{"usedPercent":80}}).as_object().unwrap().clone();
        assert_eq!(apply_windows(&mut windows, &observed, false, 10), (true, true));
        assert!(!windows.contains_key("five_hour"));
        assert_eq!(windows["seven_day"]["usedPercent"], 80);
        let empty = parse_claude_limits(&json!([])).unwrap();
        assert_eq!(apply_windows(&mut windows, &empty, true, 10), (true, true));
        assert!(windows.is_empty());
    }
}
