//! App-owned UI state, stored separately from the user's configuration.
//!
//! Everything Captastic remembers about how you last used it — where the overlay toolbar sat, the
//! tool you picked, the region you confirmed — used to be written back into `captastic.toml` as a
//! `toml_edit` read-modify-write of the user's own file. That works at interaction cadence and
//! stops working for anything faster: it puts fsync churn next to the hotkey path, it risks
//! clobbering a hand edit made while the daemon was running, and it means a file the user owns
//! keeps changing underneath them without their asking.
//!
//! This file is Captastic's, not the user's. Nobody hand-edits it, so it is serialized whole
//! rather than surgically patched, and read-modify-write cycles are serialized against each other
//! so two processes cannot lose one another's updates.

use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::{
    atomic_write, default_config_path, replace_file, storage_directory, CaptureRegion,
    CaptureRegionSource, CaptureTool, ConfigError, ConfirmedRegion, DisplayUiConfig,
    DisplayUiState, UiConfig,
};

pub const STATE_FILE_NAME: &str = "state.toml";
pub const STATE_SCHEMA_VERSION: u32 = 1;

/// How long a writer waits for another process to finish its read-modify-write cycle.
///
/// Generous relative to the work being serialized (a few kilobytes of TOML and one atomic
/// replace), because the alternative to waiting is losing the update.
const LOCK_TIMEOUT: Duration = Duration::from_millis(2_000);
/// How long a lock file may sit untouched before it is assumed to belong to a process that died
/// holding it. Well above `LOCK_TIMEOUT`, so a merely slow writer is never robbed of its lock.
const LOCK_STALE_AFTER: Duration = Duration::from_secs(30);
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(5);

/// Everything Captastic remembers between runs.
///
/// Versioned independently of `AppConfig`: this schema changes when Captastic's own memory
/// changes, which has nothing to do with when the user's settings vocabulary changes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiState {
    pub schema_version: u32,
    /// Pre-per-display globals, still honored as a fallback for a display with no entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_y: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_capture_tool: Option<CaptureTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_region: Option<CaptureRegion>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub displays: BTreeMap<String, DisplayUiConfig>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            overlay_x: None,
            overlay_y: None,
            last_capture_tool: None,
            last_region: None,
            displays: BTreeMap::new(),
        }
    }
}

impl UiState {
    /// Adopts the `[ui]` section of a configuration file as this state's starting point.
    pub fn from_config_section(ui: &UiConfig) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            overlay_x: ui.overlay_x,
            overlay_y: ui.overlay_y,
            last_capture_tool: ui.last_capture_tool,
            last_region: ui.last_region,
            displays: ui.displays.clone(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.overlay_x.is_none()
            && self.overlay_y.is_none()
            && self.last_capture_tool.is_none()
            && self.last_region.is_none()
            && self.displays.is_empty()
    }

    pub fn confirmed_regions(&self) -> BTreeMap<String, ConfirmedRegion> {
        self.displays
            .iter()
            .filter_map(|(display_id, state)| {
                state
                    .last_confirmed_region
                    .zip(state.last_confirmed_region_source)
                    .map(|(region, source)| {
                        (display_id.clone(), ConfirmedRegion { region, source })
                    })
            })
            .collect()
    }

    fn display_mut(&mut self, display_id: &str) -> &mut DisplayUiConfig {
        self.displays.entry(display_id.to_owned()).or_default()
    }
}

/// Resolves what a specific display should restore, falling back to the pre-per-display globals.
pub fn resolve_display_ui_state(state: &UiState, display_id: &str) -> DisplayUiState {
    let display = state.displays.get(display_id);
    let display_region = display.and_then(|entry| entry.last_region);
    DisplayUiState {
        overlay_center: display
            .and_then(|entry| entry.overlay_center_x.zip(entry.overlay_center_y)),
        overlay_position: display
            .and_then(|entry| entry.overlay_x.zip(entry.overlay_y))
            .or_else(|| state.overlay_x.zip(state.overlay_y)),
        tool: display
            .and_then(|entry| entry.last_capture_tool)
            .or(state.last_capture_tool),
        region: display_region.or(state.last_region),
        region_source: display.and_then(|entry| entry.last_region_source),
        region_is_display_local: display_region.is_some(),
        confirmed_region: display
            .and_then(|entry| {
                entry
                    .last_confirmed_region
                    .zip(entry.last_confirmed_region_source)
            })
            .map(|(region, source)| ConfirmedRegion { region, source }),
    }
}

