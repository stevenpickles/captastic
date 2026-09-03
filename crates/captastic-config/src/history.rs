//! Recent captures, remembered well enough to find them again.
//!
//! Its own file rather than a section of `state.toml`, and for the same reason `state.toml` is not
//! a section of `captastic.toml`: writes happen at different rates. Remembered UI state changes
//! when a person moves a toolbar; history changes on every capture. Sharing a file would make each
//! capture rewrite the whole of the other thing, under the other thing's lock, which is precisely
//! the fsync churn the earlier split removed.
//!
//! What is stored is metadata only — where the file went, when, how big, what shape. Never pixels,
//! never clipboard contents. A history entry has to be enough to find a capture, and nothing more.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::fsio::{atomic_write, replace_file, FileLock};
use crate::{storage_directory, ConfigError};

pub const HISTORY_FILE_NAME: &str = "history.toml";
pub const HISTORY_SCHEMA_VERSION: u32 = 1;

/// One capture, as much of it as is worth keeping.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryEntry {
    /// Where the capture was written.
    pub path: PathBuf,
    /// When it was taken, in Unix microseconds — an absolute instant, so retention by age does
    /// not depend on the file still existing or on its mtime surviving a copy. Signed because a
    /// TOML integer is, which keeps the stored value and the parsed one the same type.
    pub captured_at_micros: i64,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub display: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
}

impl HistoryEntry {
    fn age(&self, now: SystemTime) -> Duration {
        let micros = u64::try_from(self.captured_at_micros).unwrap_or(0);
        let captured = UNIX_EPOCH + Duration::from_micros(micros);
        // A capture stamped in the future is not "very old"; saturating to zero keeps a clock
        // adjustment from silently emptying the history.
        now.duration_since(captured).unwrap_or(Duration::ZERO)
    }

    /// Stamps an entry from a wall-clock instant.
    pub fn micros_since_epoch(at: SystemTime) -> i64 {
        at.duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| i64::try_from(elapsed.as_micros()).ok())
            .unwrap_or(0)
    }
}

/// How much history to keep. Every limit is independent; an entry surviving one may still be
/// dropped by another.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPolicy {
    /// Newest N entries. Zero disables history entirely.
    pub max_items: usize,
    /// Entries older than this are dropped. `None` means age is not a reason to forget.
    pub max_age: Option<Duration>,
    /// Total bytes of the captures referred to. `None` means size is not a reason to forget.
    pub max_total_bytes: Option<u64>,
}

/// The captures Captastic remembers, newest first.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureHistory {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<HistoryEntry>,
}

