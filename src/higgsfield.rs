//! Higgsfield credits from the official CLI. Both commands only read the account
//! (`account status`, `account transactions`) and spend nothing.

use crate::model::{self, ProviderState};
use crate::paths;
use crate::poller::Source;
use crate::process;
use crate::util::{self, JsonExt};
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::time::Duration;

/// The CLI's native binary. npm exposes only a node shim, and `hf.exe` is never taken from PATH:
/// the Hugging Face CLI installs a different `hf.exe`.
pub fn find_exe() -> Option<PathBuf> {
    if let Some(configured) = std::env::var_os("AMETER_HIGGSFIELD_PATH").map(PathBuf::from) {
        return configured.is_file().then_some(configured);
    }
    let mut dirs = paths::path_dirs();
    let npm = paths::app_data().join("npm");
    if !dirs.contains(&npm) {
        dirs.push(npm);
    }
    for dir in dirs {
        let exe = dir.join("higgsfield.exe");
        if exe.is_file() {
            return Some(exe);
        }
        if dir.join("higgsfield.cmd").is_file() || dir.join("higgsfield").is_file() {
            let vendored = dir.join("node_modules").join("@higgsfield").join("cli").join("vendor").join("hf.exe");
            if vendored.is_file() {
                return Some(vendored);
            }
        }
    }
    None
}

fn run(exe: &std::path::Path, args: &[&str]) -> Result<Map<String, Value>, String> {
    let mut cmd = process::command(exe, &paths::data_dir());
    cmd.args(args)
        // What the npm launcher sets; keeps the CLI's update hints pointed at npm.
        .env("HIGGSFIELD_INSTALL_METHOD", "npm")
        .env("HIGGSFIELD_PACKAGE_MANAGER", "npm");
    let output = process::run(cmd, b"", Duration::from_secs(30)).map_err(|e| e.to_string())?;
    let stdout = output.stdout_text();
    let parsed = (output.code == 0).then(|| json_in(&stdout)).flatten();
    parsed.ok_or_else(|| {
        util::first_line(&output.stderr_text())
            .or_else(|| util::first_line(&stdout))
            .unwrap_or_else(|| format!("종료 코드 {}", output.code))
    })
}

/// The JSON object in a tool's output (tolerates banners around it).
fn json_in(text: &str) -> Option<Map<String, Value>> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    match serde_json::from_str::<Value>(text.get(start..=end)?) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// The newest "Subscription Credits" grant plus one calendar month: when the next monthly
/// refill lands. Higgsfield reports no renewal date, but grants arrive on the same day each
/// month. `pages` yields transaction pages newest first.
pub fn renewal_from_grants(pages: impl IntoIterator<Item = Map<String, Value>>) -> Option<i64> {
    for page in pages {
        let items = page.get("items").and_then(Value::as_array)?;
        for item in items.iter().filter_map(Value::as_object) {
            if item.str_of("action") != Some("grant") {
                continue;
            }
            if !item.str_of("display_name").unwrap_or("").to_ascii_lowercase().contains("subscription") {
                continue;
            }
            let granted = chrono::DateTime::parse_from_rfc3339(item.str_of("created_at")?).ok()?;
            let next = granted.checked_add_months(chrono::Months::new(1))?;
            return Some(next.timestamp());
        }
    }
    None
}

pub struct Higgsfield {
    last: ProviderState,
    renewal_scanned_ms: i64,
    last_credits: Option<f64>,
    last_logged: String,
}

const RENEWAL_RESCAN_MS: i64 = 6 * 3600 * 1000;
const OVERDUE_RESCAN_MS: i64 = 30 * 60 * 1000;

impl Higgsfield {
    pub fn new(initial: ProviderState) -> Higgsfield {
        Higgsfield { last: initial, renewal_scanned_ms: 0, last_credits: None, last_logged: String::new() }
    }

    fn read(&mut self) -> Result<ProviderState, (String, String)> {
        let exe = find_exe().ok_or_else(|| ("CLI 없음".to_string(), "higgsfield CLI를 찾지 못했습니다".to_string()))?;
        let status = run(&exe, &["account", "status", "--json", "--no-color"]).map_err(|message| {
            let lower = message.to_lowercase();
            let auth = ["login", "unauthor", "token", "401"].iter().any(|k| lower.contains(k));
            (if auth { "로그인 필요" } else { "연결 실패" }.to_string(), message)
        })?;
        let credits = status.f64_of("credits").ok_or_else(|| ("연결 실패".to_string(), "잔액이 없는 응답".to_string()))?;
        Ok(ProviderState {
            updated_at_ms: util::now_ms(),
            credits: Some(credits),
            renews_at: self.next_renewal(&exe, credits),
            ..Default::default()
        })
    }

