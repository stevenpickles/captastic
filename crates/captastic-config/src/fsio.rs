//! Collision-safe filesystem primitives shared by everything Captastic persists.
//!
//! These grew inside the configuration loader, which is where the first thing that had to survive
//! a crash mid-write lived. They are not about configuration: an atomic replace, a uniquely named
//! temporary beside its destination, a durable rename, and the sweeping of artifacts left by a
//! process that died — that is what any writer needs, and Milestone 4's file output is the second
//! one to need it.
//!
//! [`atomic_write`] is public for that reason. The quarantine and artifact-maintenance helpers
//! stay crate-private: they encode configuration-recovery policy, not filesystem mechanics.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

/// Distinguishes concurrent temporary files within one process; the process id separates processes.
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// How long an abandoned temporary or quarantined file may linger before it is swept.
const TEMP_ARTIFACT_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub(crate) const CORRUPT_ARTIFACT_RETENTION: usize = 5;

/// Writes `contents` to `path` so that a reader sees either the previous file or the complete new
/// one, never a partial write.
///
/// The temporary is created beside the destination (a rename across volumes is not atomic),
/// flushed to disk before the rename, and the parent directory is synced afterwards so the rename
/// itself survives a power loss. `replace` is injected so callers can choose the platform
/// replacement semantics they need, and so tests can script a failing rename.
pub fn atomic_write<F>(path: &Path, contents: &[u8], replace: F) -> std::io::Result<()>
where
    F: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    let (temporary_path, mut temporary_file) = create_temporary_file(path)?;
    if let Err(error) = preserve_existing_permissions(path, &temporary_path) {
        drop(temporary_file);
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    let write_result = temporary_file
        .write_all(contents)
        .and_then(|()| temporary_file.sync_all());
    drop(temporary_file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    if let Err(error) = replace(&temporary_path, path) {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    sync_parent_directory(path)?;
    Ok(())
}

#[cfg(unix)]
fn preserve_existing_permissions(path: &Path, temporary_path: &Path) -> std::io::Result<()> {
    match fs::metadata(path) {
        Ok(metadata) => fs::set_permissions(temporary_path, metadata.permissions()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(not(unix))]
fn preserve_existing_permissions(_path: &Path, _temporary_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn create_temporary_file(path: &Path) -> std::io::Result<(PathBuf, File)> {
    maintain_config_artifacts(path, None);
    let parent = usable_parent(path);
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            ErrorKind::InvalidInput,
            "configuration path has no file name",
        )
    })?;
    for _ in 0..128 {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = OsString::from(".");
        temporary_name.push(file_name);
        temporary_name.push(format!(".tmp-{}-{sequence}", std::process::id()));
        let temporary_path = parent.join(temporary_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((temporary_path, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        ErrorKind::AlreadyExists,
        "unable to allocate a unique configuration temporary file",
    ))
}

pub(crate) fn quarantine_config(path: &Path) -> std::io::Result<Option<PathBuf>> {
    quarantine_config_with(path, |from, to| move_file(from, to, false))
}

fn quarantine_config_with<F>(path: &Path, mut move_source: F) -> std::io::Result<Option<PathBuf>>
where
    F: FnMut(&Path, &Path) -> std::io::Result<()>,
{
    let parent = usable_parent(path);
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            ErrorKind::InvalidInput,
            "configuration path has no file name",
        )
    })?;
    for _ in 0..128 {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut quarantine_name = OsString::from(file_name);
        quarantine_name.push(format!(".corrupt-{}-{sequence}", std::process::id()));
        let quarantine_path = parent.join(quarantine_name);
        if quarantine_path.exists() {
            continue;
        }
        match move_source(path, &quarantine_path) {
            Ok(()) => {
                maintain_config_artifacts(path, Some(&quarantine_path));
                return Ok(Some(quarantine_path));
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        ErrorKind::AlreadyExists,
        "unable to allocate a unique corrupt-configuration path",
    ))
}

pub(crate) fn maintain_config_artifacts(path: &Path, protected: Option<&Path>) {
    maintain_config_artifacts_at(path, protected, SystemTime::now());
}

fn maintain_config_artifacts_at(path: &Path, protected: Option<&Path>, now: SystemTime) {
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let temporary_prefix = format!(".{file_name}.tmp-");
    let corrupt_prefix = format!("{file_name}.corrupt-");
    let Ok(entries) = fs::read_dir(usable_parent(path)) else {
        return;
    };
    let mut corrupt = Vec::new();
    for entry in entries.flatten() {
        let artifact_path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if name.starts_with(&temporary_prefix) {
            let stale = metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age >= TEMP_ARTIFACT_MAX_AGE);
            if stale && protected != Some(artifact_path.as_path()) {
                let _ = fs::remove_file(artifact_path);
            }
        } else if name.starts_with(&corrupt_prefix) {
            corrupt.push((
                protected == Some(artifact_path.as_path()),
                metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                artifact_path,
            ));
        }
    }
    corrupt.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then_with(|| right.1.cmp(&left.1))
            .then_with(|| right.2.cmp(&left.2))
    });
    for (_, _, artifact_path) in corrupt.into_iter().skip(CORRUPT_ARTIFACT_RETENTION) {
        let _ = fs::remove_file(artifact_path);
    }
}

