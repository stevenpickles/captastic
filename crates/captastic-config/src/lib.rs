#![deny(unsafe_code)]

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

mod fsio;
mod history;
mod ui_state;

use fsio::{maintain_config_artifacts, quarantine_config};

pub use fsio::{atomic_write, finalize_new, replace_file};
pub use history::{
    CaptureHistory, HistoryEntry, HistoryStore, RetentionPolicy, HISTORY_FILE_NAME,
    HISTORY_SCHEMA_VERSION,
};
pub use ui_state::{
    resolve_display_ui_state, UiState, UiStateStore, STATE_FILE_NAME, STATE_SCHEMA_VERSION,
};

pub const CONFIG_SCHEMA_VERSION: u32 = 1;
pub const CONFIG_FILE_NAME: &str = "captastic.toml";

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyAction {
    LastWorkflow,
    Region,
    Window,
    FullDisplay,
    RepeatLastRegion,
}

impl HotkeyAction {
    pub const ALL: [Self; 5] = [
        Self::LastWorkflow,
        Self::Region,
        Self::Window,
        Self::FullDisplay,
        Self::RepeatLastRegion,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LastWorkflow => "last_workflow",
            Self::Region => "region",
            Self::Window => "window",
            Self::FullDisplay => "full_display",
            Self::RepeatLastRegion => "repeat_last_region",
        }
    }

    pub const fn registration_index(self) -> i32 {
        self as i32
    }
}

impl fmt::Display for HotkeyAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HotkeyKey {
    Letter(u8),
    Digit(u8),
    Function(u8),
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HotkeyModifiers(u8);

impl HotkeyModifiers {
    const CTRL: u8 = 1 << 0;
    const ALT: u8 = 1 << 1;
    const SHIFT: u8 = 1 << 2;
    const WIN: u8 = 1 << 3;

    pub const fn ctrl(self) -> bool {
        self.0 & Self::CTRL != 0
    }
    pub const fn alt(self) -> bool {
        self.0 & Self::ALT != 0
    }
    pub const fn shift(self) -> bool {
        self.0 & Self::SHIFT != 0
    }
    pub const fn win(self) -> bool {
        self.0 & Self::WIN != 0
    }

    fn insert(&mut self, flag: u8, label: &str) -> Result<(), HotkeyParseError> {
        if self.0 & flag != 0 {
            return Err(HotkeyParseError::DuplicateModifier(label.to_owned()));
        }
        self.0 |= flag;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HotkeyChord {
    modifiers: HotkeyModifiers,
    key: HotkeyKey,
}

impl HotkeyChord {
    pub const fn modifiers(self) -> HotkeyModifiers {
        self.modifiers
    }
    pub const fn key(self) -> HotkeyKey {
        self.key
    }
}

impl FromStr for HotkeyChord {
    type Err = HotkeyParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        if value.is_empty() {
            return Err(HotkeyParseError::Empty);
        }
        let mut modifiers = HotkeyModifiers::default();
        let mut key = None;
        for token in value.split('+') {
            let token = token.trim();
            if token.is_empty() {
                return Err(HotkeyParseError::EmptyToken);
            }
            let modifier = if token.eq_ignore_ascii_case("ctrl")
                || token.eq_ignore_ascii_case("control")
            {
                Some((HotkeyModifiers::CTRL, "Ctrl"))
            } else if token.eq_ignore_ascii_case("alt") {
                Some((HotkeyModifiers::ALT, "Alt"))
            } else if token.eq_ignore_ascii_case("shift") {
                Some((HotkeyModifiers::SHIFT, "Shift"))
            } else if token.eq_ignore_ascii_case("win") || token.eq_ignore_ascii_case("windows") {
                Some((HotkeyModifiers::WIN, "Win"))
            } else {
                None
            };
            if let Some((flag, label)) = modifier {
                modifiers.insert(flag, label)?;
                continue;
            }
            if key.is_some() {
                return Err(HotkeyParseError::MultipleKeys);
            }
            key = Some(parse_hotkey_key(token)?);
        }
        Ok(Self {
            modifiers,
            key: key.ok_or(HotkeyParseError::MissingKey)?,
        })
    }
}

impl fmt::Display for HotkeyChord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for (enabled, label) in [
            (self.modifiers.ctrl(), "Ctrl"),
            (self.modifiers.alt(), "Alt"),
            (self.modifiers.shift(), "Shift"),
            (self.modifiers.win(), "Win"),
        ] {
            if enabled {
                if !first {
                    formatter.write_str("+")?;
                }
                formatter.write_str(label)?;
                first = false;
            }
        }
        if !first {
            formatter.write_str("+")?;
        }
        match self.key {
            HotkeyKey::Letter(letter) | HotkeyKey::Digit(letter) => {
                formatter.write_str(&char::from(letter).to_string())
            }
            HotkeyKey::Function(number) => write!(formatter, "F{number}"),
        }
    }
}

fn parse_hotkey_key(token: &str) -> Result<HotkeyKey, HotkeyParseError> {
    let bytes = token.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_alphabetic() {
        return Ok(HotkeyKey::Letter(bytes[0].to_ascii_uppercase()));
    }
    if bytes.len() == 1 && bytes[0].is_ascii_digit() {
        return Ok(HotkeyKey::Digit(bytes[0]));
    }
    if token.len() >= 2 && token.as_bytes()[0].eq_ignore_ascii_case(&b'f') {
        let number_text = &token[1..];
        if number_text.len() <= 2 && !number_text.starts_with('0') {
            if let Ok(number) = number_text.parse::<u8>() {
                if (1..=24).contains(&number) {
                    return Ok(HotkeyKey::Function(number));
                }
            }
        }
    }
    Err(HotkeyParseError::UnsupportedKey(token.to_owned()))
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HotkeyParseError {
    #[error("hotkey binding must not be empty")]
    Empty,
    #[error("hotkey binding contains an empty token")]
    EmptyToken,
    #[error("hotkey binding repeats modifier {0}")]
    DuplicateModifier(String),
    #[error("hotkey binding must contain exactly one non-modifier key")]
    MissingKey,
    #[error("hotkey binding contains multiple non-modifier keys")]
    MultipleKeys,
    #[error("unsupported hotkey key {0:?}; use A-Z, 0-9, or F1-F24")]
    UnsupportedKey(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HotkeyBinding {
    pub action: HotkeyAction,
    pub chord: HotkeyChord,
}
/// Returns the per-user directory for Captastic configuration, state, and logs.
pub fn storage_directory() -> Option<PathBuf> {
    storage_directory_from(
        env::var_os("USERPROFILE").map(PathBuf::from),
        env::var_os("HOME").map(PathBuf::from),
        cfg!(windows),
    )
}

pub fn default_config_path() -> Option<PathBuf> {
    storage_directory().map(|path| path.join(CONFIG_FILE_NAME))
}

pub fn ensure_default_config() -> Result<PathBuf, ConfigError> {
    let path = default_config_path().ok_or(ConfigError::HomeDirectoryUnavailable)?;
    if path.exists() {
        return Ok(path);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: parent.display().to_string(),
            source,
        })?;
    }
    let contents = AppConfig::default().to_toml_pretty()?;
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            file.write_all(contents.as_bytes())
                .map_err(|source| ConfigError::Write {
                    path: path.display().to_string(),
                    source,
                })?
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(ConfigError::Write {
                path: path.display().to_string(),
                source,
            });
        }
    }
    Ok(path)
}

