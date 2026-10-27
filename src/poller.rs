//! Background refresh loop shared by the providers.

use crate::util;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub trait Source: Send + 'static {
    type Output: Send + 'static;
    const NAME: &'static str;
    const INTERVAL: Duration;
    fn poll(&mut self) -> Self::Output;
}

/// Runs `poll` at start, then every interval or on `refresh_now`. A paused poller skips polls
/// (hidden providers are not queried); resuming polls at once.
pub struct Poller {
    wake: Mutex<Sender<()>>,
    paused: Arc<AtomicBool>,
}

impl Poller {
    pub fn start<S: Source>(mut source: S, on_update: impl Fn(S::Output) + Send + 'static) -> Poller {
        let (wake, signal) = mpsc::channel::<()>();
        let paused = Arc::new(AtomicBool::new(false));
        let paused_in_loop = paused.clone();
        thread::Builder::new()
            .name(S::NAME.into())
            .spawn(move || {
                loop {
                    if !paused_in_loop.load(Ordering::SeqCst) {
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source.poll())) {
                            Ok(output) => on_update(output),
                            Err(_) => util::log(&format!("{} panicked", S::NAME)),
                        }
                    }
                    match signal.recv_timeout(S::INTERVAL) {
                        Ok(()) => while signal.try_recv().is_ok() {},
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .expect("spawn poller thread");
        Poller { wake: Mutex::new(wake), paused }
    }

    pub fn refresh_now(&self) {
        if let Ok(wake) = self.wake.lock() {
            let _ = wake.send(());
        }
    }

    pub fn set_paused(&self, paused: bool) {
        let was = self.paused.swap(paused, Ordering::SeqCst);
        if was && !paused {
            self.refresh_now();
        }
    }
}
