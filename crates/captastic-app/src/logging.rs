#[cfg(windows)]
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use captastic_config::LoggingConfig;
use log::{Level, LevelFilter, Log, Metadata, Record};
use serde_json::json;

const LOG_QUEUE_CAPACITY: usize = 2_048;
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

#[derive(Clone, Copy)]
enum LineFormat {
    Compact,
    Json,
}

struct AsyncFileLogger {
    level: LevelFilter,
    sender: SyncSender<LogMessage>,
}

struct ConsoleLogger {
    level: LevelFilter,
    format: LineFormat,
}

enum LogMessage {
    Entry(LogEntry),
    Flush(SyncSender<()>),
}

struct LogEntry {
    unix_micros: u128,
    level: Level,
    target: String,
    thread: String,
    message: String,
}

struct RotatingWriter {
    path: PathBuf,
    writer: Option<BufWriter<File>>,
    bytes_written: u64,
    max_file_bytes: u64,
    retained_files: usize,
}

impl RotatingWriter {
    fn new(path: PathBuf, max_file_bytes: u64, retained_files: usize) -> std::io::Result<Self> {
        let file = open_log_file(&path)?;
        let bytes_written = file.metadata()?.len();
        let mut writer = Self {
            path,
            writer: Some(BufWriter::new(file)),
            bytes_written,
            max_file_bytes,
            retained_files,
        };
        if writer.bytes_written >= writer.max_file_bytes {
            writer.rotate()?;
        }
        Ok(writer)
    }

    fn write_line(&mut self, line: &str, console_line: &str) {
        let mut console = anstream::stderr();
        let _ = writeln!(console, "{console_line}");
        let line_bytes = u64::try_from(line.len().saturating_add(1)).unwrap_or(u64::MAX);
        if self.bytes_written != 0
            && self.bytes_written.saturating_add(line_bytes) > self.max_file_bytes
        {
            let _ = self.rotate();
        }
        let Some(writer) = self.writer.as_mut() else {
            return;
        };
        if writeln!(writer, "{line}").is_ok() {
            self.bytes_written = self.bytes_written.saturating_add(line_bytes);
        }
        let _ = writer.flush();
    }

    fn flush(&mut self) {
        if let Some(writer) = self.writer.as_mut() {
            let _ = writer.flush();
        }
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        if let Some(mut writer) = self.writer.take() {
            writer.flush()?;
        }
        let rotation_result = self.rotate_archives();
        let file = open_log_file(&self.path)?;
        self.bytes_written = file.metadata()?.len();
        self.writer = Some(BufWriter::new(file));
        rotation_result
    }

    fn rotate_archives(&self) -> std::io::Result<()> {
        for index in (1..=self.retained_files).rev() {
            let source = if index == 1 {
                self.path.clone()
            } else {
                archive_path(&self.path, index - 1)
            };
            if !source.exists() {
                continue;
            }
            let destination = archive_path(&self.path, index);
            if destination.exists() {
                fs::remove_file(&destination)?;
            }
            fs::rename(source, destination)?;
        }
        Ok(())
    }
}

impl Log for AsyncFileLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let current_thread = thread::current();
        let entry = LogEntry {
            unix_micros: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_micros()),
            level: record.level(),
            target: record.target().to_owned(),
            thread: current_thread.name().unwrap_or("unnamed").to_owned(),
            message: record.args().to_string(),
        };
        // Logging must never block capture or UI threads. A saturated queue drops diagnostics
        // instead of introducing file I/O or backpressure on the hotkey-to-frame path.
        let _ = self.sender.try_send(LogMessage::Entry(entry));
    }

    fn flush(&self) {
        let (acknowledge, completed) = mpsc::sync_channel(0);
        if self.sender.send(LogMessage::Flush(acknowledge)).is_ok() {
            let _ = completed.recv_timeout(Duration::from_millis(250));
        }
    }
}

impl Log for ConsoleLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let current_thread = thread::current();
        let entry = LogEntry {
            unix_micros: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_micros()),
            level: record.level(),
            target: record.target().to_owned(),
            thread: current_thread.name().unwrap_or("unnamed").to_owned(),
            message: record.args().to_string(),
        };
        let mut console = anstream::stderr();
        let _ = writeln!(console, "{}", format_console_entry(self.format, &entry));
    }

    fn flush(&self) {
        let _ = anstream::stderr().flush();
    }
}