impl Default for CaptureHistory {
    fn default() -> Self {
        Self {
            schema_version: HISTORY_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

impl CaptureHistory {
    /// The most recent capture, for "open the last one".
    pub fn most_recent(&self) -> Option<&HistoryEntry> {
        self.entries.first()
    }

    /// Adds `entry` as the newest and applies `policy`, returning what was forgotten.
    ///
    /// `now` is a parameter so retention is testable without waiting for time to pass — a
    /// milestone exit criterion, and the difference between a test suite that takes milliseconds
    /// and one nobody runs.
    pub fn record(
        &mut self,
        entry: HistoryEntry,
        policy: RetentionPolicy,
        now: SystemTime,
    ) -> Vec<HistoryEntry> {
        // Newest first, so "the last capture" is a lookup rather than a scan, and so the
        // retention rules below can all be expressed as "keep the front".
        self.entries.insert(0, entry);
        self.prune(policy, now)
    }

    /// Applies `policy`, returning the entries it dropped.
    pub fn prune(&mut self, policy: RetentionPolicy, now: SystemTime) -> Vec<HistoryEntry> {
        let mut kept = Vec::with_capacity(self.entries.len().min(policy.max_items));
        let mut dropped = Vec::new();
        let mut total_bytes = 0_u64;
        for entry in std::mem::take(&mut self.entries) {
            let too_many = kept.len() >= policy.max_items;
            let too_old = policy.max_age.is_some_and(|limit| entry.age(now) > limit);
            let too_large = policy
                .max_total_bytes
                .is_some_and(|limit| total_bytes.saturating_add(entry.bytes) > limit);
            if too_many || too_old || too_large {
                dropped.push(entry);
                continue;
            }
            total_bytes = total_bytes.saturating_add(entry.bytes);
            kept.push(entry);
        }
        self.entries = kept;
        dropped
    }

    /// Forgets entries whose files are gone, returning how many were removed.
    ///
    /// History records where a capture went; the user is free to move or delete it afterwards, and
    /// an entry pointing at nothing is worse than no entry — "Open Last Capture" would fail rather
    /// than opening the one before it.
    pub fn forget_missing(&mut self) -> usize {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.path.exists());
        before - self.entries.len()
    }
}

#[derive(Clone, Debug)]
pub struct HistoryStore {
    path: Option<PathBuf>,
}

impl HistoryStore {
    pub fn for_default_storage() -> Self {
        Self {
            path: storage_directory().map(|directory| directory.join(HISTORY_FILE_NAME)),
        }
    }

    /// The history belonging to a specific configuration file, beside it.
    pub fn for_config(config_path: impl Into<PathBuf>) -> Self {
        Self {
            path: config_path
                .into()
                .parent()
                .map(|parent| parent.join(HISTORY_FILE_NAME)),
        }
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    fn required_path(&self) -> Result<&Path, ConfigError> {
        self.path
            .as_deref()
            .ok_or(ConfigError::HomeDirectoryUnavailable)
    }

    pub fn load(&self) -> Result<CaptureHistory, ConfigError> {
        Ok(read_history(self.required_path()?)?.unwrap_or_default())
    }

    /// Records a capture and applies retention, returning the entries that were forgotten.
    pub fn record(
        &self,
        entry: HistoryEntry,
        policy: RetentionPolicy,
        now: SystemTime,
    ) -> Result<Vec<HistoryEntry>, ConfigError> {
        self.update(|history| history.record(entry, policy, now))
    }

    /// Applies retention without adding anything, for an explicit prune command.
    pub fn prune(
        &self,
        policy: RetentionPolicy,
        now: SystemTime,
    ) -> Result<Vec<HistoryEntry>, ConfigError> {
        self.update(|history| {
            let mut dropped = history.prune(policy, now);
            // An explicit prune is also the moment to notice files the user removed themselves.
            let missing = history.forget_missing();
            let _ = missing;
            dropped.shrink_to_fit();
            dropped
        })
    }

    fn update<T>(&self, apply: impl FnOnce(&mut CaptureHistory) -> T) -> Result<T, ConfigError> {
        let path = self.required_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let _lock = FileLock::acquire(path)?;
        let mut history = read_history(path)?.unwrap_or_default();
        let outcome = apply(&mut history);
        write_history(path, &history)?;
        Ok(outcome)
    }
}

fn read_history(path: &Path) -> Result<Option<CaptureHistory>, ConfigError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.display().to_string(),
                source,
            })
        }
    };
    let history: CaptureHistory = toml::from_str(&text)?;
    if history.schema_version != HISTORY_SCHEMA_VERSION {
        return Err(ConfigError::UnsupportedSchema(history.schema_version));
    }
    Ok(Some(history))
}

