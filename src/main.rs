//! i3_kb_switch: remembers the keyboard layout per i3 window.
//!
//! Usage: `i3_kb_switch [-v | --verbose]`, `i3_kb_switch pane <pane_id>`
//! or `i3_kb_switch --version`.

use i3_kb_switch::{daemon, sock};

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut verbose = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "pane" => {
                // tmux hooks forward `pane <pane_id>` on focus change;
                // an absent daemon or a bad id is routine — never fail.
                if let Some(pane) = args.next().as_deref().and_then(sock::parse_pane_id)
                    && let Ok(path) = daemon::socket_path()
                {
                    sock::send_pane_event(&path, pane);
                }
                return ExitCode::SUCCESS;
            }
            "-v" | "--verbose" => verbose = true,
            "--version" => {
                println!("i3_kb_switch {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            _ => {
                eprintln!("usage: i3_kb_switch [-v | --verbose]");
                eprintln!("       i3_kb_switch pane <pane_id>");
                eprintln!("       i3_kb_switch --version");
                return ExitCode::from(2);
            }
        }
    }

    match daemon::run(daemon::RunOpts { verbose }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("i3_kb_switch: {err}");
            ExitCode::FAILURE
        }
    }
}
