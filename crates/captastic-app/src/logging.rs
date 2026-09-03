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

/// How often a writer re-checks whether another process has rotated the file out from under it.
///
/// A `metadata` call every few hundred lines is far cheaper than the alternative — a long-running
/// daemon quietly appending to an archive nobody thinks to read.
const STALENESS_CHECK_LINES: u64 = 256;
/// How long a rotation waits for another process's rotation to finish. Rotation is a rename and a
/// reopen, so a wait this long only ever expires on a genuinely stuck holder.
const ROTATION_LOCK_TIMEOUT: Duration = Duration::from_millis(500);
/// A rotation lock older than this belonged to a process that died mid-rotation.
const ROTATION_LOCK_STALE_AFTER: Duration = Duration::from_secs(30);
const ROTATION_LOCK_RETRY_DELAY: Duration = Duration::from_millis(2);

struct RotatingWriter {
    path: PathBuf,
    writer: Option<BufWriter<File>>,
    bytes_written: u64,
    max_file_bytes: u64,
    retained_files: usize,
    /// Lines written since the last check for another process's rotation.
    lines_since_staleness_check: u64,
    /// Rotation failures since the last one was reported, so a persistent problem says so once
    /// rather than on every line.
    suppressed_rotation_failures: u64,
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
            lines_since_staleness_check: 0,
            suppressed_rotation_failures: 0,
        };
        if writer.bytes_written >= writer.max_file_bytes {
            writer.rotate_or_report();
        }
        Ok(writer)
    }

    fn write_line(&mut self, line: &str, console_line: &str) {
        let mut console = anstream::stderr();
        let _ = writeln!(console, "{console_line}");
        let line_bytes = u64::try_from(line.len().saturating_add(1)).unwrap_or(u64::MAX);
        self.lines_since_staleness_check = self.lines_since_staleness_check.saturating_add(1);
        if self.lines_since_staleness_check >= STALENESS_CHECK_LINES {
            self.lines_since_staleness_check = 0;
            self.reopen_if_rotated_by_another_process();
        }
        if self.bytes_written != 0
            && self.bytes_written.saturating_add(line_bytes) > self.max_file_bytes
        {
            self.rotate_or_report();
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

    /// Reopens the log when the file this writer holds is no longer the one at `path`.
    ///
    /// Captastic's daemon and its one-shot commands log to the same file. When one of them
    /// rotates, the other's handle follows the renamed file rather than the name — so it carries
    /// on appending to `captastic.log.1`, and its lines vanish from the log anyone reads. Nothing
    /// in the handle says this has happened, so it is inferred: a file at `path` shorter than what
    /// this writer believes it has written is not the file it was writing.
    fn reopen_if_rotated_by_another_process(&mut self) {
        let on_disk = fs::metadata(&self.path).map(|metadata| metadata.len());
        let replaced = match on_disk {
            Ok(length) => length < self.bytes_written,
            // The name is gone entirely: unlinked, or renamed with nothing put back yet.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        };
        if !replaced {
            return;
        }
        if let Some(mut writer) = self.writer.take() {
            let _ = writer.flush();
        }
        match open_log_file(&self.path).and_then(|file| {
            let length = file.metadata()?.len();
            Ok((file, length))
        }) {
            Ok((file, length)) => {
                self.bytes_written = length;
                self.writer = Some(BufWriter::new(file));
            }
            Err(error) => {
                report_log_failure(format_args!(
                    "failed to reopen {} after another process rotated it: {error}",
                    self.path.display()
                ));
            }
        }
    }

    /// Rotates, reporting a failure to stderr rather than discarding it.
    ///
    /// The logger cannot log its own failures — it *is* the logger — so a swallowed rotation error
    /// meant a log that had silently stopped rotating, discovered only when the disk filled.
    fn rotate_or_report(&mut self) {
        match self.rotate() {
            Ok(()) => {
                if self.suppressed_rotation_failures > 0 {
                    report_log_failure(format_args!(
                        "log rotation for {} recovered after {} failed attempt(s)",
                        self.path.display(),
                        self.suppressed_rotation_failures
                    ));
                    self.suppressed_rotation_failures = 0;
                }
            }
            Err(error) => {
                // Only the first failure in a run is reported; a log that cannot rotate would
                // otherwise emit a line of its own for every line it was asked to write.
                if self.suppressed_rotation_failures == 0 {
                    report_log_failure(format_args!(
                        "failed to rotate {}: {error}; logging continues to the current file",
                        self.path.display()
                    ));
                }
                self.suppressed_rotation_failures =
                    self.suppressed_rotation_failures.saturating_add(1);
            }
        }
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        // Serialized across processes: two writers renaming the same archives concurrently would
        // interleave their renames and lose whichever archive lost the race.
        let lock = RotationLock::acquire(&self.path);
        if let Some(mut writer) = self.writer.take() {
            writer.flush()?;
        }
        // Whoever held the lock first may already have done this rotation, in which case rotating
        // again would archive a nearly empty file and push a real one off the end of retention.
        if fs::metadata(&self.path).is_ok_and(|metadata| metadata.len() < self.bytes_written) {
            let file = open_log_file(&self.path)?;
            self.bytes_written = file.metadata()?.len();
            self.writer = Some(BufWriter::new(file));
            drop(lock);
            return Ok(());
        }
        let rotation_result = self.rotate_archives();
        let file = open_log_file(&self.path)?;
        self.bytes_written = file.metadata()?.len();
        self.writer = Some(BufWriter::new(file));
        drop(lock);
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

/// Reports a logging failure on the one channel that cannot itself be broken by it.
///
/// Routing this through `log::` would ask the logger to report its own inability to write.
fn report_log_failure(arguments: fmt::Arguments<'_>) {
    let mut console = anstream::stderr();
    let _ = writeln!(console, "captastic: {arguments}");
}

/// Serializes log rotation between the daemon and one-shot commands sharing a log file.
struct RotationLock {
    path: Option<PathBuf>,
}

impl RotationLock {
    /// Takes the lock, or gives up and rotates anyway once the wait expires.
    ///
    /// Rotation without the lock is worse than rotation with it and better than not logging, so a
    /// contended lock delays a rotation rather than cancelling it.
    fn acquire(log_path: &Path) -> Self {
        let path = rotation_lock_path(log_path);
        let deadline = SystemTime::now() + ROTATION_LOCK_TIMEOUT;
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Self { path: Some(path) };
                }
                // `PermissionDenied` is Windows reporting a lock file that has been deleted but
                // still has a handle closing: the name is taken until it drains, exactly like
                // `AlreadyExists`.
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                    ) =>
                {
                    if break_stale_rotation_lock(&path) {
                        continue;
                    }
                    if SystemTime::now() >= deadline {
                        return Self { path: None };
                    }
                    thread::sleep(ROTATION_LOCK_RETRY_DELAY);
                }
                // The lock cannot be created at all (a read-only directory, say). Rotation is
                // still worth attempting; it simply is not serialized.
                Err(_) => return Self { path: None },
            }
        }
    }
}

