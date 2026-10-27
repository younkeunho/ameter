//! Small shared helpers: time, files, logging, JSON access, UTF-16 strings.

use serde_json::{Map, Value};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn now_sec() -> i64 {
    now_ms() / 1000
}

/// Local wall clock as (year, month, day, hour, minute, second, millisecond).
pub fn local_time() -> (u16, u16, u16, u16, u16, u16, u16) {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut t: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut t) };
    (t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds)
}

/// Text of a file that may be replaced concurrently; None when missing or unreadable.
pub fn read_text(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    Some(String::from_utf8_lossy(bytes).into_owned())
}

pub fn read_json_object(path: &Path) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(&read_text(path)?) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// Writes to a temporary file beside the target, then swaps it in (readers never see half a file).
/// Retries briefly while another program holds the target without delete sharing.
pub fn write_atomic(path: &Path, content: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_file_name(format!(
        "{}.{}.{}.tmp",
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        std::process::id(),
        now_ms()
    ));
    fs::write(&tmp, content)?;
    let mut attempt = 0;
    loop {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(_) if attempt < 20 => {
                attempt += 1;
                thread::sleep(Duration::from_millis(15));
            }
            Err(_) => {
                let copied = fs::copy(&tmp, path).map(|_| ());
                let _ = fs::remove_file(&tmp);
                return copied;
            }
        }
    }
}

pub fn write_json(path: &Path, value: &Value) -> std::io::Result<()> {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".into());
    text.push('\n');
    write_atomic(path, text.as_bytes())
}

pub fn remove_file_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------- logging

static LOG: Mutex<&'static str> = Mutex::new("ameter");

/// Selects the log file (`<data dir>/<name>.log`) for this process.
pub fn set_log_name(name: &'static str) {
    if let Ok(mut current) = LOG.lock() {
        *current = name;
    }
}

/// Appends one timestamped line. Never fails the caller; rotates at 512 KiB.
pub fn log(message: &str) {
    let Ok(name) = LOG.lock() else { return };
    let path = crate::paths::log_file(&name);
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if fs::metadata(&path).map(|m| m.len() > 512 * 1024).unwrap_or(false) {
        let old = path.with_extension("log.old");
        let _ = fs::remove_file(&old);
        let _ = fs::rename(&path, &old);
    }
    let (y, mo, d, h, mi, s, ms) = local_time();
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{ms:03} {message}");
    }
}

// ---------------------------------------------------------------- JSON access

pub trait JsonExt {
    fn obj(&self, key: &str) -> Option<&Map<String, Value>>;
    fn str_of(&self, key: &str) -> Option<&str>;
    fn f64_of(&self, key: &str) -> Option<f64>;
    fn i64_of(&self, key: &str) -> Option<i64>;
    fn bool_of(&self, key: &str) -> Option<bool>;
}

impl JsonExt for Map<String, Value> {
    fn obj(&self, key: &str) -> Option<&Map<String, Value>> {
        self.get(key)?.as_object()
    }

    fn str_of(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }

    fn f64_of(&self, key: &str) -> Option<f64> {
        self.get(key)?.as_f64()
    }

    /// Integral numbers; a fractional number is truncated.
    fn i64_of(&self, key: &str) -> Option<i64> {
        let value = self.get(key)?;
        value.as_i64().or_else(|| value.as_f64().map(|f| f as i64))
    }

    fn bool_of(&self, key: &str) -> Option<bool> {
        self.get(key)?.as_bool()
    }
}

pub fn number(value: f64) -> Value {
    serde_json::Number::from_f64(value).map(Value::Number).unwrap_or(Value::Null)
}

pub fn opt_i64(value: Option<i64>) -> Value {
    value.map(Value::from).unwrap_or(Value::Null)
}

pub fn opt_f64(value: Option<f64>) -> Value {
    value.map(number).unwrap_or(Value::Null)
}

// ---------------------------------------------------------------- Win32 strings

/// NUL-terminated UTF-16 for Win32 calls.
pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// First line of a tool's output without an "Error:" prefix, shortened for logs and status text.
pub fn first_line(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line.strip_prefix("Error:").map(str::trim).unwrap_or(line);
    Some(line.chars().take(160).collect())
}
