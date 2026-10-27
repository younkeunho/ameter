//! Child processes that never outlive ameter and never flash a console window.

use std::io::{Read, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, WaitForSingleObject};

/// Every process assigned here (and its children) is killed when the job is dropped, including
/// when ameter itself exits or crashes.
pub struct KillOnDropJob(HANDLE);

// SAFETY: a job handle is a kernel object handle; it may be used and closed from any thread.
unsafe impl Send for KillOnDropJob {}

impl KillOnDropJob {
    pub fn new() -> KillOnDropJob {
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if !handle.is_null() {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
            }
        }
        KillOnDropJob(handle)
    }

    pub fn add(&self, child: &Child) {
        if !self.0.is_null() {
            unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) };
        }
    }
}

impl Default for KillOnDropJob {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for KillOnDropJob {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// A command with piped stdio, no console window, started in `cwd`.
pub fn command(exe: &Path, cwd: &Path) -> Command {
    let _ = std::fs::create_dir_all(cwd);
    let mut cmd = Command::new(exe);
    cmd.current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Starts the command inside a kill-on-drop job.
pub fn spawn(mut cmd: Command) -> std::io::Result<(Child, KillOnDropJob)> {
    let job = KillOnDropJob::new();
    let child = cmd.spawn()?;
    job.add(&child);
    Ok((child, job))
}

/// Waits for exit; false on timeout.
pub fn wait_timeout(child: &Child, timeout: Duration) -> bool {
    let handle = child.as_raw_handle() as HANDLE;
    unsafe { WaitForSingleObject(handle, timeout.as_millis().min(u32::MAX as u128 - 1) as u32) == WAIT_OBJECT_0 }
}

pub struct Output {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Runs to completion with `input` on stdin. On timeout the process tree is killed and
/// `Err(TimedOut)` is returned.
pub fn run(cmd: Command, input: &[u8], timeout: Duration) -> std::io::Result<Output> {
    let (mut child, job) = spawn(cmd)?;
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    if let Some(mut stdin) = child.stdin.take() {
        // The child may ignore stdin and exit early; a broken pipe is not an error here.
        let _ = stdin.write_all(input);
    }

    if !wait_timeout(&child, timeout) {
        drop(job);
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "응답 시간 초과"));
    }
    let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
    // Grandchildren holding the pipes open are killed with the job before collecting output.
    drop(job);
    let collect = |h: Option<thread::JoinHandle<Vec<u8>>>| h.and_then(|h| h.join().ok()).unwrap_or_default();
    Ok(Output { code, stdout: collect(stdout), stderr: collect(stderr) })
}

fn drain<R: Read + Send + 'static>(mut reader: R) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = reader.read_to_end(&mut buffer);
        buffer
    })
}
