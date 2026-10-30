//! Compact text for the bar and the terminal: "5h", "W", "94%", "1h05m", "7,747".

use crate::model::{FIVE_HOUR_MINS, ProviderState, UsageWindow, WEEK_MINS};

/// Bar label of a window: 5h, W, or the raw duration (2d, 3h, 45m).
pub fn label(mins: Option<i64>) -> String {
    match mins {
        None => "?".into(),
        Some(FIVE_HOUR_MINS) => "5h".into(),
        Some(WEEK_MINS) => "W".into(),
        Some(m) if m % 1440 == 0 => format!("{}d", m / 1440),
        Some(m) if m % 60 == 0 => format!("{}h", m / 60),
        Some(m) => format!("{m}m"),
    }
}

/// Remaining share of a window: 100 minus the rounded usage, never below zero.
pub fn left(used_percent: f64) -> String {
    format!("{}%", (100 - used_percent.round() as i64).max(0))
}

/// Remaining credits rounded down, so the balance is never overstated: 7,747.
pub fn credits(value: f64) -> String {
    let whole = value.max(0.0).floor() as u64;
    let digits = whole.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Countdown: <1m, 42m, 1h05m, 5d13h.
pub fn short(seconds: i64) -> String {
    if seconds < 60 {
        return "<1m".into();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let (hours, minutes) = (minutes / 60, minutes % 60);
    if hours < 24 {
        return format!("{hours}h{minutes:02}m");
    }
    format!("{}d{}h", hours / 24, hours % 24)
}

/// 0 normal, 1 under 30% left, 2 under 15% left, 3 under 5% left.
pub fn severity(used_percent: f64) -> u8 {
    match used_percent {
        p if p >= 95.0 => 3,
        p if p >= 85.0 => 2,
        p if p >= 70.0 => 1,
        _ => 0,
    }
}

/// Usage in the current window. A window whose reset has passed counts as unused only when a
/// reading taken after the reset still showed no newer window; until then it is unknown.
pub fn current_used(state: &ProviderState, w: &UsageWindow, now_sec: i64) -> Option<f64> {
    match w.resets_at {
        Some(reset) if reset <= now_sec => (state.updated_at_ms / 1000 >= reset).then_some(0.0),
        _ => w.used_percent,
    }
}

/// Time until the reset; none for a window that has not started (no reset, or a past one).
pub fn countdown(w: &UsageWindow, now_sec: i64) -> Option<String> {
    w.resets_at.filter(|r| *r > now_sec).map(|r| short(r - now_sec))
}

/// Display order: 5-hour, weekly, anything else.
pub fn ordered(windows: &[UsageWindow]) -> Vec<&UsageWindow> {
    let rank = |w: &&UsageWindow| match w.duration_mins {
        Some(FIVE_HOUR_MINS) => 0,
        Some(WEEK_MINS) => 1,
        _ => 2,
    };
    let mut list: Vec<&UsageWindow> = windows.iter().collect();
    list.sort_by_key(rank);
    list
}

/// One terminal line of remaining quota: "CO  W 94% (6d0h)".
pub fn terminal_line(name: &str, state: &ProviderState, now_sec: i64, ansi: bool) -> String {
    const RESET: &str = "\x1b[0m";
    const DIM: &str = "\x1b[2m";
    let paint = |code: &str, text: &str| if ansi { format!("{code}{text}{RESET}") } else { text.to_string() };

    let color = if name == "CC" { "\x1b[38;2;215;119;87m" } else { "\x1b[1m" };
    let mut line = paint(color, &format!("{name:<3}"));
    if let Some(credits) = state.credits {
        line.push(' ');
        line.push_str(&paint("\x1b[1m", &self::credits(credits)));
        if let Some(renews) = state.renews_at.filter(|r| *r > now_sec) {
            line.push_str(&paint(DIM, &format!(" ({})", short(renews - now_sec))));
        }
        return line;
    }
    if state.windows.is_empty() {
        line.push(' ');
        line.push_str(&paint(DIM, state.status.as_deref().unwrap_or("—")));
        return line;
    }
    for (i, w) in ordered(&state.windows).into_iter().enumerate() {
        line.push_str(&if i == 0 { " ".to_string() } else { paint(DIM, " · ") });
        line.push_str(&paint(DIM, &label(w.duration_mins)));
        line.push(' ');
        match current_used(state, w, now_sec) {
            None => line.push('—'),
            Some(used) => {
                let code = match severity(used) {
                    3 => "\x1b[31m",
                    2 => "\x1b[38;5;208m",
                    1 => "\x1b[33m",
                    _ => "\x1b[1m",
                };
                line.push_str(&paint(code, &left(used)));
                if let Some(reset) = countdown(w, now_sec) {
                    line.push_str(&paint(DIM, &format!(" ({reset})")));
                }
            }
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(label(Some(300)), "5h");
        assert_eq!(label(Some(10080)), "W");
        assert_eq!(label(Some(2880)), "2d");
        assert_eq!(label(Some(180)), "3h");
        assert_eq!(label(Some(45)), "45m");
        assert_eq!(label(None), "?");
    }

    #[test]
    fn remaining_is_consistent_with_rounded_usage() {
        assert_eq!(left(23.5), "76%");
        assert_eq!(left(41.2), "59%");
        assert_eq!(left(0.0), "100%");
        assert_eq!(left(100.0), "0%");
        assert_eq!(left(120.0), "0%");
    }

    #[test]
    fn credits_round_down_with_separators() {
        assert_eq!(credits(7747.05), "7,747");
        assert_eq!(credits(999.99), "999");
        assert_eq!(credits(1_234_567.0), "1,234,567");
        assert_eq!(credits(-3.0), "0");
    }

    #[test]
    fn countdowns() {
        assert_eq!(short(59), "<1m");
        assert_eq!(short(42 * 60), "42m");
        assert_eq!(short(3600 + 5 * 60), "1h05m");
        assert_eq!(short(5 * 86400 + 13 * 3600 + 59), "5d13h");
    }

    #[test]
    fn severity_thresholds() {
        assert_eq!(severity(69.9), 0);
        assert_eq!(severity(70.0), 1);
        assert_eq!(severity(85.0), 2);
        assert_eq!(severity(95.0), 3);
    }

    #[test]
    fn terminal_line_without_color() {
        let now = 1_000_000;
        let state = ProviderState {
            windows: vec![
                UsageWindow { duration_mins: Some(10080), used_percent: Some(16.0), resets_at: Some(now + 86400 + 4 * 3600) },
                UsageWindow { duration_mins: Some(300), used_percent: Some(24.0), resets_at: Some(now + 3600 + 13 * 60) },
            ],
            ..Default::default()
        };
        assert_eq!(terminal_line("CC", &state, now, false), "CC  5h 76% (1h13m) · W 84% (1d4h)");

        // Reset at now - 60: read before it, unknown; read after it with no newer window, unused.
        let past_reset = vec![UsageWindow { duration_mins: Some(300), used_percent: Some(50.0), resets_at: Some(now - 60) }];
        let read_before = ProviderState { windows: past_reset.clone(), updated_at_ms: (now - 120) * 1000, ..Default::default() };
        assert_eq!(terminal_line("CC", &read_before, now, false), "CC  5h —");
        let read_after = ProviderState { windows: past_reset, updated_at_ms: (now - 30) * 1000, ..Default::default() };
        assert_eq!(terminal_line("CC", &read_after, now, false), "CC  5h 100%");

        let not_started = ProviderState {
            windows: vec![UsageWindow { duration_mins: Some(300), used_percent: Some(0.0), resets_at: None }],
            ..Default::default()
        };
        assert_eq!(terminal_line("CC", &not_started, now, false), "CC  5h 100%");

        let unknown = ProviderState {
            windows: vec![UsageWindow { duration_mins: Some(300), used_percent: None, resets_at: Some(now + 60) }],
            ..Default::default()
        };
        assert_eq!(terminal_line("CC", &unknown, now, false), "CC  5h —");
        assert_eq!(terminal_line("CO", &ProviderState::with_status("확인 중…"), now, false), "CO  확인 중…");
    }
}
