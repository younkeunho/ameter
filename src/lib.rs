//! ameter: a two-line taskbar meter for Codex, Claude and Higgsfield usage on Windows.
//!
//! * `ameter.exe` draws the meter on the taskbar and polls the providers.
//! * `ameter-cli.exe` is the Claude Code statusLine command and the setup tool.

pub mod autostart;
pub mod claude_setup;
pub mod claude_web;
pub mod codex;
pub mod fmt;
pub mod higgsfield;
pub mod model;
pub mod paths;
pub mod poller;
pub mod process;
pub mod statusline;
pub mod ui;
pub mod util;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
