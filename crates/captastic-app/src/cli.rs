use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::build_info;
use crate::error::AppError;

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
    /// Configuration file (defaults to %USERPROFILE%\.captastic\captastic.toml when present).
    ///
    /// One-shot captures read `[capture] cursor`, `[clipboard]`, `[output]`, and `[history]` from
    /// it, the same settings the daemon reads.
    #[arg(long)]
    pub config: Option<PathBuf>,
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

/// `benchmark`, which is either a run or one of the commands that works on runs already recorded.
///
/// `args_conflicts_with_subcommands` keeps every existing invocation parsing exactly as it did:
/// the run flags are still `benchmark`'s own, not a `benchmark run` subcommand's, so no script,
/// no README line and no operator's muscle memory had to change to make room for `compare`.
#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct BenchmarkArgs {
    #[command(subcommand)]
    pub command: Option<BenchmarkCommand>,
    #[command(flatten)]
    pub run: BenchmarkRunArgs,
}

#[derive(Debug, Subcommand)]
pub enum BenchmarkCommand {
    /// Compare a candidate run against a baseline measured on the same host.
    ///
    /// Either side may be a single `run-N.json` report or a whole `repeated.json` set.
    Compare {
        /// The accepted baseline: a `run-N.json` report or a `repeated.json` set.
        baseline: PathBuf,
        /// The run being judged, in the same two shapes.
        candidate: PathBuf,
        #[arg(long)]
        json: bool,
        /// How far a stage may move before it is called a change rather than noise.
        ///
        /// The default is just above the 1.7-6.6 % run-to-run spread this host has measured. The
        /// larger of the two sets' own measured spreads wins when it is wider than this, because a
        /// set that disagreed with itself by 12 % cannot detect an 8 % regression.
        #[arg(long, default_value_t = 7.0)]
        noise_percent: f64,
    },
}

