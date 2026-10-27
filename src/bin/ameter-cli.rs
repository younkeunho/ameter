//! ameter-cli: Claude Code statusLine command and setup tool.
//!
//!   claude-statusline                              cache rate limits, run the replaced statusline
//!   install-claude [--passthrough "<command>"]     wire ameter into ~/.claude/settings.json
//!   uninstall-claude                               restore the replaced statusline
//!   autostart on|off                               start ameter.exe at Windows sign-in
//!   print                                          print the current CO / HF / CC lines

use ameter::{autostart, claude_setup, fmt, model, paths, statusline, util};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("");

    if command == "claude-statusline" {
        util::set_log_name("statusline");
        // Never fail Claude Code's statusline, whatever happens.
        if std::panic::catch_unwind(statusline::run).is_err() {
            util::log("statusline panicked");
        }
        return ExitCode::SUCCESS;
    }

    util::set_log_name("cli");
    let result = match command {
        "install-claude" => claude_setup::install(&paths::cli_exe(), option(&args, "--passthrough")),
        "uninstall-claude" => claude_setup::uninstall(),
        "autostart" => match args.get(1).map(String::as_str) {
            Some(state @ ("on" | "off")) => autostart::set(state == "on", &paths::widget_exe())
                .map(|()| format!("자동 실행: {}", if state == "on" { "켬" } else { "끔" })),
            _ => return usage(),
        },
        "print" => {
            let now = util::now_sec();
            Ok([
                fmt::terminal_line("CO", &model::load_codex(), now, false),
                fmt::terminal_line("HF", &model::load_higgsfield(), now, false),
                fmt::terminal_line("CC", &model::load_claude(), now, false),
            ]
            .join("\n"))
        }
        _ => return usage(),
    };
    match result {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn option<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn usage() -> ExitCode {
    eprintln!("ameter-cli {}", ameter::VERSION);
    eprintln!("  claude-statusline | install-claude [--passthrough \"<command>\"] | uninstall-claude | autostart on|off | print");
    ExitCode::from(2)
}