pub fn init(config: &LoggingConfig) -> Result<PathBuf, String> {
    let level = parse_level(&config.level)?;
    let format = match config.format.as_str() {
        "compact" => LineFormat::Compact,
        "json" => LineFormat::Json,
        value => return Err(format!("unsupported log format {value}")),
    };
    let path = match config.file.clone() {
        Some(path) => path,
        None => default_log_path()?,
    };
    if config.max_file_bytes == 0 {
        return Err("log size limit must be greater than zero".to_owned());
    }
    if config.retained_files == 0 {
        return Err("retained log file count must be greater than zero".to_owned());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "failed to create log directory {}: {error}",
                parent.display()
            )
        })?;
    }
    let writer = RotatingWriter::new(path.clone(), config.max_file_bytes, config.retained_files)
        .map_err(|error| format!("failed to initialize log file {}: {error}", path.display()))?;
    let (sender, receiver) = mpsc::sync_channel(LOG_QUEUE_CAPACITY);
    thread::Builder::new()
        .name("captastic-log".to_owned())
        .spawn(move || {
            let mut writer = writer;
            while let Ok(message) = receiver.recv() {
                match message {
                    LogMessage::Entry(entry) => {
                        let line = format_entry(format, &entry);
                        let console_line = format_console_entry(format, &entry);
                        writer.write_line(&line, &console_line);
                    }
                    LogMessage::Flush(acknowledge) => {
                        writer.flush();
                        let _ = acknowledge.send(());
                    }
                }
            }
            writer.flush();
        })
        .map_err(|error| format!("failed to start log writer: {error}"))?;
    log::set_boxed_logger(Box::new(AsyncFileLogger { level, sender }))
        .map_err(|error| format!("failed to install logger: {error}"))?;
    log::set_max_level(level);
    let _ = LOG_PATH.set(path.clone());
    Ok(path)
}

pub fn init_console(config: &LoggingConfig) -> Result<(), String> {
    let level = parse_level(&config.level)?;
    let format = match config.format.as_str() {
        "compact" => LineFormat::Compact,
        "json" => LineFormat::Json,
        value => return Err(format!("unsupported log format {value}")),
    };
    log::set_boxed_logger(Box::new(ConsoleLogger { level, format }))
        .map_err(|error| format!("failed to install logger: {error}"))?;
    log::set_max_level(level);
    Ok(())
}

#[cfg(windows)]
pub fn path() -> Option<&'static Path> {
    LOG_PATH.get().map(PathBuf::as_path)
}

#[cfg(windows)]
pub fn error(arguments: fmt::Arguments<'_>) {
    log::error!("{arguments}");
}

#[cfg(windows)]
pub fn warn(arguments: fmt::Arguments<'_>) {
    log::warn!("{arguments}");
}

fn parse_level(value: &str) -> Result<LevelFilter, String> {
    match value {
        "off" => Ok(LevelFilter::Off),
        "error" => Ok(LevelFilter::Error),
        "warn" => Ok(LevelFilter::Warn),
        "info" => Ok(LevelFilter::Info),
        "debug" => Ok(LevelFilter::Debug),
        "trace" => Ok(LevelFilter::Trace),
        _ => Err(format!("unsupported log level {value}")),
    }
}

fn default_log_path() -> Result<PathBuf, String> {
    captastic_config::storage_directory()
        .ok_or_else(|| {
            "unable to determine the user home directory from USERPROFILE or HOME".to_owned()
        })
        .map(|path| path.join("logs").join("captastic.log"))
}

fn open_log_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn archive_path(path: &Path, index: usize) -> PathBuf {
    let mut archived = path.as_os_str().to_os_string();
    archived.push(format!(".{index}"));
    PathBuf::from(archived)
}

fn format_entry(format: LineFormat, entry: &LogEntry) -> String {
    let timestamp = format_utc_timestamp(entry.unix_micros);
    match format {
        LineFormat::Compact => format!(
            "{} {} {}: {}",
            timestamp, entry.level, entry.target, entry.message
        ),
        LineFormat::Json => json!({
            "timestamp": timestamp,
            "level": entry.level.as_str().to_ascii_lowercase(),
            "thread": entry.thread,
            "target": entry.target,
            "message": entry.message,
        })
        .to_string(),
    }
}