#[derive(Clone, Debug)]
pub struct UiStateStore {
    state_path: Option<PathBuf>,
    /// The configuration this state sits beside. Used to migrate a pre-split `[ui]` section, and
    /// to answer the tray's "open configuration" action, which still means the user's file.
    config_path: Option<PathBuf>,
}

impl UiStateStore {
    /// The store in the per-user storage directory, beside the default configuration.
    pub fn for_default_storage() -> Self {
        Self {
            state_path: storage_directory().map(|directory| directory.join(STATE_FILE_NAME)),
            config_path: default_config_path(),
        }
    }

    /// The store belonging to an explicitly supplied configuration file.
    ///
    /// State lives beside the configuration it accompanies, so `--config` pointing at an alternate
    /// profile gets that profile's memory rather than the default one's.
    pub fn for_config(config_path: impl Into<PathBuf>) -> Self {
        let config_path = config_path.into();
        let state_path = config_path
            .parent()
            .map(|parent| parent.join(STATE_FILE_NAME));
        Self {
            state_path,
            config_path: Some(config_path),
        }
    }

    /// A store addressing one specific state file, with no configuration to migrate from.
    pub fn at(state_path: impl Into<PathBuf>) -> Self {
        Self {
            state_path: Some(state_path.into()),
            config_path: None,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.state_path.as_deref()
    }

    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// Resolves the configuration file for the tray's "open configuration" action, creating the
    /// default one if it is the default and does not exist yet.
    pub fn prepare_for_open(&self) -> Result<PathBuf, ConfigError> {
        let path = self
            .config_path
            .as_deref()
            .ok_or(ConfigError::HomeDirectoryUnavailable)?;
        crate::prepare_config_path_for_open(
            path,
            default_config_path().as_deref(),
            crate::ensure_default_config,
        )
    }

    fn required_state_path(&self) -> Result<&Path, ConfigError> {
        self.state_path
            .as_deref()
            .ok_or(ConfigError::HomeDirectoryUnavailable)
    }

    /// Reads the whole remembered state, migrating a pre-split `[ui]` section on first use.
    pub fn load(&self) -> Result<UiState, ConfigError> {
        let path = self.required_state_path()?;
        match read_state(path)? {
            Some(state) => Ok(state),
            None => Ok(self.migrated_state()),
        }
    }

    pub fn load_display_ui_state(&self, display_id: &str) -> Result<DisplayUiState, ConfigError> {
        Ok(resolve_display_ui_state(&self.load()?, display_id))
    }

    pub fn confirmed_regions(&self) -> Result<BTreeMap<String, ConfirmedRegion>, ConfigError> {
        Ok(self.load()?.confirmed_regions())
    }

    pub fn save_display_overlay_center(
        &self,
        display_id: &str,
        center_x: f64,
        center_y: f64,
    ) -> Result<(), ConfigError> {
        if !center_x.is_finite()
            || !center_y.is_finite()
            || !(0.0..=1.0).contains(&center_x)
            || !(0.0..=1.0).contains(&center_y)
        {
            return Err(ConfigError::InvalidValue(
                "display overlay center coordinates must be finite values between 0 and 1"
                    .to_owned(),
            ));
        }
        self.update(|state| {
            let display = state.display_mut(display_id);
            display.overlay_center_x = Some(center_x);
            display.overlay_center_y = Some(center_y);
            // The normalized centre supersedes any pixel position remembered for this display.
            // Leaving both would let `resolve_display_ui_state` restore a stale absolute position
            // on a monitor that has since moved or changed resolution.
            display.overlay_x = None;
            display.overlay_y = None;
        })
    }

    pub fn save_display_interaction_state(
        &self,
        display_id: &str,
        tool: CaptureTool,
        region: Option<CaptureRegion>,
        region_source: Option<CaptureRegionSource>,
    ) -> Result<(), ConfigError> {
        if region.is_some_and(|region| region.width == 0 || region.height == 0)
            || region_source.is_some_and(|source| source.width == 0 || source.height == 0)
        {
            return Err(ConfigError::InvalidValue(
                "remembered capture regions must have nonzero dimensions".to_owned(),
            ));
        }
        self.update(|state| {
            let display = state.display_mut(display_id);
            display.last_capture_tool = Some(tool);
            if let Some(region) = region {
                display.last_region = Some(region);
                // A region always arrives with the geometry it was measured against, or with
                // none at all. Keeping a previous source beside a new region would describe the
                // region in terms of a monitor layout it never belonged to.
                display.last_region_source = region_source;
            }
        })
    }

    pub fn save_display_confirmed_region(
        &self,
        display_id: &str,
        region: CaptureRegion,
        source: CaptureRegionSource,
    ) -> Result<(), ConfigError> {
        if region.width == 0 || region.height == 0 || source.width == 0 || source.height == 0 {
            return Err(ConfigError::InvalidValue(
                "confirmed capture regions must have nonzero dimensions".to_owned(),
            ));
        }
        self.update(|state| {
            let display = state.display_mut(display_id);
            display.last_confirmed_region = Some(region);
            display.last_confirmed_region_source = Some(source);
        })
    }

    /// Runs one read-modify-write cycle with every other writer held off.
    ///
    /// The lock is what makes concurrent writers safe: the daemon and a one-shot capture both
    /// persist state, and without exclusion the second to write would rewrite the whole file from
    /// a copy read before the first one's update landed, silently discarding it.
    fn update(&self, apply: impl FnOnce(&mut UiState)) -> Result<(), ConfigError> {
        let path = self.required_state_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let _lock = StateLock::acquire(path)?;
        let mut state = match read_state(path)? {
            Some(state) => state,
            None => self.migrated_state(),
        };
        apply(&mut state);
        write_state(path, &state)
    }

    /// The state a pre-split installation implies: whatever its configuration's `[ui]` section
    /// held. Returns the default when there is no configuration, none can be read, or it carries
    /// no remembered state.
    fn migrated_state(&self) -> UiState {
        let Some(config_path) = self.config_path.as_deref() else {
            return UiState::default();
        };
        // Deliberately lenient. A configuration too damaged to parse is a problem for whoever
        // loads it as configuration; it must not also cost the user their remembered layout, and
        // it must not turn every state read into a hard failure.
        let Ok(text) = fs::read_to_string(config_path) else {
            return UiState::default();
        };
        let Ok(legacy) = toml::from_str::<LegacyUiSection>(&text) else {
            return UiState::default();
        };
        let state = UiState::from_config_section(&legacy.ui);
        if !state.is_empty() {
            log::info!(
                "migrating remembered UI state out of {} into {}; the configuration's [ui] section is no longer written",
                config_path.display(),
                self.state_path
                    .as_deref()
                    .unwrap_or(Path::new(STATE_FILE_NAME))
                    .display(),
            );
        }
        state
    }
}

/// Reads only `[ui]`, ignoring every other section.
///
/// Lenient on purpose: this runs against the user's configuration to recover pre-split state, and
/// the rest of that file is somebody else's problem at this point.
#[derive(Default, Deserialize)]
struct LegacyUiSection {
    #[serde(default)]
    ui: UiConfig,
}

fn read_state(path: &Path) -> Result<Option<UiState>, ConfigError> {
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
    let state: UiState = toml::from_str(&text)?;
    if state.schema_version != STATE_SCHEMA_VERSION {
        return Err(ConfigError::UnsupportedSchema(state.schema_version));
    }
    crate::validate_display_entries(&state.displays)?;
    Ok(Some(state))
}

fn write_state(path: &Path, state: &UiState) -> Result<(), ConfigError> {
    // Serialized whole rather than patched in place. This file has no comments to preserve and no
    // hand edits to respect, which is the entire reason for splitting it out of the user's.
    let text = toml::to_string_pretty(state).map_err(ConfigError::Serialize)?;
    atomic_write(path, text.as_bytes(), replace_file).map_err(|source| ConfigError::Write {
        path: path.display().to_string(),
        source,
    })
}

/// A cross-process advisory lock over one state file.
struct StateLock {
    path: PathBuf,
}

impl StateLock {
    fn acquire(state_path: &Path) -> Result<Self, ConfigError> {
        let path = lock_path(state_path);
        let deadline = Instant::now() + LOCK_TIMEOUT;
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    // Contents are diagnostic only; the file's existence is the lock.
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Self { path });
                }
                // `AlreadyExists` is the ordinary "somebody has it". `PermissionDenied` means the
                // same thing on Windows, where a lock file that has been deleted but still has an
                // open handle sits in a delete-pending state: the name is taken, and creating it
                // fails with ACCESS_DENIED rather than ALREADY_EXISTS until the last handle
                // closes. Treating that as a hard error made every release a chance to fail the
                // next acquire.
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::AlreadyExists | ErrorKind::PermissionDenied
                    ) =>
                {
                    if break_stale_lock(&path) {
                        continue;
                    }
                    if Instant::now() >= deadline {
                        return Err(ConfigError::Write {
                            path: path.display().to_string(),
                            // Carries the cause: a genuine permission problem times out exactly
                            // the way contention does, and the two need telling apart.
                            source: std::io::Error::new(
                                ErrorKind::TimedOut,
                                format!(
                                    "could not take the UI state lock within {} ms (last attempt: {error})",
                                    LOCK_TIMEOUT.as_millis(),
                                ),
                            ),
                        });
                    }
                    thread::sleep(LOCK_RETRY_DELAY);
                }
                Err(source) => {
                    return Err(ConfigError::Write {
                        path: path.display().to_string(),
                        source,
                    })
                }
            }
        }
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        // A failure here leaves a lock that the staleness check will clear.
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_path(state_path: &Path) -> PathBuf {
    let mut name = state_path
        .file_name()
        .map(std::ffi::OsString::from)
        .unwrap_or_else(|| std::ffi::OsString::from(STATE_FILE_NAME));
    name.push(".lock");
    state_path
        .parent()
        .map_or_else(|| PathBuf::from(&name), |parent| parent.join(&name))
}