impl Drop for RotationLock {
    fn drop(&mut self) {
        if let Some(path) = self.path.as_ref() {
            let _ = fs::remove_file(path);
        }
    }
}

fn rotation_lock_path(log_path: &Path) -> PathBuf {
    let mut name = log_path.as_os_str().to_os_string();
    name.push(".rotate-lock");
    PathBuf::from(name)
}

fn break_stale_rotation_lock(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return true;
    };
    let held_for = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.elapsed().ok());
    held_for.is_some_and(|elapsed| elapsed > ROTATION_LOCK_STALE_AFTER)
        && fs::remove_file(path).is_ok()
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
    let (year, month, day, hour, minute, second, _) = crate::clock::utc_parts(unix_micros);
    let micros = (unix_micros % 1_000_000) as u32;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}Z")
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

    fn log_test_directory(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("current time")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "captastic-log-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("test log directory");
        directory
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

    #[test]
    fn a_writer_reopens_after_another_process_rotates_the_file() {
        // The M15 harm: the daemon and a one-shot command share one log file. Whichever rotates
        // renames it, and the other's handle follows the *file* rather than the name — so it goes
        // on appending to an archive, and its lines disappear from the log anyone reads.
        let directory = log_test_directory("cross-process");
        let path = directory.join("captastic.log");

        let mut daemon = RotatingWriter::new(path.clone(), 1_000_000, 3).expect("daemon writer");
        daemon.write_line("daemon before rotation", "");
        daemon.flush();

        // Another process rotates underneath it.
        fs::rename(&path, archive_path(&path, 1)).expect("rotate from another process");
        fs::write(
            &path,
            b"one-shot line
",
        )
        .expect("fresh log from another process");

        // The daemon has no way to be told, so it has to notice.
        for index in 0..STALENESS_CHECK_LINES {
            daemon.write_line(&format!("daemon line {index}"), "");
        }
        daemon.flush();

        let active = fs::read_to_string(&path).expect("active log");
        assert!(
            active.contains("daemon line"),
            "the daemon kept writing to the archive: {active}"
        );
        assert!(
            active.starts_with("one-shot line"),
            "the other process's line must survive: {active}"
        );
        let archived = fs::read_to_string(archive_path(&path, 1)).expect("archive");
        assert!(archived.contains("daemon before rotation"));

        drop(daemon);
        fs::remove_dir_all(directory).expect("remove test log directory");
    }

    #[test]
    fn rotation_is_serialized_between_writers_sharing_a_file() {
        // Two writers crossing the size limit together must produce one rotation, not two: the
        // second would archive a nearly empty file and push a real one off the end of retention.
        let directory = log_test_directory("rotation-lock");
        let path = directory.join("captastic.log");
        fs::write(
            &path,
            b"0123456789
",
        )
        .expect("seed an over-limit log");

        let mut first = RotatingWriter::new(path.clone(), 8, 3).expect("first writer");
        let mut second = RotatingWriter::new(path.clone(), 8, 3).expect("second writer");
        first.write_line("aaaa", "");
        second.write_line("bbbb", "");
        first.flush();
        second.flush();

        assert_eq!(
            fs::read_to_string(archive_path(&path, 1)).expect("first archive"),
            "0123456789
",
            "the seeded content must be archived exactly once"
        );
        assert!(
            !archive_path(&path, 2).exists(),
            "a second rotation archived an almost-empty file"
        );

        drop(first);
        drop(second);
        fs::remove_dir_all(directory).expect("remove test log directory");
    }

    #[test]
    fn a_rotation_failure_is_reported_once_rather_than_swallowed() {
        // Rotation errors used to be discarded with `let _ =`, so a log that had quietly stopped
        // rotating was discovered when the disk filled. The logger cannot log its own failure, so
        // the observable contract is that it counts them and reports the first.
        let directory = log_test_directory("rotation-failure");
        let path = directory.join("captastic.log");
        // One retained archive, so rotation's only move is `captastic.log` -> `captastic.log.1`.
        let mut writer = RotatingWriter::new(path.clone(), 8, 1).expect("writer");

        // A directory standing in the archive slot. Rotation must first clear the destination,
        // and `remove_file` cannot remove a directory, so the rename never happens.
        fs::create_dir_all(archive_path(&path, 1)).expect("block the archive slot");

        writer.write_line("aaaaaaaa", "");
        writer.write_line("bbbbbbbb", "");
        writer.flush();

        assert!(
            writer.suppressed_rotation_failures >= 1,
            "a failed rotation must be counted, not discarded"
        );
        // Logging continues despite the failure.
        assert!(fs::read_to_string(&path)
            .expect("active log")
            .contains("bbbbbbbb"));

        drop(writer);
        fs::remove_dir_all(directory).expect("remove test log directory");
    }

    #[test]
    fn a_stale_rotation_lock_does_not_block_rotation_forever() {
        let directory = log_test_directory("stale-rotation-lock");
        let path = directory.join("captastic.log");
        let lock = rotation_lock_path(&path);
        fs::write(
            &lock, b"99999
",
        )
        .expect("plant a lock");

        // Fresh: the lock stands, and acquisition falls back to rotating unserialized rather than
        // giving up on rotation altogether.
        assert!(!break_stale_rotation_lock(&lock));

        let backdated = SystemTime::now() - ROTATION_LOCK_STALE_AFTER - Duration::from_secs(60);
        let file = OpenOptions::new()
            .write(true)
            .open(&lock)
            .expect("open lock file");
        file.set_modified(backdated).expect("backdate the lock");
        drop(file);

        assert!(break_stale_rotation_lock(&lock));
        assert!(!lock.exists());

        fs::remove_dir_all(directory).expect("remove test log directory");
    }
}