#[derive(Debug, Args)]
pub struct BenchmarkRunArgs {
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
    /// Write every repeat's raw artifacts into this directory: `run-N.json` per run,
    /// `run-N.events.jsonl` when `--raw-events` is given, and a `repeated.json` set file.
    ///
    /// The directory is what a baseline is: `benchmark compare` reads it back, and a published
    /// figure whose supporting runs went to a console and were lost cannot be checked again.
    #[arg(long)]
    pub output_dir: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

impl BenchmarkRunArgs {
    /// Rejects the flag combinations clap cannot express, before any capture is taken.
    ///
    /// Checked up front rather than at the point of use, so a run that would produce no artifacts
    /// fails in the first millisecond rather than after two hundred timed captures.
    pub fn validate(&self) -> Result<(), AppError> {
        if self.repeat > 1 && self.raw_events.is_some() && self.output_dir.is_none() {
            // A repeat set has one event stream per run, and they cannot all be written to the
            // single path `--raw-events` names. Before this the flag was accepted and quietly
            // ignored, which is the failure worth being loud about: the operator believed the
            // evidence had been collected.
            return Err(AppError::InvalidArgument(
                "--raw-events with --repeat greater than 1 needs --output-dir: each run has its \
                 own event stream, and they are written there as run-N.events.jsonl"
                    .to_owned(),
            ));
        }
        Ok(())
    }
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
    use std::path::Path;

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
    fn a_one_shot_capture_can_be_pointed_at_a_configuration_file() {
        // `capture` read the default configuration and nothing else, so `--config` was accepted
        // nowhere and a capture could not be run against a file under test.
        let cli = Cli::try_parse_from([
            "captastic",
            "capture",
            "--config",
            "C:/tmp/captastic.toml",
            "--backend",
            "fake",
        ])
        .expect("capture accepts a configuration file");
        let Some(Command::Capture(args)) = cli.command else {
            panic!("expected a capture command");
        };
        assert_eq!(
            args.config.as_deref(),
            Some(Path::new("C:/tmp/captastic.toml"))
        );

        let cli = Cli::try_parse_from(["captastic", "capture"]).expect("capture without a config");
        let Some(Command::Capture(args)) = cli.command else {
            panic!("expected a capture command");
        };
        assert!(
            args.config.is_none(),
            "the default file is still the default"
        );
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
    fn raw_events_under_repeat_says_which_flag_is_missing() {
        // `--raw-events --repeat 3` used to be accepted and do nothing at all: `run_repeated`
        // dropped the events, so no file was written and no message said so. The operator's whole
        // reason for passing the flag was to keep that evidence.
        let cli = Cli::try_parse_from([
            "captastic",
            "benchmark",
            "--repeat",
            "3",
            "--raw-events",
            "events.jsonl",
        ])
        .expect("the combination parses; it is refused by validation, not by clap");
        let Some(Command::Benchmark(args)) = cli.command else {
            panic!("expected a benchmark command");
        };
        let args = args.run;
        let error = args.validate().expect_err("the combination is refused");
        assert!(
            error.to_string().contains("--output-dir"),
            "the refusal names the flag that would fix it: {error}"
        );

        // With a directory to write them into, the same combination is exactly what the claim
        // procedure asks an operator to run.
        let cli = Cli::try_parse_from([
            "captastic",
            "benchmark",
            "--repeat",
            "3",
            "--raw-events",
            "events.jsonl",
            "--output-dir",
            "C:/tmp/run",
        ])
        .expect("benchmark with an output directory");
        let Some(Command::Benchmark(args)) = cli.command else {
            panic!("expected a benchmark command");
        };
        assert_eq!(
            args.run.output_dir.as_deref(),
            Some(Path::new("C:/tmp/run"))
        );
        args.run.validate().expect("the combination is accepted");

        // A single run still writes its one event stream to the one path it was given.
        let cli = Cli::try_parse_from(["captastic", "benchmark", "--raw-events", "events.jsonl"])
            .expect("single-run raw events");
        let Some(Command::Benchmark(args)) = cli.command else {
            panic!("expected a benchmark command");
        };
        args.run
            .validate()
            .expect("a single run needs no directory");
    }

    #[test]
    fn benchmark_gained_a_subcommand_without_moving_its_own_flags() {
        // `args_conflicts_with_subcommands` is what makes this safe: the run flags stayed on
        // `benchmark` itself rather than moving to a `benchmark run` subcommand, so every script,
        // README line and budget-file comment that predates `compare` still parses.
        let cli = Cli::try_parse_from(["captastic", "benchmark", "--iterations", "5"])
            .expect("the pre-existing form still parses");
        let Some(Command::Benchmark(args)) = cli.command else {
            panic!("expected a benchmark command");
        };
        assert!(args.command.is_none(), "no subcommand was asked for");
        assert_eq!(args.run.iterations, 5);

        let cli = Cli::try_parse_from([
            "captastic",
            "benchmark",
            "compare",
            "a.json",
            "b.json",
            "--noise-percent",
            "3",
        ])
        .expect("the compare form parses");
        let Some(Command::Benchmark(args)) = cli.command else {
            panic!("expected a benchmark command");
        };
        let Some(BenchmarkCommand::Compare {
            baseline,
            candidate,
            json,
            noise_percent,
        }) = args.command
        else {
            panic!("expected a compare subcommand");
        };
        assert_eq!(baseline, Path::new("a.json"));
        assert_eq!(candidate, Path::new("b.json"));
        assert!(!json);
        assert!((noise_percent - 3.0).abs() < f64::EPSILON);

        // The default noise floor sits just above the run-to-run spread this host has measured.
        let cli = Cli::try_parse_from(["captastic", "benchmark", "compare", "a.json", "b.json"])
            .expect("compare without a noise floor");
        let Some(Command::Benchmark(args)) = cli.command else {
            panic!("expected a benchmark command");
        };
        let Some(BenchmarkCommand::Compare { noise_percent, .. }) = args.command else {
            panic!("expected a compare subcommand");
        };
        assert!((noise_percent - 7.0).abs() < f64::EPSILON);

        // Run flags and the subcommand are mutually exclusive rather than silently ignored.
        assert!(
            Cli::try_parse_from([
                "captastic",
                "benchmark",
                "--iterations",
                "5",
                "compare",
                "a.json",
                "b.json",
            ])
            .is_err(),
            "a run flag beside `compare` would do nothing and must not be accepted"
        );
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
