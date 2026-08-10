use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "captastic",
    version,
    about = "Fast native screenshot capture for Windows"
)]
pub struct Cli {
    /// Persistent log file (defaults to %USERPROFILE%\.captastic\logs\captastic.log on Windows).
    #[arg(long, global = true)]
    pub log_file: Option<PathBuf>,
    /// Persistent logging threshold.
    #[arg(
        long,
        global = true,
        value_parser = ["off", "error", "warn", "info", "debug", "trace"]
    )]
    pub log_level: Option<String>,
    /// Persistent log line format.
    #[arg(long, global = true, value_parser = ["compact", "json"])]
    pub log_format: Option<String>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Daemon(DaemonArgs),
    Status {
        #[arg(long)]
        json: bool,
    },
    Stop,
    Displays {
        #[arg(long, default_value = "fake")]
        backend: String,
        #[arg(long)]
        json: bool,
    },
    Capture(CaptureArgs),
    Benchmark(BenchmarkArgs),
    Doctor {
        #[arg(long)]
        json: bool,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

impl Default for Command {
    fn default() -> Self {
        Self::Daemon(DaemonArgs::default())
    }
}

#[derive(Debug, Args, Default)]
pub struct DaemonArgs {
    /// Configuration file (defaults to %USERPROFILE%\.captastic\captastic.toml when present).
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long)]
    pub backend: Option<String>,
    #[arg(long, value_enum)]
    pub mode: Option<ModeArg>,
    #[arg(long)]
    pub fresh_timeout_ms: Option<u64>,
    #[arg(long)]
    pub max_frame_age_ms: Option<u64>,
    #[arg(long, action = clap::ArgAction::Set)]
    pub cpu_frame: Option<bool>,
    #[arg(long, action = clap::ArgAction::Set)]
    pub clipboard: Option<bool>,
    #[arg(long, action = clap::ArgAction::Set)]
    pub selection: Option<bool>,
    #[arg(long)]
    pub max_captures: Option<usize>,
    #[arg(long)]
    pub self_trigger: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ModeArg {
    Fresh,
    Latest,
}

#[derive(Debug, Args)]
pub struct CaptureArgs {
    #[arg(long, default_value = "fake")]
    pub backend: String,
    #[arg(long, value_enum, default_value_t = ModeArg::Latest)]
    pub mode: ModeArg,
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    pub cpu_frame: bool,
    #[arg(long, action = clap::ArgAction::Set, default_value_t = false)]
    pub selection: bool,
    #[arg(long, action = clap::ArgAction::Set, default_value_t = false)]
    pub clipboard: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct BenchmarkArgs {
    #[arg(long, default_value = "fake")]
    pub backend: String,
    #[arg(long, value_enum, default_value_t = ModeArg::Latest)]
    pub mode: ModeArg,
    #[arg(long, default_value_t = 100)]
    pub iterations: usize,
    #[arg(long, default_value_t = 10)]
    pub warmup: usize,
    #[arg(long, default_value_t = 250)]
    pub native_delay_us: u64,
    #[arg(long, default_value_t = 250)]
    pub readback_delay_us: u64,
    #[arg(long, default_value_t = 1000)]
    pub frame_age_us: u64,
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    pub cpu_frame: bool,
    #[arg(long)]
    pub output_results: Option<PathBuf>,
    #[arg(long)]
    pub raw_events: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    Show {
        #[arg(long)]
        path: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    Validate {
        #[arg(long)]
        path: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_arguments_resolve_to_the_desktop_daemon() {
        let cli = Cli::try_parse_from(["captastic"]).expect("zero-argument desktop launch");
        assert!(matches!(
            cli.command.unwrap_or_default(),
            Command::Daemon(_)
        ));
    }

    #[test]
    fn explicit_commands_remain_available() {
        let cli =
            Cli::try_parse_from(["captastic", "status", "--json"]).expect("explicit CLI command");
        assert!(matches!(cli.command, Some(Command::Status { json: true })));
    }
}