/// The user's home directory, by the same rules `storage_directory` uses.
fn home_directory() -> Option<PathBuf> {
    home_directory_from(
        env::var_os("USERPROFILE").map(PathBuf::from),
        env::var_os("HOME").map(PathBuf::from),
        cfg!(windows),
    )
}

fn home_directory_from(
    user_profile: Option<PathBuf>,
    home: Option<PathBuf>,
    windows: bool,
) -> Option<PathBuf> {
    let (primary, fallback) = if windows {
        (user_profile, home)
    } else {
        (home, user_profile)
    };
    primary
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| fallback.filter(|path| !path.as_os_str().is_empty()))
}

fn storage_directory_from(
    user_profile: Option<PathBuf>,
    home: Option<PathBuf>,
    windows: bool,
) -> Option<PathBuf> {
    home_directory_from(user_profile, home, windows).map(|path| path.join(".captastic"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureTool {
    FullDisplay,
    Window,
    Region,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRegion {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureRegionSource {
    pub width: u32,
    pub height: u32,
    pub rotation_degrees: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfirmedRegion {
    pub region: CaptureRegion,
    pub source: CaptureRegionSource,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DisplayUiState {
    /// Normalized toolbar center within the display work area.
    pub overlay_center: Option<(f64, f64)>,
    /// Backward-compatible monitor-local origin written by earlier alpha builds.
    pub overlay_position: Option<(i32, i32)>,
    pub tool: Option<CaptureTool>,
    pub region: Option<CaptureRegion>,
    pub region_source: Option<CaptureRegionSource>,
    /// Per-display regions are monitor-local. A legacy global fallback remains desktop-absolute.
    pub confirmed_region: Option<ConfirmedRegion>,
    pub region_is_display_local: bool,
}

pub(crate) fn prepare_config_path_for_open(
    path: &Path,
    default_path: Option<&Path>,
    ensure_default: impl FnOnce() -> Result<PathBuf, ConfigError>,
) -> Result<PathBuf, ConfigError> {
    if path.exists() || default_path != Some(path) {
        Ok(path.to_path_buf())
    } else {
        ensure_default()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub schema_version: u32,
    pub daemon: DaemonConfig,
    pub hotkey: HotkeyConfig,
    pub capture: CaptureConfig,
    pub selection: SelectionConfig,
    pub clipboard: ClipboardConfig,
    pub output: OutputConfig,
    pub metrics: MetricsConfig,
    pub logging: LoggingConfig,
    pub history: HistoryConfig,
    pub ui: UiConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            daemon: DaemonConfig::default(),
            hotkey: HotkeyConfig::default(),
            capture: CaptureConfig::default(),
            selection: SelectionConfig::default(),
            clipboard: ClipboardConfig::default(),
            output: OutputConfig::default(),
            metrics: MetricsConfig::default(),
            logging: LoggingConfig::default(),
            history: HistoryConfig::default(),
            ui: UiConfig::default(),
        }
    }
}

/// Reads nothing but `schema_version`, tolerating every field around it.
///
/// `AppConfig` is `deny_unknown_fields`, which is what makes a typo a startup error rather than a
/// silently ignored setting. It also means a configuration written by a *newer* Captastic fails
/// during deserialization, complaining about whichever new key it happened to reach first — so
/// `UnsupportedSchema`, the error that exists to explain exactly this, could never fire. Reading
/// the version on its own, leniently, lets it speak before the strict parse gets a chance to
/// mis-describe the problem.
#[derive(Deserialize)]
struct SchemaProbe {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
}

const fn default_schema_version() -> u32 {
    CONFIG_SCHEMA_VERSION
}

/// Fails with `UnsupportedSchema` when `text` declares a schema this binary does not implement.
///
/// A missing `schema_version` is treated as the current one: that is what a hand-written partial
/// configuration looks like, and rejecting those would be a regression.
fn check_schema_version(text: &str) -> Result<(), ConfigError> {
    let probe: SchemaProbe = toml::from_str(text)?;
    if probe.schema_version != CONFIG_SCHEMA_VERSION {
        return Err(ConfigError::UnsupportedSchema(probe.schema_version));
    }
    Ok(())
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        check_schema_version(&text)?;
        let config: Self = toml::from_str(&text)?;
        config.validate()?;
        Ok(config)
    }

    pub fn load_default() -> Result<Self, ConfigError> {
        let Some(path) = default_config_path() else {
            return Ok(Self::default());
        };
        Self::load_optional(&path)
    }

    /// Loads the default configuration, quarantining syntactically damaged TOML so startup can
    /// continue with safe defaults. Explicitly supplied configuration files remain strict.
    pub fn load_default_recovering() -> Result<(Self, Option<ConfigRecovery>), ConfigError> {
        let Some(path) = default_config_path() else {
            return Ok((Self::default(), None));
        };
        Self::load_recovering(&path)
    }

    fn load_optional(path: &Path) -> Result<Self, ConfigError> {
        match Self::load(path) {
            Ok(config) => Ok(config),
            Err(ConfigError::Read { source, .. }) if source.kind() == ErrorKind::NotFound => {
                Ok(Self::default())
            }
            Err(error) => Err(error),
        }
    }

    fn load_recovering(path: &Path) -> Result<(Self, Option<ConfigRecovery>), ConfigError> {
        maintain_config_artifacts(path, None);
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == ErrorKind::NotFound => {
                return Ok((Self::default(), None));
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) => {
                return quarantine_damaged_config(path, error.to_string());
            }
        };
        if let Err(syntax_error) = text.parse::<toml_edit::Document>() {
            return quarantine_damaged_config(path, syntax_error.to_string());
        }
        // Before the strict parse, so a configuration from a newer Captastic is reported as a
        // version this binary cannot read rather than as an unknown key. Deliberately *not*
        // quarantined: the file is not damaged, this binary is simply too old for it, and moving
        // it aside would destroy a working configuration for whichever install wrote it.
        check_schema_version(&text)?;
        let config: Self = toml::from_str(&text)?;
        config.validate()?;
        Ok((config, None))
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchema(self.schema_version));
        }
        validate_capacity(
            "daemon.trigger_queue_capacity",
            self.daemon.trigger_queue_capacity,
        )?;
        if !matches!(
            self.daemon.display.as_str(),
            "primary" | "pointer" | "virtual_desktop"
        ) && self
            .daemon
            .display
            .strip_prefix("display:")
            .is_none_or(|id| id.trim().is_empty())
        {
            return Err(ConfigError::InvalidValue(
                "daemon.display must be pointer, primary, virtual_desktop, or display:<persistent-id>".to_owned(),
            ));
        }
        validate_capacity("selection.queue_capacity", self.selection.queue_capacity)?;
        validate_capacity("clipboard.queue_capacity", self.clipboard.queue_capacity)?;
        validate_capacity("output.queue_capacity", self.output.queue_capacity)?;
        if let Some(directory) = self.output.directory.as_deref() {
            if directory.as_os_str().is_empty() {
                return Err(ConfigError::InvalidValue(
                    "output.directory must not be empty".to_owned(),
                ));
            }
            // A daemon's working directory is whatever launched it, so a relative path would put
            // captures somewhere the user cannot predict and would move between launches.
            if !directory.is_absolute() {
                return Err(ConfigError::InvalidValue(format!(
                    "output.directory must be an absolute path, got {}",
                    directory.display()
                )));
            }
        }
        if !matches!(self.output.format.as_str(), "png") {
            return Err(ConfigError::InvalidValue(
                "output.format must be png".to_owned(),
            ));
        }
        if self.metrics.ring_capacity == 0 || self.metrics.ring_capacity > 10_000_000 {
            return Err(ConfigError::InvalidValue(
                "metrics.ring_capacity must be between 1 and 10000000".to_owned(),
            ));
        }
        if self.capture.fake_width == 0
            || self.capture.fake_height == 0
            || self.capture.fake_width > 16_384
            || self.capture.fake_height > 16_384
        {
            return Err(ConfigError::InvalidValue(
                "fake frame dimensions must be between 1 and 16384".to_owned(),
            ));
        }
        if self.capture.buffer_slots != 3 {
            return Err(ConfigError::InvalidValue(
                "capture.buffer_slots: the CPU readback pool is currently fixed at three slots; remove the override".to_owned(),
            ));
        }
        if !matches!(self.capture.mode.as_str(), "fresh" | "latest") {
            return Err(ConfigError::InvalidValue(
                "capture.mode must be fresh or latest".to_owned(),
            ));
        }
        if !matches!(self.capture.cursor.as_str(), "include" | "exclude") {
            return Err(ConfigError::InvalidValue(
                "capture.cursor must be include or exclude".to_owned(),
            ));
        }
        if !matches!(self.hotkey.repeat.as_str(), "ignore" | "coalesce") {
            return Err(ConfigError::InvalidValue(
                "hotkey.repeat must be ignore or coalesce".to_owned(),
            ));
        }
        if self.hotkey.repeat == "coalesce" {
            return Err(ConfigError::InvalidValue(
                "hotkey.repeat: coalesce is not implemented yet (roadmap milestone 4); hotkey.repeat must be ignore or coalesce, but only ignore is currently supported".to_owned(),
            ));
        }
        self.hotkey.resolved_bindings()?;
        if !matches!(self.logging.format.as_str(), "compact" | "json") {
            return Err(ConfigError::InvalidValue(
                "logging.format must be compact or json".to_owned(),
            ));
        }
        if !matches!(
            self.logging.level.as_str(),
            "off" | "error" | "warn" | "info" | "debug" | "trace"
        ) {
            return Err(ConfigError::InvalidValue(
                "logging.level must be off, error, warn, info, debug, or trace".to_owned(),
            ));
        }
        if self
            .logging
            .file
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(ConfigError::InvalidValue(
                "logging.file must not be empty".to_owned(),
            ));
        }
        if !(1_024..=1_073_741_824).contains(&self.logging.max_file_bytes) {
            return Err(ConfigError::InvalidValue(
                "logging.max_file_bytes must be between 1024 and 1073741824".to_owned(),
            ));
        }
        if self.history.max_items > 10_000 {
            return Err(ConfigError::InvalidValue(
                "history.max_items must be 10000 or fewer".to_owned(),
            ));
        }
        if self.history.max_age_days > 3_650 {
            return Err(ConfigError::InvalidValue(
                "history.max_age_days must be 3650 or fewer".to_owned(),
            ));
        }
        if !(1..=20).contains(&self.logging.retained_files) {
            return Err(ConfigError::InvalidValue(
                "logging.retained_files must be between 1 and 20".to_owned(),
            ));
        }
        if self.ui.overlay_x.is_some() != self.ui.overlay_y.is_some() {
            return Err(ConfigError::InvalidValue(
                "ui.overlay_x and ui.overlay_y must either both be set or both be omitted"
                    .to_owned(),
            ));
        }
        if self
            .ui
            .last_region
            .is_some_and(|region| region.width == 0 || region.height == 0)
        {
            return Err(ConfigError::InvalidValue(
                "ui.last_region width and height must be greater than zero".to_owned(),
            ));
        }
        validate_display_entries(&self.ui.displays)?;
        Ok(())
    }

    pub fn to_toml_pretty(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(ConfigError::Serialize)
    }
}

/// Validates the per-display entries shared by the configuration's legacy `[ui]` section and
/// Captastic's own state file.
///
/// Both carry the same `DisplayUiConfig` values and both can be wrong in the same ways: the
/// configuration because a user hand-edited it, the state file because a previous version wrote
/// something this one rejects. One function so the two cannot drift apart.
pub(crate) fn validate_display_entries(
    displays: &BTreeMap<String, DisplayUiConfig>,
) -> Result<(), ConfigError> {
    for (display_id, state) in displays {
        if display_id.trim().is_empty() {
            return Err(ConfigError::InvalidValue(
                "ui.displays keys must not be empty".to_owned(),
            ));
        }
        if state.overlay_x.is_some() != state.overlay_y.is_some() {
            return Err(ConfigError::InvalidValue(format!(
                "ui.displays.{display_id}.overlay_x and overlay_y must either both be set or both be omitted"
            )));
        }
        if state.overlay_center_x.is_some() != state.overlay_center_y.is_some() {
            return Err(ConfigError::InvalidValue(format!(
                "ui.displays.{display_id}.overlay_center_x and overlay_center_y must either both be set or both be omitted"
            )));
        }
        if state
            .overlay_center_x
            .zip(state.overlay_center_y)
            .is_some_and(|(x, y)| {
                !x.is_finite()
                    || !y.is_finite()
                    || !(0.0..=1.0).contains(&x)
                    || !(0.0..=1.0).contains(&y)
            })
        {
            return Err(ConfigError::InvalidValue(format!(
                "ui.displays.{display_id} overlay center coordinates must be finite values between 0 and 1"
            )));
        }
        if state
            .last_region
            .is_some_and(|region| region.width == 0 || region.height == 0)
        {
            return Err(ConfigError::InvalidValue(format!(
                "ui.displays.{display_id}.last_region width and height must be greater than zero"
            )));
        }
        if let Some(source) = state.last_region_source {
            if source.width == 0 || source.height == 0 {
                return Err(ConfigError::InvalidValue(format!(
                    "ui.displays.{display_id}.last_region_source dimensions must be greater than zero"
                )));
            }
            if !matches!(source.rotation_degrees, 0 | 90 | 180 | 270) {
                return Err(ConfigError::InvalidValue(format!(
                    "ui.displays.{display_id}.last_region_source rotation must be 0, 90, 180, or 270 degrees"
                )));
            }
            if state.last_region.is_none() {
                return Err(ConfigError::InvalidValue(format!(
                    "ui.displays.{display_id}.last_region_source requires last_region"
                )));
            }
        }
        match (
            state.last_confirmed_region,
            state.last_confirmed_region_source,
        ) {
            (Some(region), Some(source)) => {
                let right = i64::from(region.x) + i64::from(region.width);
                let bottom = i64::from(region.y) + i64::from(region.height);
                if region.x < 0
                    || region.y < 0
                    || region.width == 0
                    || region.height == 0
                    || source.width == 0
                    || source.height == 0
                    || right > i64::from(source.width)
                    || bottom > i64::from(source.height)
                {
                    return Err(ConfigError::InvalidValue(format!(
                        "ui.displays.{display_id}.last_confirmed_region must fit its source geometry"
                    )));
                }
                if !matches!(source.rotation_degrees, 0 | 90 | 180 | 270) {
                    return Err(ConfigError::InvalidValue(format!(
                        "ui.displays.{display_id}.last_confirmed_region_source rotation must be 0, 90, 180, or 270 degrees"
                    )));
                }
            }
            (None, None) => {}
            _ => {
                return Err(ConfigError::InvalidValue(format!(
                    "ui.displays.{display_id} confirmed region and source must both be set or both be omitted"
                )));
            }
        }
    }
    Ok(())
}

fn quarantine_damaged_config(
    path: &Path,
    reason: String,
) -> Result<(AppConfig, Option<ConfigRecovery>), ConfigError> {
    let quarantined_path = quarantine_config(path).map_err(|source| ConfigError::Quarantine {
        path: path.display().to_string(),
        source,
    })?;
    let Some(quarantined_path) = quarantined_path else {
        return Ok((AppConfig::default(), None));
    };
    Ok((
        AppConfig::default(),
        Some(ConfigRecovery {
            original_path: path.to_path_buf(),
            quarantined_path,
            reason,
        }),
    ))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    pub backend: String,
    pub display: String,
    pub trigger_queue_capacity: usize,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            backend: "dxgi".to_owned(),
            display: "pointer".to_owned(),
            trigger_queue_capacity: 4,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HotkeyConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    pub repeat: String,
    #[serde(
        default = "HotkeyBindingsConfig::disabled",
        skip_serializing_if = "HotkeyBindingsConfig::is_empty"
    )]
    pub bindings: HotkeyBindingsConfig,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            binding: None,
            repeat: "ignore".to_owned(),
            bindings: HotkeyBindingsConfig {
                last_workflow: Some("Ctrl+Shift+F9".to_owned()),
                ..HotkeyBindingsConfig::disabled()
            },
        }
    }
}

