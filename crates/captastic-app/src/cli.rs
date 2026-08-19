use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::build_info;

#[derive(Debug, Parser)]
#[command(
    name = "captastic",
    version = build_info::BUILD_VERSION,
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
    /// Report the exact source and build identity.
    Version {
        #[arg(long)]
        json: bool,
    },
    Doctor {
        #[arg(long)]
        json: bool,
    },
    Startup {
        #[command(subcommand)]
        command: StartupCommand,
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
    /// Display policy: pointer, primary, virtual_desktop, or display:<persistent-id>.
    #[arg(long)]
    pub display: Option<String>,
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
    /// Repeat the self-trigger every N milliseconds, for soak runs. Pair with --max-captures.
    ///
    /// Observed on the daemon's event loop, so the interval is a floor rather than a cadence.
    #[arg(long, requires = "self_trigger")]
    pub self_trigger_interval_ms: Option<u64>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ModeArg {
    Fresh,
    Latest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum PreviewArg {
    Auto,
    Live,
    Frozen,
}

#[derive(Debug, Args)]
pub struct CaptureArgs {
    #[arg(long, default_value = "fake")]
    pub backend: String,
    /// Display policy: pointer, primary, virtual_desktop, or display:<persistent-id>.
    #[arg(long, default_value = "primary")]
    pub display: String,
    #[arg(long, value_enum, default_value_t = ModeArg::Latest)]
    pub mode: ModeArg,
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    pub cpu_frame: bool,
    #[arg(long, action = clap::ArgAction::Set, default_value_t = false)]
    pub selection: bool,
    /// Selection presenter: auto, live, or frozen.
    #[arg(long, value_enum, default_value_t = PreviewArg::Auto)]
    pub selection_preview: PreviewArg,
    #[arg(long, action = clap::ArgAction::Set, default_value_t = false)]
    pub clipboard: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct BenchmarkArgs {
    #[arg(long, default_value = "fake")]
    pub backend: String,
    /// Display policy: pointer, primary, virtual_desktop, or display:<persistent-id>.
    #[arg(long, default_value = "primary")]
    pub display: String,
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
    /// Reject retained frames older than this, in milliseconds. Zero accepts any age, which is the
    /// default and matches `latest` mode's documented behaviour.
    #[arg(long, default_value_t = 0)]
    pub max_frame_age_ms: u64,
    #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
    pub cpu_frame: bool,
    /// Composite the pointer into each capture. Milestone 5 asks for cursor-on and cursor-off to
    /// be measured separately; this is the switch between the two runs.
    #[arg(long, value_enum, default_value_t = CursorArg::Exclude)]
    pub cursor: CursorArg,
    /// Repeat the whole timed run this many times. Every repeat is an independent run against a
    /// fresh backend, and the comparison refuses to aggregate runs whose environments differ.
    #[arg(long, default_value_t = 1)]
    pub repeat: usize,
    /// Judge the run against a budget file. Budgets name the host they describe and are skipped,
    /// loudly, anywhere else — a GPU timing budget evaluated on a CI runner fails every time, and
    /// a check that always fails is one nobody reads.
    #[arg(long)]
    pub budgets: Option<PathBuf>,
    #[arg(long)]
    pub output_results: Option<PathBuf>,
    #[arg(long)]
    pub raw_events: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum CursorArg {
    Include,
    Exclude,
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

#[derive(Debug, Subcommand)]
pub enum StartupCommand {
    Enable,
    Disable,
    Status {
        #[arg(long)]
        json: bool,
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

    #[test]
    fn one_shot_selection_preview_is_explicit_and_defaults_to_auto() {
        let cli = Cli::try_parse_from(["captastic", "capture", "--selection", "true"])
            .expect("one-shot capture command");
        let Some(Command::Capture(args)) = cli.command else {
            panic!("capture command should be selected");
        };
        assert!(args.selection);
        assert_eq!(args.selection_preview, PreviewArg::Auto);

        let cli = Cli::try_parse_from([
            "captastic",
            "capture",
            "--selection",
            "true",
            "--selection-preview",
            "frozen",
        ])
        .expect("frozen one-shot capture command");
        let Some(Command::Capture(args)) = cli.command else {
            panic!("capture command should be selected");
        };
        assert_eq!(args.selection_preview, PreviewArg::Frozen);
    }

    #[test]
    fn version_command_supports_structured_output() {
        let cli = Cli::try_parse_from(["captastic", "version", "--json"])
            .expect("structured version command");
        assert!(matches!(cli.command, Some(Command::Version { json: true })));
    }

    #[test]
    fn startup_management_is_an_explicit_cli_workflow() {
        let cli = Cli::try_parse_from(["captastic", "startup", "status", "--json"])
            .expect("startup status command");
        assert!(matches!(
            cli.command,
            Some(Command::Startup {
                command: StartupCommand::Status { json: true }
            })
        ));
    }

    #[test]
    fn display_policy_can_override_daemon_configuration() {
        let cli = Cli::try_parse_from([
            "captastic",
            "daemon",
            "--display",
            "display:windows-monitor-0123456789abcdef",
        ])
        .expect("daemon display override");
        let Some(Command::Daemon(args)) = cli.command else {
            panic!("daemon command");
        };
        assert_eq!(
            args.display.as_deref(),
            Some("display:windows-monitor-0123456789abcdef")
        );
    }
}