fn write_history(path: &Path, history: &CaptureHistory) -> Result<(), ConfigError> {
    // `toml` serializes a struct's scalar fields before its tables, and a list of tables is a
    // table; wrapping keeps `schema_version` from being emitted after the entries, where it would
    // parse as a field of the last one.
    let mut document = BTreeMap::new();
    document.insert("history", history);
    let text = toml::to_string_pretty(&document).map_err(ConfigError::Serialize)?;
    let text = text
        .strip_prefix("[history]\n")
        .map_or_else(|| text.clone(), str::to_owned)
        .replace("[[history.entries]]", "[[entries]]");
    atomic_write(path, text.as_bytes(), replace_file).map_err(|source| ConfigError::Write {
        path: path.display().to_string(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(max_items: usize) -> RetentionPolicy {
        RetentionPolicy {
            max_items,
            max_age: None,
            max_total_bytes: None,
        }
    }

    fn entry(label: &str, captured_at_micros: i64, bytes: u64) -> HistoryEntry {
        HistoryEntry {
            path: PathBuf::from(format!("C:/captures/{label}.png")),
            captured_at_micros,
            bytes,
            width: 1920,
            height: 1080,
            display: "primary".to_owned(),
            mode: "region".to_owned(),
        }
    }

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_000_000)
    }

    fn micros_ago(seconds: i64) -> i64 {
        (1_000_000 - seconds) * 1_000_000
    }

    #[test]
    fn the_newest_capture_is_the_one_at_the_front() {
        let mut history = CaptureHistory::default();
        history.record(entry("first", micros_ago(30), 10), policy(10), now());
        history.record(entry("second", micros_ago(20), 10), policy(10), now());

        assert_eq!(
            history.most_recent().expect("a capture").path,
            PathBuf::from("C:/captures/second.png")
        );
    }

    #[test]
    fn an_item_limit_forgets_the_oldest_and_says_which() {
        let mut history = CaptureHistory::default();
        for index in 0..5_i64 {
            history.record(
                entry(&format!("capture-{index}"), micros_ago(50 - index), 10),
                policy(3),
                now(),
            );
        }

        assert_eq!(history.entries.len(), 3);
        assert_eq!(
            history
                .entries
                .iter()
                .map(|entry| entry.path.file_stem().expect("stem").to_string_lossy())
                .collect::<Vec<_>>(),
            ["capture-4", "capture-3", "capture-2"]
        );
    }

    #[test]
    fn an_age_limit_is_measured_against_an_injected_clock() {
        // Retention has to be testable without wall-clock sleeps: a milestone exit criterion, and
        // the difference between a suite that runs in milliseconds and one nobody runs.
        let mut history = CaptureHistory::default();
        history.record(entry("old", micros_ago(3_600), 10), policy(10), now());
        history.record(entry("fresh", micros_ago(10), 10), policy(10), now());

        let dropped = history.prune(
            RetentionPolicy {
                max_items: 10,
                max_age: Some(Duration::from_secs(60)),
                max_total_bytes: None,
            },
            now(),
        );

        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].path, PathBuf::from("C:/captures/old.png"));
        assert_eq!(history.entries.len(), 1);
    }

    #[test]
    fn a_storage_limit_keeps_the_newest_that_fit() {
        let mut history = CaptureHistory::default();
        for index in 0..4_i64 {
            history.record(
                entry(&format!("capture-{index}"), micros_ago(40 - index), 100),
                policy(10),
                now(),
            );
        }

        let dropped = history.prune(
            RetentionPolicy {
                max_items: 10,
                max_age: None,
                max_total_bytes: Some(250),
            },
            now(),
        );

        // Two at 100 bytes fit; the third would exceed the limit.
        assert_eq!(history.entries.len(), 2);
        assert_eq!(dropped.len(), 2);
        assert!(history.entries.iter().map(|entry| entry.bytes).sum::<u64>() <= 250);
    }

    #[test]
    fn a_zero_item_limit_remembers_nothing() {
        // How history is turned off: the recording path stays live, so nothing has to check a
        // flag before calling it, and nothing accumulates.
        let mut history = CaptureHistory::default();
        let dropped = history.record(entry("capture", micros_ago(1), 10), policy(0), now());

        assert!(history.entries.is_empty());
        assert_eq!(dropped.len(), 1);
        assert!(history.most_recent().is_none());
    }

    #[test]
    fn history_round_trips_through_its_file() {
        let directory = std::env::temp_dir().join(format!(
            "captastic-history-round-trip-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("now")
                .as_nanos()
        ));
        fs::create_dir_all(&directory).expect("create test directory");
        let store = HistoryStore::at(directory.join(HISTORY_FILE_NAME));

        store
            .record(entry("first", micros_ago(20), 10), policy(10), now())
            .expect("record");
        store
            .record(entry("second", micros_ago(10), 20), policy(10), now())
            .expect("record");

        let history = store.load().expect("load");
        assert_eq!(history.entries.len(), 2);
        assert_eq!(
            history.most_recent().expect("newest").path,
            PathBuf::from("C:/captures/second.png")
        );
        assert_eq!(history.entries[1].bytes, 10);
        assert_eq!(history.entries[0].display, "primary");

        fs::remove_dir_all(directory).expect("clean up");
    }

    #[test]
    fn entries_whose_files_are_gone_are_forgotten() {
        // The user is free to move or delete a capture. An entry pointing at nothing is worse
        // than no entry: "Open Last Capture" would fail rather than opening the one before it.
        let directory = std::env::temp_dir().join(format!(
            "captastic-history-missing-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("now")
                .as_nanos()
        ));
        fs::create_dir_all(&directory).expect("create test directory");
        let present = directory.join("present.png");
        fs::write(&present, b"capture").expect("write a capture");

        let mut history = CaptureHistory::default();
        history.record(entry("gone", micros_ago(20), 10), policy(10), now());
        history.record(
            HistoryEntry {
                path: present.clone(),
                ..entry("present", micros_ago(10), 10)
            },
            policy(10),
            now(),
        );

        assert_eq!(history.forget_missing(), 1);
        assert_eq!(history.entries.len(), 1);
        assert_eq!(history.most_recent().expect("survivor").path, present);

        fs::remove_dir_all(directory).expect("clean up");
    }

    #[test]
    fn history_from_a_newer_schema_is_reported_rather_than_discarded() {
        let directory = std::env::temp_dir().join(format!(
            "captastic-history-schema-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("now")
                .as_nanos()
        ));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join(HISTORY_FILE_NAME);
        fs::write(
            &path,
            format!("schema_version = {}\n", HISTORY_SCHEMA_VERSION + 1),
        )
        .expect("write newer history");

        assert!(matches!(
            HistoryStore::at(&path).load(),
            Err(ConfigError::UnsupportedSchema(version)) if version == HISTORY_SCHEMA_VERSION + 1
        ));

        fs::remove_dir_all(directory).expect("clean up");
    }
}
