//! The shipped executable: Captastic in the notification area, and nothing else.
//!
//! Built for the Windows subsystem, so Windows never gives it a console - not at login, not from
//! the Start Menu, not from a shortcut. That is the whole reason the 0.2 `captastic-desktop.exe`
//! launcher existed, and running the daemon here in-process retires it. The cost is that this
//! program has no stdout a person can see: anything it must say before the log is open, or when it
//! cannot run at all, it says in a dialog.

use std::path::PathBuf;
use std::process;

use clap::Parser;

use crate::build_info;
use crate::cli::{Cli, Command, DaemonArgs};

/// The options the shipped executable accepts: the daemon's own, and where and how it logs.
///
/// Deliberately not the developer command line. `captastic.exe capture` would otherwise parse, run
/// with no console to report into, and leave a user wondering what happened.
#[derive(Debug, Parser)]
#[command(
    name = "captastic",
    version = build_info::BUILD_VERSION,
    about = "Captastic screenshot capture. Runs in the notification area; the developer command \
             line is captastic-dev, built from the source repository."
)]
struct DesktopCli {
    /// Persistent log file (defaults to %USERPROFILE%\.captastic\logs\captastic.log).
    #[arg(long)]
    log_file: Option<PathBuf>,
    /// Persistent logging threshold.
    #[arg(long, value_parser = ["off", "error", "warn", "info", "debug", "trace"])]
    log_level: Option<String>,
    /// Persistent log line format.
    #[arg(long, value_parser = ["compact", "json"])]
    log_format: Option<String>,
    #[command(flatten)]
    daemon: DaemonArgs,
}

pub fn main() {
    let desktop = match DesktopCli::try_parse() {
        Ok(desktop) => desktop,
        Err(error) => {
            // `--help` and `--version` arrive here too, as "errors" that exit zero. Either way the
            // text is the answer, and a dialog is the only place it can be read.
            let text = error.render().to_string();
            if error.use_stderr() {
                captastic_windows::show_error_dialog(&text);
            } else {
                captastic_windows::show_info_dialog(&text);
            }
            process::exit(error.exit_code());
        }
    };
    // A second start - the Start Menu item clicked while Captastic is already in the notification
    // area - has nothing to do. Asked before the log is opened so it adds no line to the running
    // daemon's log; the daemon's own control event still refuses a real race further in.
    if captastic_windows::DaemonControl::is_running() {
        return;
    }
    let cli = Cli {
        log_file: desktop.log_file,
        log_level: desktop.log_level,
        log_format: desktop.log_format,
        command: Some(Command::Daemon(desktop.daemon)),
    };
    let logging_available = crate::init_logging(&cli, true);
    let Some(Command::Daemon(args)) = cli.command else {
        unreachable!("the desktop executable only ever builds a daemon command");
    };
    if let Err(error) = crate::daemon::run(args) {
        if logging_available {
            log::error!("captastic daemon failed: {error}");
            log::logger().flush();
        }
        // Without this a daemon that cannot start - an invalid configuration, a hotkey another
        // program holds - would vanish at login with nothing on screen to say it had tried.
        captastic_windows::show_error_dialog(&format!("Captastic stopped.\n\n{error}"));
        process::exit(error.exit_code());
    }
    if logging_available {
        log::info!("captastic daemon finished successfully");
        log::logger().flush();
    }
}