fn usable_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    File::open(usable_parent(path))?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Replaces `to` with `from`, atomically where the platform allows it.
///
/// The default `replace` for [`atomic_write`].
pub fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    move_file(from, to, true)
}

/// Moves `from` onto `to`, failing with `AlreadyExists` rather than overwriting.
///
/// The `replace` half of [`atomic_write`] for anything that must not clobber a file it did not
/// create — a capture written into a directory the user also keeps things in, say. Checking for
/// the destination first and then replacing would leave a window in which somebody else creates
/// it; refusing the move closes that window, because the refusal *is* the check.
pub fn finalize_new(from: &Path, to: &Path) -> std::io::Result<()> {
    move_file(from, to, false)
}

#[cfg(not(windows))]
fn move_file(from: &Path, to: &Path, replace_existing: bool) -> std::io::Result<()> {
    if !replace_existing && to.exists() {
        return Err(std::io::Error::new(
            ErrorKind::AlreadyExists,
            "destination already exists",
        ));
    }
    fs::rename(from, to)
}

#[cfg(windows)]
fn move_file(from: &Path, to: &Path, replace_existing: bool) -> std::io::Result<()> {
    windows_file_move::move_file(from, to, replace_existing)
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_file_move {
    use std::io;
    use std::iter;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::thread;
    use std::time::{Duration, Instant};

    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetFileAttributesW, MoveFileExW, SetFileAttributesW, FILE_ATTRIBUTE_ARCHIVE,
        FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED,
        FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM,
        FILE_ATTRIBUTE_TEMPORARY, FILE_FLAGS_AND_ATTRIBUTES, INVALID_FILE_ATTRIBUTES,
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    const SHARING_VIOLATION: i32 = 32;
    const RETRY_INTERVAL: Duration = Duration::from_millis(10);
    const RETRY_TIMEOUT: Duration = Duration::from_millis(250);

    pub(super) fn move_file(from: &Path, to: &Path, replace_existing: bool) -> io::Result<()> {
        let from_wide = wide_path(from)?;
        let to_wide = wide_path(to)?;
        let preserved_attributes = if replace_existing {
            prepare_attribute_preserving_replace(&from_wide, &to_wide)?
        } else {
            None
        };
        let mut flags = MOVEFILE_WRITE_THROUGH;
        if replace_existing {
            flags |= MOVEFILE_REPLACE_EXISTING;
        }
        let started = Instant::now();
        loop {
            // SAFETY: both buffers are NUL-terminated and remain alive for the duration of the
            // synchronous call. The paths refer to sibling files, so replacement stays on-volume.
            let result =
                unsafe { MoveFileExW(PCWSTR(from_wide.as_ptr()), PCWSTR(to_wide.as_ptr()), flags) };
            if result.is_ok() {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(SHARING_VIOLATION) || started.elapsed() >= RETRY_TIMEOUT
            {
                if let Some((source_attributes, destination_attributes)) = preserved_attributes {
                    let _ = set_attributes(&from_wide, source_attributes);
                    let _ = set_attributes(&to_wide, destination_attributes);
                }
                return Err(error);
            }
            thread::sleep(RETRY_INTERVAL);
        }
    }

    fn prepare_attribute_preserving_replace(
        from: &[u16],
        to: &[u16],
    ) -> io::Result<Option<(FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAGS_AND_ATTRIBUTES)>> {
        let Some(destination_attributes) = attributes(to)? else {
            return Ok(None);
        };
        let source_attributes = attributes(from)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "replacement source disappeared")
        })?;
        set_attributes(from, destination_attributes)?;
        if destination_attributes.0 & FILE_ATTRIBUTE_READONLY.0 != 0 {
            let writable =
                FILE_FLAGS_AND_ATTRIBUTES(destination_attributes.0 & !FILE_ATTRIBUTE_READONLY.0);
            if let Err(error) = set_attributes(to, writable) {
                let _ = set_attributes(from, source_attributes);
                return Err(error);
            }
        }
        Ok(Some((source_attributes, destination_attributes)))
    }

    fn attributes(path: &[u16]) -> io::Result<Option<FILE_FLAGS_AND_ATTRIBUTES>> {
        // SAFETY: path is a live, NUL-terminated UTF-16 buffer.
        let raw = unsafe { GetFileAttributesW(PCWSTR(path.as_ptr())) };
        if raw != INVALID_FILE_ATTRIBUTES {
            return Ok(Some(FILE_FLAGS_AND_ATTRIBUTES(raw)));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error)
        }
    }

    fn set_attributes(path: &[u16], attributes: FILE_FLAGS_AND_ATTRIBUTES) -> io::Result<()> {
        let attributes = settable_attributes(attributes);
        // SAFETY: path is a live, NUL-terminated UTF-16 buffer and attributes came from Windows.
        unsafe { SetFileAttributesW(PCWSTR(path.as_ptr()), attributes) }
            .map_err(|_| io::Error::last_os_error())
    }

    fn settable_attributes(attributes: FILE_FLAGS_AND_ATTRIBUTES) -> FILE_FLAGS_AND_ATTRIBUTES {
        let supported = FILE_ATTRIBUTE_ARCHIVE.0
            | FILE_ATTRIBUTE_HIDDEN.0
            | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED.0
            | FILE_ATTRIBUTE_OFFLINE.0
            | FILE_ATTRIBUTE_READONLY.0
            | FILE_ATTRIBUTE_SYSTEM.0
            | FILE_ATTRIBUTE_TEMPORARY.0;
        let filtered = attributes.0 & supported;
        if filtered == 0 {
            FILE_ATTRIBUTE_NORMAL
        } else {
            FILE_FLAGS_AND_ATTRIBUTES(filtered)
        }
    }

    #[cfg(test)]
    pub(super) fn file_attributes(path: &Path) -> io::Result<u32> {
        attributes(&wide_path(path)?)?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "file attributes are unavailable")
            })
            .map(|attributes| attributes.0)
    }

    #[cfg(test)]
    pub(super) fn set_file_attributes(path: &Path, attributes: u32) -> io::Result<()> {
        set_attributes(&wide_path(path)?, FILE_FLAGS_AND_ATTRIBUTES(attributes))
    }

    #[cfg(test)]
    pub(super) fn filtered_file_attributes(attributes: u32) -> u32 {
        settable_attributes(FILE_FLAGS_AND_ATTRIBUTES(attributes)).0
    }

    fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
        let encoded: Vec<u16> = path.as_os_str().encode_wide().collect();
        if encoded.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows path contains an embedded NUL",
            ));
        }
        Ok(encoded.into_iter().chain(iter::once(0)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AppConfig, CONFIG_FILE_NAME};

    /// Mirrors the configuration crate's test directory: created on construction, removed on drop.
    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            Self(test_directory(label))
        }

        fn join(&self, path: &str) -> PathBuf {
            self.0.join(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_directory(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("current time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "captastic-fsio-{label}-{}-{unique}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create isolated test directory");
        path
    }

    #[test]
    fn artifact_maintenance_never_deletes_the_new_quarantine() {
        let directory = TestDirectory::new("artifact-protected");
        let path = directory.join(CONFIG_FILE_NAME);
        let protected = directory
            .0
            .join(format!("{CONFIG_FILE_NAME}.corrupt-protected"));
        fs::write(&protected, "new damage").expect("write protected quarantine");
        for sequence in 0..CORRUPT_ARTIFACT_RETENTION {
            fs::write(
                directory
                    .0
                    .join(format!("{CONFIG_FILE_NAME}.corrupt-old-{sequence}")),
                "old damage",
            )
            .expect("write old quarantine");
        }

        maintain_config_artifacts_at(&path, Some(&protected), SystemTime::now());

        assert!(protected.exists());
        assert_eq!(
            fs::read_dir(&directory.0)
                .expect("read protected artifacts")
                .count(),
            CORRUPT_ARTIFACT_RETENTION
        );
    }

    #[test]
    fn artifact_maintenance_removes_stale_temps_and_bounds_corrupt_backups() {
        let directory = TestDirectory::new("artifact-retention");
        let path = directory.join(CONFIG_FILE_NAME);
        let first_temp = directory.0.join(format!(".{CONFIG_FILE_NAME}.tmp-first"));
        let second_temp = directory.0.join(format!(".{CONFIG_FILE_NAME}.tmp-second"));
        fs::write(&first_temp, "first").expect("write first temporary artifact");
        fs::write(&second_temp, "second").expect("write second temporary artifact");
        let baseline = SystemTime::now();

        maintain_config_artifacts_at(
            &path,
            None,
            baseline + TEMP_ARTIFACT_MAX_AGE - Duration::from_secs(1),
        );
        assert!(first_temp.exists());
        assert!(second_temp.exists());

        for sequence in 0..=CORRUPT_ARTIFACT_RETENTION {
            fs::write(
                directory
                    .0
                    .join(format!("{CONFIG_FILE_NAME}.corrupt-test-{sequence}")),
                "damaged",
            )
            .expect("write corrupt artifact");
        }
        maintain_config_artifacts_at(
            &path,
            None,
            baseline + TEMP_ARTIFACT_MAX_AGE + Duration::from_secs(1),
        );

        assert!(!first_temp.exists());
        assert!(!second_temp.exists());
        assert_eq!(
            fs::read_dir(&directory.0)
                .expect("read retained artifacts")
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(&format!("{CONFIG_FILE_NAME}.corrupt-"))
                })
                .count(),
            CORRUPT_ARTIFACT_RETENTION
        );
    }

    #[cfg(windows)]
    #[test]
    fn atomic_replace_filters_filesystem_controlled_windows_attributes() {
        const READONLY: u32 = 0x1;
        const HIDDEN: u32 = 0x2;
        const COMPRESSED: u32 = 0x800;
        const ENCRYPTED: u32 = 0x4000;
        const SPARSE_FILE: u32 = 0x200;
        const REPARSE_POINT: u32 = 0x400;

        assert_eq!(
            windows_file_move::filtered_file_attributes(
                READONLY | HIDDEN | COMPRESSED | ENCRYPTED | SPARSE_FILE | REPARSE_POINT
            ),
            READONLY | HIDDEN
        );
    }

    #[cfg(windows)]
    #[test]
    fn atomic_replace_preserves_hidden_and_readonly_attributes_on_windows() {
        const READONLY: u32 = 0x1;
        const HIDDEN: u32 = 0x2;
        const NORMAL: u32 = 0x80;

        let directory = TestDirectory::new("windows-attributes");
        let path = directory.join(CONFIG_FILE_NAME);
        fs::write(&path, "before").expect("write attributed config");
        windows_file_move::set_file_attributes(&path, READONLY | HIDDEN)
            .expect("set original attributes");

        atomic_write(&path, b"after", replace_file).expect("replace attributed config");

        let attributes = windows_file_move::file_attributes(&path).expect("read final attributes");
        assert_eq!(attributes & (READONLY | HIDDEN), READONLY | HIDDEN);
        assert_eq!(
            fs::read_to_string(&path).expect("read replacement"),
            "after"
        );
        windows_file_move::set_file_attributes(&path, NORMAL).expect("make test file removable");
    }

    #[test]
    fn quarantine_tolerates_source_removal_after_the_damaged_read() {
        let directory = TestDirectory::new("quarantine-not-found-race");
        let path = directory.join(CONFIG_FILE_NAME);
        fs::write(&path, "damaged").expect("create source before race");

        let result = quarantine_config_with(&path, |source, _destination| {
            fs::remove_file(source).expect("simulate concurrent source removal");
            Err(std::io::Error::new(
                ErrorKind::NotFound,
                "source disappeared before quarantine move",
            ))
        })
        .expect("a concurrently removed source is already recovered");

        assert_eq!(result, None);
        assert!(!path.exists());
        assert_eq!(
            fs::read_dir(&directory.0)
                .expect("read test directory")
                .count(),
            0
        );
    }

    #[test]
    fn atomic_write_replaces_an_existing_configuration() {
        let directory = TestDirectory::new("atomic-success");
        let path = directory.join(CONFIG_FILE_NAME);
        fs::write(&path, "schema_version = 0\n").expect("write original config");
        let replacement = AppConfig::default()
            .to_toml_pretty()
            .expect("serialize replacement config");

        atomic_write(&path, replacement.as_bytes(), replace_file)
            .expect("atomically replace config");

        assert_eq!(
            fs::read_to_string(&path).expect("read replacement"),
            replacement
        );
        AppConfig::load(&path).expect("replacement is parseable");
        let entries = fs::read_dir(&directory.0)
            .expect("read test directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("directory entries");
        assert_eq!(entries.len(), 1, "temporary file should be moved away");
    }

    #[test]
    fn failed_atomic_replace_preserves_the_original_configuration() {
        let directory = TestDirectory::new("atomic-failure");
        let path = directory.join(CONFIG_FILE_NAME);
        let original = AppConfig::default()
            .to_toml_pretty()
            .expect("serialize original config");
        fs::write(&path, &original).expect("write original config");

        let error = atomic_write(&path, b"schema_version = ", |_temporary, _destination| {
            Err(std::io::Error::other("injected failure before replacement"))
        })
        .expect_err("injected replacement failure");

        assert_eq!(error.kind(), ErrorKind::Other);
        assert_eq!(
            fs::read_to_string(&path).expect("original remains"),
            original
        );
        AppConfig::load(&path).expect("original remains parseable");
        let entries = fs::read_dir(&directory.0)
            .expect("read test directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("directory entries");
        assert_eq!(entries.len(), 1, "temporary file should be cleaned up");
    }
}
