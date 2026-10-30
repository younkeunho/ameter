//! Claude usage as claude.ai reports it, read through the signed-in Aside browser. ameter connects
//! to the browser's DevTools port (remote debugging turned on in chrome://inspect), keeps one
//! hidden claude.ai tab that is not in the tab strip, and runs two cookie-bearing GET requests
//! there. No agent, no model. Skipped while the browser is closed, so polling never launches it.
//!
//! The browser asks the user once per connection, so the connection is kept for as long as the
//! browser runs. A refused connection is not retried until the browser is started again.

use crate::cdp::{Cdp, CdpError, Handshake};
use crate::model::{self, CLAUDE_WINDOWS};
use crate::paths;
use crate::poller::Source;
use crate::util::{self, JsonExt};
use serde_json::{Map, Value, json};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct WebStatus {
    /// Aside is installed, so claude.ai is a usage source.
    pub available: bool,
    pub ok: bool,
    /// Bar text while nothing is cached.
    pub status: Option<String>,
}

/// A small same-origin page; the requests need the claude.ai origin and its cookies.
const PAGE: &str = "https://claude.ai/robots.txt";
const ORIGIN: &str = "https://claude.ai";
/// How long one poll waits for the user to answer the browser's question.
const CONSENT_WAIT: Duration = Duration::from_secs(5);
const LOAD_TIMEOUT: Duration = Duration::from_secs(15);
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);

const SCRIPT: &str = "(async () => {
  const out = {};
  try {
    const r = await fetch('/api/organizations');
    out.orgStatus = r.status;
    if (r.ok) {
      const list = await r.json();
      const caps = (o) => (o && o.capabilities) || [];
      const orgs = Array.isArray(list) ? list : [];
      const org = orgs.find((o) => caps(o).includes('claude_max') || caps(o).includes('claude_pro'))
        || orgs.find((o) => caps(o).includes('chat'));
      if (org) {
        const u = await fetch('/api/organizations/' + org.uuid + '/usage');
        out.usageStatus = u.status;
        if (u.ok) { const j = await u.json(); out.five_hour = j.five_hour || null; out.seven_day = j.seven_day || null; }
      }
    }
  } catch (e) { out.error = String((e && e.message) || e); }
  return JSON.stringify(out);
})()";

/// Turns the script's result into cache entries, or a (status, log message) problem.
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

/// Port and browser path from `DevToolsActivePort`. The path changes with every browser start.
type Endpoint = (u16, String);

fn devtools_endpoint() -> Option<Endpoint> {
    let text = std::fs::read_to_string(paths::aside_user_data().join("DevToolsActivePort")).ok()?;
    let mut lines = text.lines().map(str::trim);
    let port = lines.next()?.parse().ok()?;
    let path = lines.next().filter(|p| p.starts_with('/'))?.to_string();
    Some((port, path))
}

enum Link {
    Closed,
    /// Sent; the browser is asking the user.
    Asking(Endpoint, Handshake),
    /// With the hidden claude.ai tab, once opened.
    Open(Endpoint, Cdp, Option<Tab>),
    Refused(Endpoint),
}

impl Link {
    fn endpoint(&self) -> Option<&Endpoint> {
        match self {
            Link::Closed => None,
            Link::Asking(e, _) | Link::Open(e, _, _) | Link::Refused(e) => Some(e),
        }
    }
}

struct Tab {
    target: String,
    session: String,
}

fn close_tab(cdp: &mut Cdp, target: &str) {
    let _ = cdp.call("Target.closeTarget", json!({ "targetId": target }), None);
}

/// Opens the hidden claude.ai tab and waits until the page has loaded. A tab that does not get
/// there is closed again.
fn open_tab(cdp: &mut Cdp) -> Result<Tab, CdpError> {
    let created = cdp.call("Target.createTarget", json!({ "url": PAGE, "hidden": true }), None)?;
    let target = created.str_of("targetId").ok_or_else(|| CdpError::Protocol("Target.createTarget: no target".into()))?.to_string();
    match attach_loaded(cdp, &target) {
        Ok(session) => Ok(Tab { target, session }),
        Err(e) => {
            close_tab(cdp, &target);
            Err(e)
        }
    }
}