/// Removes a lock whose holder has plainly gone away, reporting whether it did.
///
/// A process killed mid-write would otherwise lock its own state file out permanently. The window
/// is deliberately far longer than any legitimate hold, so a slow writer keeps its lock; two
/// processes both deciding a lock is stale is harmless, because whichever loses the subsequent
/// `create_new` simply goes back to waiting.
fn break_stale_lock(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        // Already gone: the holder released it between our open and this check.
        return true;
    };
    let held_for = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.elapsed().ok());
    if held_for.is_some_and(|elapsed| elapsed > LOCK_STALE_AFTER) {
        log::warn!(
            "clearing a UI state lock held for more than {} s at {}; its owner probably exited without releasing it",
            LOCK_STALE_AFTER.as_secs(),
            path.display()
        );
        return fs::remove_file(path).is_ok();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CONFIG_FILE_NAME;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("captastic-ui-state-{label}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create isolated test directory");
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn region(width: u32, height: u32) -> CaptureRegion {
        CaptureRegion {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    fn region_source(width: u32, height: u32) -> CaptureRegionSource {
        CaptureRegionSource {
            width,
            height,
            rotation_degrees: 0,
        }
    }

    #[test]
    fn saving_state_leaves_the_users_configuration_untouched() {
        // The point of the split: Captastic's own memory stops rewriting a file the user owns.
        let directory = TestDirectory::new("isolation");
        let config_path = directory.join(CONFIG_FILE_NAME);
        let original = "# a comment the user wrote\nschema_version = 1\n";
        fs::write(&config_path, original).expect("seed configuration");
        let store = UiStateStore::for_config(&config_path);

        store
            .save_display_overlay_center("display-1", 0.25, 0.75)
            .expect("save overlay center");

        assert_eq!(
            fs::read_to_string(&config_path).expect("read configuration"),
            original,
            "the user's configuration must not be rewritten"
        );
        assert!(directory.join(STATE_FILE_NAME).exists());
        assert_eq!(
            store
                .load_display_ui_state("display-1")
                .expect("load state")
                .overlay_center,
            Some((0.25, 0.75))
        );
    }

    #[test]
    fn pre_split_state_migrates_out_of_the_configuration_once() {
        let directory = TestDirectory::new("migration");
        let config_path = directory.join(CONFIG_FILE_NAME);
        fs::write(
            &config_path,
            "schema_version = 1\n\n[ui]\nlast_capture_tool = \"region\"\n\n\
             [ui.displays.\"display-1\"]\noverlay_center_x = 0.5\noverlay_center_y = 0.5\n",
        )
        .expect("seed a pre-split configuration");
        let store = UiStateStore::for_config(&config_path);

        // Reading alone migrates in memory without writing anything.
        let migrated = store.load().expect("load migrates");
        assert_eq!(migrated.last_capture_tool, Some(CaptureTool::Region));
        assert_eq!(
            resolve_display_ui_state(&migrated, "display-1").overlay_center,
            Some((0.5, 0.5))
        );
        assert!(!directory.join(STATE_FILE_NAME).exists());

        // The first write persists the migrated state alongside the new value.
        store
            .save_display_overlay_center("display-2", 0.1, 0.2)
            .expect("save");
        let state = store.load().expect("load after write");
        assert_eq!(state.last_capture_tool, Some(CaptureTool::Region));
        assert_eq!(
            resolve_display_ui_state(&state, "display-1").overlay_center,
            Some((0.5, 0.5))
        );
        assert_eq!(
            resolve_display_ui_state(&state, "display-2").overlay_center,
            Some((0.1, 0.2))
        );
    }

    #[test]
    fn an_unreadable_configuration_costs_no_state_and_no_error() {
        let directory = TestDirectory::new("damaged-config");
        let config_path = directory.join(CONFIG_FILE_NAME);
        fs::write(&config_path, "this is not = = valid toml\n").expect("seed damaged config");
        let store = UiStateStore::for_config(&config_path);

        assert!(store
            .load()
            .expect("damaged config still loads state")
            .is_empty());
        store
            .save_display_overlay_center("display-1", 0.5, 0.5)
            .expect("save still succeeds");
    }

    #[test]
    fn independent_updates_to_the_same_file_all_survive() {
        // M36: each save is a read-modify-write of the whole file, so without exclusion the last
        // writer would rewrite it from a copy taken before the others landed.
        let directory = TestDirectory::new("concurrent");
        let state_path = directory.join(STATE_FILE_NAME);

        thread::scope(|scope| {
            for index in 0..8_u32 {
                let state_path = state_path.clone();
                scope.spawn(move || {
                    let store = UiStateStore::at(&state_path);
                    store
                        .save_display_overlay_center(
                            &format!("display-{index}"),
                            f64::from(index) / 10.0,
                            0.5,
                        )
                        .expect("concurrent save");
                });
            }
        });

        let state = UiStateStore::at(&state_path).load().expect("load");
        assert_eq!(state.displays.len(), 8, "an update was lost: {state:?}");
        for index in 0..8_u32 {
            assert_eq!(
                resolve_display_ui_state(&state, &format!("display-{index}")).overlay_center,
                Some((f64::from(index) / 10.0, 0.5))
            );
        }
    }

    #[test]
    fn a_stale_lock_is_broken_rather_than_blocking_forever() {
        let directory = TestDirectory::new("stale-lock");
        let state_path = directory.join(STATE_FILE_NAME);
        let lock = lock_path(&state_path);
        fs::write(&lock, "99999\n").expect("plant a lock");

        // Fresh: the lock stands, and a writer gives up rather than stealing it.
        assert!(!break_stale_lock(&lock));
        let error = UiStateStore::at(&state_path)
            .save_display_overlay_center("display-1", 0.5, 0.5)
            .expect_err("a held lock blocks the write");
        assert!(error.to_string().contains("UI state lock"), "{error}");
        assert!(lock.exists());

        // Backdated past the staleness window: the abandoned lock is cleared and the write lands.
        let stale = std::time::SystemTime::now() - LOCK_STALE_AFTER - Duration::from_secs(60);
        set_modified(&lock, stale);
        UiStateStore::at(&state_path)
            .save_display_overlay_center("display-1", 0.5, 0.5)
            .expect("a stale lock is broken");
        assert!(!lock.exists(), "the lock must be released after the write");
    }

    fn set_modified(path: &Path, when: std::time::SystemTime) {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open lock file");
        file.set_modified(when).expect("backdate the lock file");
    }

    #[test]
    fn the_lock_is_released_even_when_the_write_fails() {
        let directory = TestDirectory::new("lock-release");
        // A directory where the state file should be: every write fails, but the lock must not
        // survive the failure or the next attempt would wait two seconds for nothing.
        let state_path = directory.join(STATE_FILE_NAME);
        fs::create_dir_all(&state_path).expect("create a directory in the state file's place");

        let store = UiStateStore::at(&state_path);
        assert!(store
            .save_display_overlay_center("display-1", 0.5, 0.5)
            .is_err());
        assert!(!lock_path(&state_path).exists());
    }

    #[test]
    fn invalid_values_are_rejected_before_anything_is_written() {
        let directory = TestDirectory::new("validation");
        let state_path = directory.join(STATE_FILE_NAME);
        let store = UiStateStore::at(&state_path);

        for (center_x, center_y) in [
            (-0.1, 0.5),
            (1.1, 0.5),
            (f64::NAN, 0.5),
            (0.5, f64::INFINITY),
        ] {
            assert!(matches!(
                store.save_display_overlay_center("display-1", center_x, center_y),
                Err(ConfigError::InvalidValue(_))
            ));
        }
        assert!(matches!(
            store.save_display_confirmed_region("display-1", region(0, 10), region_source(10, 10)),
            Err(ConfigError::InvalidValue(_))
        ));
        assert!(matches!(
            store.save_display_interaction_state(
                "display-1",
                CaptureTool::Region,
                Some(region(10, 0)),
                None
            ),
            Err(ConfigError::InvalidValue(_))
        ));
        assert!(
            !state_path.exists(),
            "a rejected value must not create a state file"
        );
    }

    #[test]
    fn state_from_a_newer_schema_is_reported_rather_than_silently_reset() {
        let directory = TestDirectory::new("state-schema");
        let state_path = directory.join(STATE_FILE_NAME);
        fs::write(
            &state_path,
            format!("schema_version = {}\n", STATE_SCHEMA_VERSION + 1),
        )
        .expect("write newer state");

        assert!(matches!(
            UiStateStore::at(&state_path).load(),
            Err(ConfigError::UnsupportedSchema(version)) if version == STATE_SCHEMA_VERSION + 1
        ));
    }

    #[test]
    fn confirmed_regions_are_collected_per_display() {
        let directory = TestDirectory::new("confirmed");
        let store = UiStateStore::at(directory.join(STATE_FILE_NAME));
        store
            .save_display_confirmed_region("display-1", region(10, 20), region_source(100, 200))
            .expect("save confirmed region");
        store
            .save_display_interaction_state("display-2", CaptureTool::Window, None, None)
            .expect("save interaction without a confirmed region");

        let regions = store
            .confirmed_regions()
            .expect("collect confirmed regions");
        assert_eq!(regions.len(), 1);
        assert_eq!(regions["display-1"].region, region(10, 20));
    }
}