    /// Rescans the transactions at start, when the balance goes up (a refill), every 6 hours,
    /// and every 30 minutes once the expected refill is overdue.
    fn next_renewal(&mut self, exe: &std::path::Path, credits: f64) -> Option<i64> {
        let now = util::now_ms();
        let known = self.last.renews_at;
        let upcoming = known.is_some_and(|r| r > now / 1000);
        let refilled = self.last_credits.is_some_and(|c| credits > c + 0.5);
        self.last_credits = Some(credits);
        let age = now - self.renewal_scanned_ms;
        if self.renewal_scanned_ms != 0 && !refilled && age < if upcoming { RENEWAL_RESCAN_MS } else { OVERDUE_RESCAN_MS } {
            return known;
        }
        self.renewal_scanned_ms = now;

        let mut cursor: Option<String> = None;
        for _ in 0..20 {
            let mut args = vec!["account", "transactions", "--json", "--no-color", "--size", "100"];
            if let Some(c) = &cursor {
                args.extend(["--cursor", c.as_str()]);
            }
            let page = match run(exe, &args) {
                Ok(page) => page,
                Err(e) => {
                    util::log(&format!("higgsfield renewal scan failed: {e}"));
                    return known;
                }
            };
            cursor = page.str_of("cursor").filter(|c| !c.is_empty()).map(String::from);
            if let Some(found) = renewal_from_grants([page]) {
                return Some(found);
            }
            if cursor.is_none() {
                break;
            }
        }
        known
    }
}

impl Source for Higgsfield {
    type Output = ProviderState;
    const NAME: &'static str = "higgsfield";
    const INTERVAL: Duration = Duration::from_secs(60);

    fn poll(&mut self) -> ProviderState {
        let summary;
        self.last = match self.read() {
            Ok(state) => {
                model::save_higgsfield(&state);
                summary = "ok".to_string();
                state
            }
            Err((status, message)) => {
                summary = message;
                self.last.failed(&status)
            }
        };
        if summary != self.last_logged {
            util::log(&format!("higgsfield: {summary}"));
            self.last_logged = summary;
        }
        self.last.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn page(items: Value) -> Map<String, Value> {
        json!({ "items": items }).as_object().unwrap().clone()
    }

    #[test]
    fn renewal_is_one_month_after_the_latest_subscription_grant() {
        let first = page(json!([
            { "action": "spend", "created_at": "2026-10-25T22:49:48.549118Z", "credits": -2.75 },
            { "action": "refund", "created_at": "2026-10-20T09:27:24Z", "credits": 168 },
        ]));
        let second = page(json!([
            { "action": "grant", "created_at": "2026-10-08T04:31:17.1Z", "credits": 9000, "display_name": "Subscription Credits" },
            { "action": "grant", "created_at": "2026-09-08T04:20:26Z", "credits": 9000, "display_name": "Subscription Credits" },
        ]));
        let expected = chrono::DateTime::parse_from_rfc3339("2026-11-08T04:31:17.1Z").unwrap().timestamp();
        assert_eq!(renewal_from_grants([first, second]), Some(expected));
    }

    #[test]
    fn month_end_grants_clamp_to_the_shorter_month() {
        let grants = page(json!([{ "action": "grant", "created_at": "2026-01-31T00:00:00Z", "display_name": "Subscription Credits" }]));
        let expected = chrono::DateTime::parse_from_rfc3339("2026-02-28T00:00:00Z").unwrap().timestamp();
        assert_eq!(renewal_from_grants([grants]), Some(expected));
    }

    #[test]
    fn other_grants_do_not_count() {
        let grants = page(json!([{ "action": "grant", "created_at": "2026-10-01T00:00:00Z", "display_name": "Promo" }]));
        assert_eq!(renewal_from_grants([grants]), None);
    }

    #[test]
    fn json_is_found_inside_noise() {
        assert_eq!(json_in("note\n{\"credits\": 7747.05}\n").unwrap().f64_of("credits"), Some(7747.05));
        assert!(json_in("Error: No workspace selected.").is_none());
    }
}