fn attach_loaded(cdp: &mut Cdp, target: &str) -> Result<String, CdpError> {
    let attached = cdp.call("Target.attachToTarget", json!({ "targetId": target, "flatten": true }), None)?;
    let session = attached.str_of("sessionId").ok_or_else(|| CdpError::Protocol("Target.attachToTarget: no session".into()))?.to_string();
    let ready = format!("{ORIGIN} complete");
    let started = Instant::now();
    loop {
        // Until the document exists there is no execution context to evaluate in.
        match cdp.evaluate(&session, "location.origin + ' ' + document.readyState") {
            Ok(state) if state.as_str() == Some(ready.as_str()) => return Ok(session),
            Ok(_) | Err(CdpError::Protocol(_)) => {}
            Err(e) => return Err(e),
        }
        if started.elapsed() > LOAD_TIMEOUT {
            return Err(CdpError::Protocol("claude.ai 페이지가 열리지 않습니다".into()));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Runs the requests in the hidden tab, opening it first if needed. A tab that fails is
/// replaced on the next read.
fn read_usage(cdp: &mut Cdp, tab: &mut Option<Tab>) -> Result<Map<String, Value>, CdpError> {
    let session = match tab {
        Some(tab) => tab.session.clone(),
        None => tab.insert(open_tab(cdp)?).session.clone(),
    };
    let value = match cdp.evaluate(&session, SCRIPT) {
        Ok(value) => value,
        Err(e) => {
            if let Some(failed) = tab.take() {
                close_tab(cdp, &failed.target);
            }
            return Err(e);
        }
    };
    match value.as_str().map(serde_json::from_str::<Value>) {
        Some(Ok(Value::Object(result))) => Ok(result),
        _ => Err(CdpError::Protocol("claude.ai 응답을 읽지 못했습니다".into())),
    }
}

pub struct ClaudeWeb {
    link: Link,
    last_logged: String,
}

impl Default for ClaudeWeb {
    fn default() -> Self {
        ClaudeWeb { link: Link::Closed, last_logged: String::new() }
    }
}

impl ClaudeWeb {
    fn read(&mut self) -> Result<(), (&'static str, String)> {
        let closed = || ("Aside 꺼짐", "Aside 브라우저가 꺼져 있거나 원격 디버깅이 꺼져 있습니다".to_string());
        let endpoint = devtools_endpoint().ok_or_else(closed)?;
        if self.link.endpoint() != Some(&endpoint) {
            self.link = Link::Closed; // the browser was started again: a new connection, a new question
        }
        if matches!(self.link, Link::Refused(_)) {
            return Err(("허용 안 됨", "Aside에서 원격 디버깅을 거부했습니다. Aside를 다시 켜면 다시 묻습니다".into()));
        }
        if matches!(self.link, Link::Closed) {
            // Refused: the file is left over from a browser that has exited.
            let handshake = Handshake::start(endpoint.0, &endpoint.1).map_err(|_| closed())?;
            self.link = Link::Asking(endpoint.clone(), handshake);
        }
        if let Link::Asking(_, handshake) = &mut self.link {
            match handshake.progress(CONSENT_WAIT) {
                Ok(false) => return Err(("허용 필요", "Aside의 '원격 디버깅을 허용하시겠습니까?' 창에서 허용을 누르세요".into())),
                Ok(true) => {
                    let Link::Asking(endpoint, handshake) = std::mem::replace(&mut self.link, Link::Closed) else { unreachable!() };
                    let cdp = handshake.into_cdp(REPLY_TIMEOUT).map_err(|e| ("연결 실패", e.to_string()))?;
                    self.link = Link::Open(endpoint, cdp, None);
                }
                Err(e) => {
                    self.link = Link::Refused(endpoint);
                    return Err(("허용 안 됨", format!("Aside에서 원격 디버깅을 거부했습니다 ({e})")));
                }
            }
        }
        let Link::Open(_, cdp, tab) = &mut self.link else { unreachable!() };
        let result = match read_usage(cdp, tab) {
            Ok(result) => result,
            Err(e) => {
                if matches!(e, CdpError::Io(_)) {
                    self.link = Link::Closed;
                }
                return Err(("연결 실패", e.to_string()));
            }
        };
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
        if !paths::aside_user_data().is_dir() {
            return WebStatus::default();
        }
        let (status, summary) = match self.read() {
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