impl HotkeyConfig {
    pub fn resolved_bindings(&self) -> Result<Vec<HotkeyBinding>, ConfigError> {
        if self.binding.is_some() && self.bindings.last_workflow.is_some() {
            return Err(ConfigError::InvalidValue(
                "hotkey.binding and hotkey.bindings.last_workflow define the same action; remove one"
                    .to_owned(),
            ));
        }

        let mut bindings = Vec::with_capacity(HotkeyAction::ALL.len());
        let mut chords = BTreeMap::new();
        for action in HotkeyAction::ALL {
            let value = if action == HotkeyAction::LastWorkflow {
                self.bindings
                    .last_workflow
                    .as_deref()
                    .or(self.binding.as_deref())
            } else {
                self.bindings.value(action)
            };
            let Some(value) = value else { continue };
            let chord = value.parse::<HotkeyChord>().map_err(|error| {
                ConfigError::InvalidValue(format!(
                    "hotkey.bindings.{} is invalid: {error}",
                    action.as_str()
                ))
            })?;
            if let Some(previous) = chords.insert(chord, action) {
                return Err(ConfigError::InvalidValue(format!(
                    "hotkey chord {chord} is assigned to both {previous} and {action}"
                )));
            }
            bindings.push(HotkeyBinding { action, chord });
        }
        Ok(bindings)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HotkeyBindingsConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_workflow: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_display: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat_last_region: Option<String>,
}

impl HotkeyBindingsConfig {
    const fn disabled() -> Self {
        Self {
            last_workflow: None,
            region: None,
            window: None,
            full_display: None,
            repeat_last_region: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.last_workflow.is_none()
            && self.region.is_none()
            && self.window.is_none()
            && self.full_display.is_none()
            && self.repeat_last_region.is_none()
    }

    fn value(&self, action: HotkeyAction) -> Option<&str> {
        match action {
            HotkeyAction::LastWorkflow => self.last_workflow.as_deref(),
            HotkeyAction::Region => self.region.as_deref(),
            HotkeyAction::Window => self.window.as_deref(),
            HotkeyAction::FullDisplay => self.full_display.as_deref(),
            HotkeyAction::RepeatLastRegion => self.repeat_last_region.as_deref(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    pub mode: String,
    pub fresh_timeout_ms: u64,
    pub max_frame_age_ms: u64,
    pub cpu_frame: bool,
    pub cursor: String,
    pub buffer_slots: usize,
    pub fake_width: u32,
    pub fake_height: u32,
    pub fake_native_delay_us: u64,
    pub fake_readback_delay_us: u64,
    pub fake_frame_age_us: u64,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            mode: "latest".to_owned(),
            fresh_timeout_ms: 100,
            max_frame_age_ms: 0,
            cpu_frame: true,
            cursor: "exclude".to_owned(),
            buffer_slots: 3,
            fake_width: 64,
            fake_height: 64,
            fake_native_delay_us: 250,
            fake_readback_delay_us: 250,
            fake_frame_age_us: 1_000,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClipboardConfig {
    pub enabled: bool,
    pub queue_capacity: usize,
    /// Whether a capture may be kept in the Win+V clipboard history.
    ///
    /// Off by default. A screenshot is whatever happened to be on screen, and history keeps it
    /// reachable long after the paste it was taken for.
    pub allow_history: bool,
    /// Whether a capture may be synced to the signed-in Microsoft account, and from there to that
    /// account's other machines.
    ///
    /// Off by default, and the more consequential of the two: local history is a convenience a
    /// user can restore, while a capture that has left the machine cannot be recalled.
    pub allow_cloud_sync: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewMode {
    #[default]
    Auto,
    Live,
    Frozen,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SelectionConfig {
    pub enabled: bool,
    pub queue_capacity: usize,
    pub preview: PreviewMode,
}

impl Default for SelectionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            queue_capacity: 1,
            preview: PreviewMode::Auto,
        }
    }
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            queue_capacity: 1,
            // Both retention paths are declined by default. See ADR 0008: the two mistakes are not
            // symmetrical, so the default is the one whose consequences a user can undo.
            allow_history: false,
            allow_cloud_sync: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    pub enabled: bool,
    pub format: String,
    pub queue_capacity: usize,
    /// Where captures are written. `None` selects [`default_output_directory`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<PathBuf>,
    /// Names a capture, without its extension. See `DEFAULT_FILENAME_TEMPLATE`.
    pub filename_template: String,
}

/// The default capture name: sortable, and identical for every capture but the moment it was taken.
pub const DEFAULT_FILENAME_TEMPLATE: &str = "captastic-{timestamp}";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistoryConfig {
    /// How many recent captures to remember. Zero forgets everything, which is how history is
    /// turned off — the recording path stays live, so nothing has to check a flag first.
    pub max_items: usize,
    /// Forget captures older than this. Zero means age is not a reason to forget.
    pub max_age_days: u32,
    /// Forget the oldest once the remembered captures exceed this many bytes. Zero means size is
    /// not a reason to forget.
    pub max_total_bytes: u64,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            // Enough to find something from this week's work; small enough that the file stays
            // trivial to read and rewrite on every capture.
            max_items: 50,
            max_age_days: 30,
            max_total_bytes: 0,
        }
    }
}

impl HistoryConfig {
    pub fn retention(&self) -> RetentionPolicy {
        RetentionPolicy {
            max_items: self.max_items,
            max_age: (self.max_age_days > 0)
                .then(|| Duration::from_secs(u64::from(self.max_age_days) * 24 * 60 * 60)),
            max_total_bytes: (self.max_total_bytes > 0).then_some(self.max_total_bytes),
        }
    }
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            format: "png".to_owned(),
            queue_capacity: 2,
            directory: None,
            filename_template: DEFAULT_FILENAME_TEMPLATE.to_owned(),
        }
    }
}

/// Where captures land when the user has not said otherwise.
///
/// Beside the pictures a person already has, rather than in `.captastic` beside Captastic's own
/// files: a screenshot is the user's document, not application state.
pub fn default_output_directory() -> Option<PathBuf> {
    home_directory().map(|home| home.join("Pictures").join("Captastic"))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    pub enabled: bool,
    pub ring_capacity: usize,
    pub raw_events: bool,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ring_capacity: 10_000,
            raw_events: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: String,
    pub format: String,
    pub file: Option<PathBuf>,
    pub max_file_bytes: u64,
    pub retained_files: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
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

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DisplayUiConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_center_x: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_center_y: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_y: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_capture_tool: Option<CaptureTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_region: Option<CaptureRegion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_region_source: Option<CaptureRegionSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_confirmed_region: Option<CaptureRegion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_confirmed_region_source: Option<CaptureRegionSource>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_owned(),
            format: "compact".to_owned(),
            file: None,
            max_file_bytes: 5 * 1_024 * 1_024,
            retained_files: 3,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("unable to determine the user home directory from USERPROFILE or HOME")]
    HomeDirectoryUnavailable,
    #[error("failed to read configuration {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse configuration: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("failed to edit configuration: {0}")]
    Edit(#[from] toml_edit::TomlError),
    #[error("failed to serialize configuration: {0}")]
    Serialize(toml::ser::Error),
    #[error("failed to write configuration {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to quarantine damaged configuration {path}: {source}")]
    Quarantine {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("unsupported configuration schema version {0}")]
    UnsupportedSchema(u32),
    #[error("invalid configuration: {0}")]
    InvalidValue(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigRecovery {
    pub original_path: PathBuf,
    pub quarantined_path: PathBuf,
    pub reason: String,
}

fn validate_capacity(name: &str, capacity: usize) -> Result<(), ConfigError> {
    if capacity == 0 || capacity > 1_024 {
        return Err(ConfigError::InvalidValue(format!(
            "{name} must be between 1 and 1024"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("current time")
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "captastic-config-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create isolated test directory");
        path
    }

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
            fs::remove_dir_all(&self.0).expect("remove isolated test directory");
        }
    }

    #[test]
    fn defaults_are_valid() {
        let config = AppConfig::default();
        config.validate().expect("valid defaults");
        assert_eq!(config.selection.preview, PreviewMode::Auto);
    }

    #[test]
    fn a_newer_schema_reports_its_version_rather_than_an_unknown_key() {
        // The regression: `deny_unknown_fields` rejected the newer binary's fields first, so the
        // user was told `unknown field "future_setting"` — a typo's error message — for a
        // configuration that is not wrong, merely from the future.
        let directory = TestDirectory::new("newer-schema");
        let path = directory.join(CONFIG_FILE_NAME);
        fs::write(
            &path,
            format!(
                "schema_version = {}\n\n[future]\nfuture_setting = true\n",
                CONFIG_SCHEMA_VERSION + 1
            ),
        )
        .expect("write a newer configuration");

        for error in [
            AppConfig::load(&path).expect_err("explicit load rejects a newer schema"),
            AppConfig::load_recovering(&path).expect_err("recovering load rejects a newer schema"),
        ] {
            assert!(
                matches!(error, ConfigError::UnsupportedSchema(version)
                    if version == CONFIG_SCHEMA_VERSION + 1),
                "expected a versioned error, got {error}"
            );
        }
        // A newer configuration is not damaged, so nothing may be moved aside: the install that
        // wrote it still needs it.
        assert!(path.exists());
        assert_eq!(
            fs::read_dir(&directory.0).expect("list directory").count(),
            1,
            "a newer configuration must not be quarantined"
        );
    }

    #[test]
    fn an_older_schema_also_reports_its_version() {
        let directory = TestDirectory::new("older-schema");
        let path = directory.join(CONFIG_FILE_NAME);
        fs::write(&path, "schema_version = 0\n").expect("write an older configuration");

        assert!(matches!(
            AppConfig::load(&path).expect_err("older schema is rejected"),
            ConfigError::UnsupportedSchema(0)
        ));
    }

    #[test]
    fn an_omitted_schema_version_is_treated_as_current() {
        // A hand-written partial configuration has no version line; requiring one would turn
        // every such file into a startup failure.
        let directory = TestDirectory::new("implicit-schema");
        let path = directory.join(CONFIG_FILE_NAME);
        fs::write(&path, "[capture]\ncpu_frame = false\n").expect("write a partial configuration");

        let config = AppConfig::load(&path).expect("partial configuration loads");
        assert_eq!(config.schema_version, CONFIG_SCHEMA_VERSION);
        assert!(!config.capture.cpu_frame);
    }

    #[test]
    fn an_unknown_key_at_the_current_schema_is_still_an_error() {
        // The version check must not become a way to smuggle typos past `deny_unknown_fields`.
        let directory = TestDirectory::new("unknown-key");
        let path = directory.join(CONFIG_FILE_NAME);
        fs::write(
            &path,
            format!("schema_version = {CONFIG_SCHEMA_VERSION}\n\n[capture]\ncpu_frmae = false\n"),
        )
        .expect("write a typo");

        let error = AppConfig::load(&path).expect_err("a typo is still rejected");
        assert!(
            !matches!(error, ConfigError::UnsupportedSchema(_)),
            "a typo at the current schema must not be reported as a version problem"
        );
    }

    #[test]
    fn existing_selection_tables_default_to_automatic_preview() {
        let config: AppConfig =
            toml::from_str("schema_version = 1\n[selection]\nenabled = true\nqueue_capacity = 1\n")
                .expect("legacy selection table remains compatible");

        assert_eq!(config.selection.preview, PreviewMode::Auto);
    }

    #[test]
    fn selection_preview_policy_is_strictly_typed() {
        for (value, expected) in [
            ("auto", PreviewMode::Auto),
            ("live", PreviewMode::Live),
            ("frozen", PreviewMode::Frozen),
        ] {
            let source = format!("schema_version = 1\n[selection]\npreview = \"{value}\"\n");
            let config: AppConfig = toml::from_str(&source).expect("supported preview policy");
            assert_eq!(config.selection.preview, expected);
        }

        toml::from_str::<AppConfig>("schema_version = 1\n[selection]\npreview = \"continuous\"\n")
            .expect_err("unknown preview policies must be rejected");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_existing_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = test_directory("atomic-permissions");
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join("captastic.toml");
        fs::write(&path, "old").expect("write original");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("restrict original permissions");

        atomic_write(&path, b"new", replace_file).expect("replace config");

        let mode = fs::metadata(&path)
            .expect("replacement metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn ui_state_store_reads_and_writes_its_injected_config_path() {
        let directory = TestDirectory::new("ui-state-path");
        let path = directory.join("alternate-profile.toml");
        let store = UiStateStore::for_config(&path);

        store
            .save_display_overlay_center("external", 0.25, 0.75)
            .expect("save alternate profile UI state");
        let state = store
            .load_display_ui_state("external")
            .expect("load alternate profile UI state");

        assert_eq!(state.overlay_center, Some((0.25, 0.75)));
        // State lands beside the profile it belongs to, and the profile itself is never created
        // or touched by a state write.
        assert!(directory.join(STATE_FILE_NAME).exists());
        assert!(!path.exists());
    }

    #[test]
    fn opening_a_missing_default_prepares_it_but_explicit_paths_remain_strict() {
        let directory = TestDirectory::new("prepare-config");
        let default_path = directory.join(CONFIG_FILE_NAME);
        let explicit_path = directory.join("explicit.toml");

        let prepared = prepare_config_path_for_open(&default_path, Some(&default_path), || {
            fs::write(&default_path, "schema_version = 1\n").expect("create default");
            Ok(default_path.clone())
        })
        .expect("prepare missing default");
        assert_eq!(prepared, default_path);
        assert!(prepared.exists());

        let explicit = prepare_config_path_for_open(&explicit_path, Some(&default_path), || {
            panic!("explicit paths must not be synthesized")
        })
        .expect("return missing explicit path");
        assert_eq!(explicit, explicit_path);
        assert!(!explicit.exists());
    }

    #[test]
    fn recovering_load_quarantines_malformed_toml_and_returns_defaults() {
        let directory = TestDirectory::new("corrupt-recovery");
        let path = directory.join(CONFIG_FILE_NAME);
        let malformed = "schema_version = 1\n[daemon\n";
        fs::write(&path, malformed).expect("write malformed config");

        let (config, recovery) =
            AppConfig::load_recovering(&path).expect("recover malformed default config");
        let recovery = recovery.expect("recovery metadata");

        config.validate().expect("recovered defaults are valid");
        assert_eq!(recovery.original_path, path);
        assert!(
            !path.exists(),
            "damaged path should be available for a fresh save"
        );
        assert_eq!(
            fs::read_to_string(&recovery.quarantined_path).expect("quarantined contents"),
            malformed
        );
        assert!(!recovery.reason.is_empty());
    }

    #[test]
    fn recovering_load_keeps_well_formed_but_invalid_config_in_place() {
        let directory = TestDirectory::new("strict-recovery");
        let path = directory.join(CONFIG_FILE_NAME);
        let invalid = "schema_version = 1\nunknown_key = true\n";
        fs::write(&path, invalid).expect("write well-formed invalid config");

        let error = AppConfig::load_recovering(&path)
            .expect_err("unknown fields remain strict instead of being quarantined");

        assert!(matches!(error, ConfigError::Parse(_)));
        assert_eq!(
            fs::read_to_string(&path).expect("original remains"),
            invalid
        );
        assert_eq!(
            fs::read_dir(&directory.0)
                .expect("read test directory")
                .count(),
            1,
            "strict failures must not create a quarantine file"
        );
    }

    #[test]
    fn recovering_load_quarantines_invalid_utf8() {
        let directory = TestDirectory::new("invalid-utf8-recovery");
        let path = directory.join(CONFIG_FILE_NAME);
        let damaged = b"schema_version = 1\n#\xff\n";
        fs::write(&path, damaged).expect("write invalid UTF-8 config");

        let (config, recovery) =
            AppConfig::load_recovering(&path).expect("recover invalid UTF-8 default config");
        let recovery = recovery.expect("recovery metadata");

        config.validate().expect("recovered defaults are valid");
        assert_eq!(
            fs::read(&recovery.quarantined_path).expect("quarantined bytes"),
            damaged
        );
        assert!(!path.exists());
    }

    #[test]
    fn recovering_load_keeps_wrong_types_and_validation_failures_in_place() {
        for (label, invalid) in [
            (
                "wrong-type",
                "schema_version = 1\n[daemon]\ntrigger_queue_capacity = 'many'\n",
            ),
            (
                "validation",
                "schema_version = 1\n[daemon]\ntrigger_queue_capacity = 0\n",
            ),
        ] {
            let directory = TestDirectory::new(label);
            let path = directory.join(CONFIG_FILE_NAME);
            fs::write(&path, invalid).expect("write semantically invalid config");

            AppConfig::load_recovering(&path)
                .expect_err("well-formed semantic failures must remain strict");

            assert_eq!(
                fs::read_to_string(&path).expect("original remains"),
                invalid
            );
            assert_eq!(
                fs::read_dir(&directory.0)
                    .expect("read test directory")
                    .count(),
                1,
                "semantic failures must not create quarantine files"
            );
        }
    }

    #[test]
    fn storage_directory_uses_platform_appropriate_home_precedence() {
        let user_profile = PathBuf::from("user-profile-home");
        let home = PathBuf::from("home");
        assert_eq!(
            storage_directory_from(Some(user_profile.clone()), Some(home.clone()), true),
            Some(user_profile.join(".captastic"))
        );
        assert_eq!(
            storage_directory_from(
                Some(PathBuf::from("windows-home")),
                Some(home.clone()),
                false
            ),
            Some(home.join(".captastic"))
        );
        assert_eq!(
            storage_directory_from(Some(PathBuf::new()), Some(home.clone()), true),
            Some(home.join(".captastic"))
        );
        assert_eq!(storage_directory_from(None, None, true), None);
    }

    #[test]
    fn saving_a_zero_dimension_region_is_rejected_and_leaves_the_file_untouched() {
        let directory = TestDirectory::new("zero-region-rejected");
        let path = directory.join(CONFIG_FILE_NAME);
        let store = UiStateStore::for_config(&path);

        let region = CaptureRegion {
            x: 10,
            y: 20,
            width: 300,
            height: 200,
        };
        let source = CaptureRegionSource {
            width: 1_920,
            height: 1_080,
            rotation_degrees: 0,
        };
        store
            .save_display_interaction_state("main", CaptureTool::Region, Some(region), Some(source))
            .expect("save valid interaction state");
        let state_path = directory.join(STATE_FILE_NAME);
        let before = fs::read_to_string(&state_path).expect("read persisted state");

        let error = store
            .save_display_interaction_state(
                "main",
                CaptureTool::Region,
                Some(CaptureRegion {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 200,
                }),
                None,
            )
            .expect_err("zero-area region must be rejected");
        assert!(matches!(error, ConfigError::InvalidValue(_)));

        let after = fs::read_to_string(&state_path).expect("read state after rejected save");
        assert_eq!(before, after, "rejected save must not modify the file");
    }

    #[test]
    fn writing_a_region_without_a_source_clears_the_previous_source() {
        let directory = TestDirectory::new("region-source-clears");
        let path = directory.join(CONFIG_FILE_NAME);
        let store = UiStateStore::for_config(&path);

        let region = CaptureRegion {
            x: 10,
            y: 20,
            width: 300,
            height: 200,
        };
        let source = CaptureRegionSource {
            width: 1_920,
            height: 1_080,
            rotation_degrees: 0,
        };
        store
            .save_display_interaction_state("main", CaptureTool::Region, Some(region), Some(source))
            .expect("save interaction state with a source");
        let state = store
            .load_display_ui_state("main")
            .expect("load state with source");
        assert_eq!(state.region_source, Some(source));

        let new_region = CaptureRegion {
            x: 5,
            y: 5,
            width: 640,
            height: 480,
        };
        store
            .save_display_interaction_state("main", CaptureTool::Region, Some(new_region), None)
            .expect("save interaction state without a source");
        let state = store
            .load_display_ui_state("main")
            .expect("load state without source");
        assert_eq!(state.region, Some(new_region));
        assert_eq!(state.region_source, None, "stale source must be removed");
    }

    #[test]
    fn rejects_incomplete_or_empty_per_display_state() {
        let mut config = AppConfig::default();
        config.ui.displays.insert(
            "external".to_owned(),
            DisplayUiConfig {
                overlay_x: Some(10),
                ..DisplayUiConfig::default()
            },
        );
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));
        config.ui.displays.clear();
        config.ui.displays.insert(
            String::new(),
            DisplayUiConfig {
                overlay_x: Some(10),
                overlay_y: Some(10),
                ..DisplayUiConfig::default()
            },
        );
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));
        config.ui.displays.clear();
        config.ui.displays.insert(
            "external".to_owned(),
            DisplayUiConfig {
                overlay_center_x: Some(1.5),
                overlay_center_y: Some(0.5),
                ..DisplayUiConfig::default()
            },
        );
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let error = toml::from_str::<AppConfig>("schema_version = 1\nsurprise = true")
            .expect_err("unknown field should fail");
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn rejects_zero_queue_capacity() {
        let mut config = AppConfig::default();
        config.daemon.trigger_queue_capacity = 0;
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));
    }

    #[test]
    fn accepts_pointer_primary_virtual_desktop_or_a_persistent_display_id() {
        let mut config = AppConfig::default();
        config.daemon.display = "pointer".to_owned();
        config.validate().expect("pointer display policy");
        config.daemon.display = "primary".to_owned();
        config.validate().expect("primary display policy");
        config.daemon.display = "virtual_desktop".to_owned();
        config.validate().expect("virtual desktop policy");
        config.daemon.display = "display:windows-monitor-0123456789abcdef".to_owned();
        config.validate().expect("fixed display policy");
    }

    #[test]
    fn rejects_empty_or_unknown_display_policies() {
        for value in ["display:", "windows-monitor-id"] {
            let mut config = AppConfig::default();
            config.daemon.display = value.to_owned();
            assert!(matches!(
                config.validate(),
                Err(ConfigError::InvalidValue(_))
            ));
        }
    }

    #[test]
    fn output_can_now_be_enabled() {
        // `output.enabled` was rejected outright until Milestone 4 implemented the file worker.
        let mut config = AppConfig::default();
        config.output.enabled = true;
        config.validate().expect("file output is implemented");
    }

    #[test]
    fn an_output_directory_must_be_absolute() {
        // A daemon's working directory is whatever launched it, so a relative path would put
        // captures somewhere the user cannot predict and would move between launches.
        let mut config = AppConfig::default();
        config.output.enabled = true;
        config.output.directory = Some(PathBuf::from("captures"));
        let error = config.validate().expect_err("a relative path is rejected");
        assert!(error.to_string().contains("absolute"), "{error}");

        config.output.directory = Some(PathBuf::new());
        let error = config.validate().expect_err("an empty path is rejected");
        assert!(error.to_string().contains("must not be empty"), "{error}");

        config.output.directory = Some(if cfg!(windows) {
            PathBuf::from(r"C:\Users\someone\Pictures\Captastic")
        } else {
            PathBuf::from("/home/someone/Pictures/Captastic")
        });
        config.validate().expect("an absolute path is accepted");
    }

    #[test]
    fn the_default_output_directory_sits_with_the_users_pictures() {
        // A screenshot is the user's document, not application state, so it does not belong in
        // `.captastic` beside Captastic's own files.
        let directory = default_output_directory().expect("a home directory exists in this test");
        // `Path::ends_with` compares components, so one form covers both separators.
        assert!(
            directory.ends_with("Pictures/Captastic"),
            "{}",
            directory.display()
        );
        assert!(!directory.to_string_lossy().contains(".captastic"));
    }

    #[test]
    fn rejects_unknown_output_format() {
        let mut config = AppConfig::default();
        config.output.format = "jpeg".to_owned();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));
    }

    #[test]
    fn rejects_capture_buffer_slots_other_than_the_fixed_default() {
        let mut config = AppConfig::default();
        assert_eq!(config.capture.buffer_slots, 3);
        for value in [2, 4, 16] {
            config.capture.buffer_slots = value;
            assert!(matches!(
                config.validate(),
                Err(ConfigError::InvalidValue(_))
            ));
        }
    }

    #[test]
    fn rejects_unknown_capture_cursor_mode() {
        let mut config = AppConfig::default();
        config.capture.cursor = "auto".to_owned();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));
    }

    #[test]
    fn accepts_capture_cursor_include_now_that_it_is_implemented() {
        let mut config = AppConfig::default();
        config.capture.cursor = "include".to_owned();
        config.validate().expect("cursor include is implemented");

        // The value is still checked; only the "not implemented" refusal is gone.
        config.capture.cursor = "sometimes".to_owned();
        let error = config.validate().expect_err("an unknown cursor policy");
        assert!(matches!(error, ConfigError::InvalidValue(_)));
        assert!(error.to_string().contains("capture.cursor"));
    }

    #[test]
    fn rejects_hotkey_repeat_coalesce_as_not_implemented() {
        let mut config = AppConfig::default();
        config.hotkey.repeat = "coalesce".to_owned();
        let error = config.validate().expect_err("repeat coalesce is dormant");
        assert!(matches!(error, ConfigError::InvalidValue(_)));
        assert!(error.to_string().contains("hotkey.repeat"));
    }

    #[test]
    fn rejects_unknown_logging_level() {
        let mut config = AppConfig::default();
        config.logging.level = "verbose".to_owned();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));
    }

    #[test]
    fn rejects_unbounded_logging_configuration() {
        let mut config = AppConfig::default();
        config.logging.max_file_bytes = 0;
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));
        config.logging.max_file_bytes = 5 * 1_024 * 1_024;
        config.logging.retained_files = 0;
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));
    }

    #[test]
    fn rejects_an_empty_saved_region() {
        let mut config = AppConfig::default();
        config.ui.last_region = Some(CaptureRegion {
            x: 0,
            y: 0,
            width: 0,
            height: 100,
        });
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue(_))
        ));
    }
    #[test]
    fn legacy_binding_resolves_to_last_workflow() {
        let config: AppConfig =
            toml::from_str("[hotkey]\nbinding = \"ctrl + shift + f9\"\nrepeat = \"ignore\"\n")
                .expect("legacy config");
        let bindings = config.hotkey.resolved_bindings().expect("legacy binding");
        assert_eq!(
            bindings,
            vec![HotkeyBinding {
                action: HotkeyAction::LastWorkflow,
                chord: "Ctrl+Shift+F9".parse().expect("chord"),
            }]
        );
    }

    #[test]
    fn hotkey_grammar_canonicalizes_supported_modifiers_and_keys() {
        for (input, expected) in [
            ("control+windows+alt+shift+a", "Ctrl+Alt+Shift+Win+A"),
            ("ALT+0", "Alt+0"),
            ("win+f1", "Win+F1"),
            ("Ctrl+F24", "Ctrl+F24"),
            ("z", "Z"),
        ] {
            let chord: HotkeyChord = input.parse().expect(input);
            assert_eq!(chord.to_string(), expected);
        }
    }

    #[test]
    fn malformed_hotkeys_are_rejected_without_reinterpretation() {
        for input in [
            "",
            "Ctrl++F9",
            "Ctrl+Control+F9",
            "Ctrl+Shift",
            "Ctrl+A+B",
            "Ctrl+F25",
            "Ctrl+F01",
            "Ctrl+Space",
        ] {
            assert!(input.parse::<HotkeyChord>().is_err(), "accepted {input:?}");
        }
    }

    #[test]
    fn duplicate_chords_across_actions_are_rejected_after_canonicalization() {
        let mut config = AppConfig::default();
        config.hotkey.bindings.region = Some("alt+r".to_owned());
        config.hotkey.bindings.window = Some("ALT+R".to_owned());
        let error = config.validate().expect_err("duplicate chord");
        assert!(error.to_string().contains("region"));
        assert!(error.to_string().contains("window"));
        assert!(error.to_string().contains("Alt+R"));
    }

    #[test]
    fn omitted_actions_are_disabled_and_empty_actions_are_invalid() {
        let config: AppConfig = toml::from_str(
            "[hotkey]\nrepeat = \"ignore\"\n[hotkey.bindings]\nfull_display = \"Win+F12\"\n",
        )
        .expect("config");
        let bindings = config.hotkey.resolved_bindings().expect("one binding");
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].action, HotkeyAction::FullDisplay);

        let mut invalid = config;
        invalid.hotkey.bindings.region = Some(String::new());
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn legacy_and_canonical_last_workflow_are_ambiguous() {
        let config: AppConfig = toml::from_str(
            "[hotkey]\nbinding = \"Ctrl+Shift+F9\"\nrepeat = \"ignore\"\n[hotkey.bindings]\nlast_workflow = \"Alt+F9\"\n",
        )
        .expect("syntax is valid");
        let error = config.validate().expect_err("ambiguous action");
        assert!(error.to_string().contains("define the same action"));
    }

    #[test]
    fn canonical_bindings_round_trip_through_serialization() {
        let source = "# keep this comment
[daemon]
trigger_queue_capacity = 9

[hotkey]
repeat = \"ignore\"

[hotkey.bindings]
last_workflow = \"Ctrl+Shift+F9\"
region = \"Alt+R\"
full_display = \"Win+F12\"
";
        let config: AppConfig = toml::from_str(source).expect("canonical config");
        config.validate().expect("valid bindings");
        let serialized = config.to_toml_pretty().expect("serialize");
        let reloaded: AppConfig = toml::from_str(&serialized).expect("reload");
        assert_eq!(
            reloaded.hotkey.resolved_bindings().expect("bindings"),
            config.hotkey.resolved_bindings().expect("bindings")
        );
        assert_eq!(reloaded.daemon.trigger_queue_capacity, 9);
    }
}