fn format_console_entry(format: LineFormat, entry: &LogEntry) -> String {
    if matches!(format, LineFormat::Json) {
        return format_entry(format, entry);
    }
    let timestamp = format_utc_timestamp(entry.unix_micros);
    let level_color = match entry.level {
        Level::Error => "1;91",
        Level::Warn => "1;93",
        Level::Info => "1;92",
        Level::Debug => "1;94",
        Level::Trace => "35",
    };
    format!(
        "\u{1b}[90m{timestamp}\u{1b}[0m \u{1b}[{level_color}m{}\u{1b}[0m \u{1b}[36m{}\u{1b}[0m: {}",
        entry.level, entry.target, entry.message
    )
}

fn format_utc_timestamp(unix_micros: u128) -> String {
    let seconds = i64::try_from(unix_micros / 1_000_000).unwrap_or(i64::MAX);
    let micros = (unix_micros % 1_000_000) as u32;
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    let (year, month, day) = civil_date_from_unix_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}Z")
}

fn civil_date_from_unix_days(days: i64) -> (i64, i64, i64) {
    // Howard Hinnant's civil-from-days algorithm, with day zero at 1970-01-01.
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> LogEntry {
        LogEntry {
            unix_micros: 1_786_325_894_402_172,
            level: Level::Warn,
            target: "captastic::test".to_owned(),
            thread: "worker".to_owned(),
            message: "sample message".to_owned(),
        }
    }

    #[test]
    fn validates_supported_levels() {
        assert_eq!(parse_level("debug"), Ok(LevelFilter::Debug));
        assert!(parse_level("verbose").is_err());
    }

    #[test]
    fn compact_log_contains_operational_context() {
        assert_eq!(
            format_entry(LineFormat::Compact, &entry()),
            "2026-08-10T01:38:14.402172Z WARN captastic::test: sample message"
        );
    }

    #[test]
    fn compact_console_log_colorizes_context() {
        assert_eq!(
            format_console_entry(LineFormat::Compact, &entry()),
            "\u{1b}[90m2026-08-10T01:38:14.402172Z\u{1b}[0m \u{1b}[1;93mWARN\u{1b}[0m \u{1b}[36mcaptastic::test\u{1b}[0m: sample message"
        );
        assert!(!format_console_entry(LineFormat::Json, &entry()).contains('\u{1b}'));
    }

    #[test]
    fn json_log_is_machine_readable() {
        let value: serde_json::Value =
            serde_json::from_str(&format_entry(LineFormat::Json, &entry())).expect("JSON log");
        assert_eq!(value["timestamp"], "2026-08-10T01:38:14.402172Z");
        assert_eq!(value["level"], "warn");
        assert_eq!(value["message"], "sample message");
    }

    #[test]
    fn utc_timestamp_handles_epoch_and_leap_day() {
        assert_eq!(format_utc_timestamp(0), "1970-01-01T00:00:00.000000Z");
        assert_eq!(
            format_utc_timestamp(1_709_164_800_000_001),
            "2024-02-29T00:00:00.000001Z"
        );
    }

    #[test]
    fn rotates_at_the_size_limit_and_retains_three_archives() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("current time")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "captastic-log-rotation-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("test log directory");
        let path = directory.join("captastic.log");
        let mut writer = RotatingWriter::new(path.clone(), 10, 3).expect("rotating writer");

        for line in ["12345678", "abcdefgh", "ABCDEFGH", "87654321"] {
            writer.write_line(line, line);
        }
        writer.flush();

        assert_eq!(fs::read_to_string(&path).expect("active log"), "87654321\n");
        assert_eq!(
            fs::read_to_string(archive_path(&path, 1)).expect("first archive"),
            "ABCDEFGH\n"
        );
        assert_eq!(
            fs::read_to_string(archive_path(&path, 2)).expect("second archive"),
            "abcdefgh\n"
        );
        assert_eq!(
            fs::read_to_string(archive_path(&path, 3)).expect("third archive"),
            "12345678\n"
        );
        assert!(!archive_path(&path, 4).exists());

        drop(writer);
        fs::remove_dir_all(directory).expect("remove test log directory");
    }
}
