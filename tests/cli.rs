//! ameter-cli against throwaway AMETER_DATA_DIR / CLAUDE_CONFIG_DIR directories.

use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("ameter-test-{name}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(root.join("data")).unwrap();
        fs::create_dir_all(root.join("claude")).unwrap();
        Sandbox { root }
    }

    fn data(&self, file: &str) -> PathBuf {
        self.root.join("data").join(file)
    }

    fn settings(&self) -> PathBuf {
        self.root.join("claude").join("settings.json")
    }

    fn run(&self, args: &[&str], input: &[u8]) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ameter-cli"))
            .args(args)
            .env("AMETER_DATA_DIR", self.root.join("data"))
            .env("CLAUDE_CONFIG_DIR", self.root.join("claude"))
            .env_remove("AMETER_STATUSLINE_WRAPPED")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    fn statusline(&self, input: &Value) -> Output {
        self.run(&["claude-statusline"], input.to_string().as_bytes())
    }

    fn cache(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.data("claude.json")).unwrap()).unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

fn input(five_hour: (f64, i64), seven_day: (f64, i64)) -> Value {
    json!({
        "session_id": "s", "prompt": "never logged",
        "model": { "display_name": "Opus" },
        "rate_limits": {
            "five_hour": { "used_percentage": five_hour.0, "resets_at": five_hour.1 },
            "seven_day": { "used_percentage": seven_day.0, "resets_at": seven_day.1 }
        }
    })
}

fn git_bash() -> Option<&'static Path> {
    ["C:\\Program Files\\Git\\bin\\bash.exe", "C:\\Program Files (x86)\\Git\\bin\\bash.exe"]
        .into_iter()
        .map(Path::new)
        .find(|p| p.is_file())
}

#[test]
fn statusline_caches_rate_limits_and_prints_remaining_quota() {
    let sb = Sandbox::new("print");
    let out = sb.statusline(&input((24.0, now() + 3600), (41.0, now() + 86400)));
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    let plain: String =
        text.split('\x1b').enumerate().map(|(i, s)| if i == 0 { s } else { s.split_once('m').map(|p| p.1).unwrap_or("") }).collect();
    assert!(plain.contains("CC  5h 76%"), "{plain}");
    assert!(plain.contains("W 59%"), "{plain}");
    let lines: Vec<&str> = plain.lines().collect();
    assert!(lines[0].starts_with("CO") && lines[1].starts_with("HF") && lines[2].starts_with("CC"), "{plain}");

    let cache = sb.cache();
    assert_eq!(cache["windows"]["five_hour"]["usedPercent"].as_f64(), Some(24.0));
    assert_eq!(cache["source"], "Claude Code");
    assert!(!fs::read_to_string(sb.data("claude.json")).unwrap().contains("never logged"));
}

#[test]
fn input_without_rate_limits_leaves_the_cache_alone() {
    let sb = Sandbox::new("norl");
    let out = sb.statusline(&json!({ "session_id": "s", "model": { "display_name": "Opus" } }));
    assert!(out.status.success());
    assert!(!sb.data("claude.json").exists());
}

#[test]
fn windows_only_move_forward_across_sessions() {
    let sb = Sandbox::new("forward");
    let reset = now() + 3600;
    sb.statusline(&input((30.0, reset), (40.0, reset + 86400)));
    sb.statusline(&input((20.0, reset + 30), (40.0, reset + 86400)));
    assert_eq!(sb.cache()["windows"]["five_hour"]["usedPercent"].as_f64(), Some(30.0));
    sb.statusline(&input((2.0, reset + 18000), (41.0, reset + 86400)));
    let cache = sb.cache();
    assert_eq!(cache["windows"]["five_hour"]["usedPercent"].as_f64(), Some(2.0));
    assert_eq!(cache["windows"]["seven_day"]["usedPercent"].as_f64(), Some(41.0));
}

#[test]
fn malformed_input_never_fails_the_statusline() {
    let sb = Sandbox::new("garbage");
    for input in [&b"not json"[..], b"", b"[1,2]", b"{\"rate_limits\": 5}"] {
        assert!(sb.run(&["claude-statusline"], input).status.success());
    }
}

#[test]
fn install_wraps_the_statusline_keeps_other_settings_and_uninstall_restores_it() {
    let Some(_) = git_bash() else { return };
    let sb = Sandbox::new("install");
    let original = json!({ "type": "command", "command": "echo wrapped-ok", "padding": 0 });
    fs::write(sb.settings(), serde_json::to_string_pretty(&json!({ "model": "opus", "statusLine": original })).unwrap()).unwrap();

    let out = sb.run(&["install-claude"], b"");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let settings: Value = serde_json::from_str(&fs::read_to_string(sb.settings()).unwrap()).unwrap();
    assert_eq!(settings["model"], "opus");
    assert_eq!(settings["statusLine"]["padding"], 0);
    assert!(settings["statusLine"]["command"].as_str().unwrap().ends_with("ameter-cli.exe claude-statusline"));
    assert!(sb.root.join("data").join("backups").read_dir().unwrap().next().is_some());

    let out = sb.statusline(&input((10.0, now() + 3600), (10.0, now() + 86400)));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "wrapped-ok");
    assert!(sb.data("claude.json").exists());

    assert!(sb.run(&["uninstall-claude"], b"").status.success());
    let settings: Value = serde_json::from_str(&fs::read_to_string(sb.settings()).unwrap()).unwrap();
    assert_eq!(settings["statusLine"], original);
    assert!(!sb.data("statusline-passthrough.txt").exists());
}

#[test]
fn install_refuses_an_unparseable_settings_file() {
    let sb = Sandbox::new("broken");
    fs::write(sb.settings(), "{ not json").unwrap();
    let out = sb.run(&["install-claude"], b"");
    assert!(!out.status.success());
    assert_eq!(fs::read_to_string(sb.settings()).unwrap(), "{ not json");
}

#[test]
fn a_statusline_that_calls_ameter_again_does_not_recurse() {
    let Some(_) = git_bash() else { return };
    let sb = Sandbox::new("recurse");
    let cli = env!("CARGO_BIN_EXE_ameter-cli").replace('\\', "/");
    fs::write(sb.data("statusline-passthrough.txt"), format!("\"{cli}\" claude-statusline; echo outer-done")).unwrap();
    let out = sb.statusline(&input((10.0, now() + 3600), (10.0, now() + 86400)));
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "outer-done");
}
