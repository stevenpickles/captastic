#![deny(unsafe_code)]

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CONFIG_SCHEMA_VERSION: u32 = 1;
pub const CONFIG_FILE_NAME: &str = "captastic.toml";

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const TEMP_ARTIFACT_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const CORRUPT_ARTIFACT_RETENTION: usize = 5;

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

fn storage_directory_from(
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
        .map(|path| path.join(".captastic"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureTool {
    FullDisplay,
    Window,
    Region,
}

impl CaptureTool {
    const fn as_str(self) -> &'static str {
        match self {
            Self::FullDisplay => "full_display",
            Self::Window => "window",
            Self::Region => "region",
        }
    }
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CaptureHistory {
    pub tool: Option<CaptureTool>,
    pub region: Option<CaptureRegion>,
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

#[derive(Clone, Debug)]
pub struct UiStateStore {
    path: Option<PathBuf>,
}

impl UiStateStore {
    pub fn for_default_config() -> Self {
        Self {
            path: default_config_path(),
        }
    }

    pub fn for_config(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Some(path.into()),
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn prepare_for_open(&self) -> Result<PathBuf, ConfigError> {
        let path = self.required_path()?;
        prepare_config_path_for_open(
            path,
            default_config_path().as_deref(),
            ensure_default_config,
        )
    }

    fn required_path(&self) -> Result<&Path, ConfigError> {
        self.path
            .as_deref()
            .ok_or(ConfigError::HomeDirectoryUnavailable)
    }

    pub fn load_display_ui_state(&self, display_id: &str) -> Result<DisplayUiState, ConfigError> {
        let config = AppConfig::load_optional(self.required_path()?)?;
        Ok(resolve_display_ui_state(&config.ui, display_id))
    }

    pub fn save_display_overlay_center(
        &self,
        display_id: &str,
        center_x: f64,
        center_y: f64,
    ) -> Result<(), ConfigError> {
        let path = self.required_path()?;
        let source = read_optional_config_source(path)?;
        let updated = update_display_overlay_center(&source, display_id, center_x, center_y)?;
        write_config_source(path, updated)
    }

    pub fn save_display_interaction_state(
        &self,
        display_id: &str,
        tool: CaptureTool,
        region: Option<CaptureRegion>,
        region_source: Option<CaptureRegionSource>,
    ) -> Result<(), ConfigError> {
        let path = self.required_path()?;
        let source = read_optional_config_source(path)?;
        let updated =
            update_display_interaction_state(&source, display_id, tool, region, region_source)?;
        write_config_source(path, updated)
    }

    pub fn save_display_confirmed_region(
        &self,
        display_id: &str,
        region: CaptureRegion,
        source: CaptureRegionSource,
    ) -> Result<(), ConfigError> {
        let path = self.required_path()?;
        let current = read_optional_config_source(path)?;
        let updated = update_display_confirmed_region(&current, display_id, region, source)?;
        write_config_source(path, updated)
    }
}

fn prepare_config_path_for_open(
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
    pub selection: QueueFeatureConfig,
    pub clipboard: QueueFeatureConfig,
    pub output: OutputConfig,
    pub metrics: MetricsConfig,
    pub logging: LoggingConfig,
    pub ui: UiConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            daemon: DaemonConfig::default(),
            hotkey: HotkeyConfig::default(),
            capture: CaptureConfig::default(),
            selection: QueueFeatureConfig::default(),
            clipboard: QueueFeatureConfig::default(),
            output: OutputConfig::default(),
            metrics: MetricsConfig::default(),
            logging: LoggingConfig::default(),
            ui: UiConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
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
        let config: Self = toml::from_str(&text)?;
        config.validate()?;
        Ok((config, None))
    }

    pub fn confirmed_regions(&self) -> BTreeMap<String, ConfirmedRegion> {
        self.ui
            .displays
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
        if self.capture.buffer_slots < 2 || self.capture.buffer_slots > 16 {
            return Err(ConfigError::InvalidValue(
                "capture.buffer_slots must be between 2 and 16".to_owned(),
            ));
        }
        if !matches!(self.capture.mode.as_str(), "fresh" | "latest") {
            return Err(ConfigError::InvalidValue(
                "capture.mode must be fresh or latest".to_owned(),
            ));
        }
        if !matches!(self.hotkey.repeat.as_str(), "ignore" | "coalesce") {
            return Err(ConfigError::InvalidValue(
                "hotkey.repeat must be ignore or coalesce".to_owned(),
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
        for (display_id, state) in &self.ui.displays {
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

    pub fn to_toml_pretty(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(ConfigError::Serialize)
    }
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

pub fn load_display_ui_state(display_id: &str) -> Result<DisplayUiState, ConfigError> {
    UiStateStore::for_default_config().load_display_ui_state(display_id)
}

pub fn resolve_display_ui_state(ui: &UiConfig, display_id: &str) -> DisplayUiState {
    let display = ui.displays.get(display_id);
    let display_region = display.and_then(|state| state.last_region);
    DisplayUiState {
        overlay_center: display
            .and_then(|state| state.overlay_center_x.zip(state.overlay_center_y)),
        overlay_position: display
            .and_then(|state| state.overlay_x.zip(state.overlay_y))
            .or_else(|| ui.overlay_x.zip(ui.overlay_y)),
        tool: display
            .and_then(|state| state.last_capture_tool)
            .or(ui.last_capture_tool),
        region: display_region.or(ui.last_region),
        region_source: display.and_then(|state| state.last_region_source),
        region_is_display_local: display_region.is_some(),
        confirmed_region: display
            .and_then(|state| {
                state
                    .last_confirmed_region
                    .zip(state.last_confirmed_region_source)
            })
            .map(|(region, source)| ConfirmedRegion { region, source }),
    }
}

pub fn save_display_overlay_center(
    display_id: &str,
    center_x: f64,
    center_y: f64,
) -> Result<(), ConfigError> {
    UiStateStore::for_default_config().save_display_overlay_center(display_id, center_x, center_y)
}

fn update_display_overlay_center(
    source: &str,
    display_id: &str,
    center_x: f64,
    center_y: f64,
) -> Result<String, ConfigError> {
    if !center_x.is_finite()
        || !center_y.is_finite()
        || !(0.0..=1.0).contains(&center_x)
        || !(0.0..=1.0).contains(&center_y)
    {
        return Err(ConfigError::InvalidValue(
            "display overlay center coordinates must be finite values between 0 and 1".to_owned(),
        ));
    }
    let mut document = editable_document(source)?;
    let state = &mut document["ui"]["displays"][display_id];
    state["overlay_center_x"] = toml_edit::value(center_x);
    state["overlay_center_y"] = toml_edit::value(center_y);
    if let Some(table) = state.as_table_like_mut() {
        table.remove("overlay_x");
        table.remove("overlay_y");
    }
    Ok(document.to_string())
}

pub fn save_display_overlay_position(display_id: &str, x: i32, y: i32) -> Result<(), ConfigError> {
    let path = default_config_path().ok_or(ConfigError::HomeDirectoryUnavailable)?;
    let source = read_optional_config_source(&path)?;
    let updated = update_display_overlay_position(&source, display_id, x, y)?;
    write_config_source(&path, updated)
}

fn update_display_overlay_position(
    source: &str,
    display_id: &str,
    x: i32,
    y: i32,
) -> Result<String, ConfigError> {
    let mut document = editable_document(source)?;
    document["ui"]["displays"][display_id]["overlay_x"] = toml_edit::value(i64::from(x));
    document["ui"]["displays"][display_id]["overlay_y"] = toml_edit::value(i64::from(y));
    Ok(document.to_string())
}

pub fn save_display_interaction_state(
    display_id: &str,
    tool: CaptureTool,
    region: Option<CaptureRegion>,
    region_source: Option<CaptureRegionSource>,
) -> Result<(), ConfigError> {
    UiStateStore::for_default_config().save_display_interaction_state(
        display_id,
        tool,
        region,
        region_source,
    )
}

fn update_display_interaction_state(
    source: &str,
    display_id: &str,
    tool: CaptureTool,
    region: Option<CaptureRegion>,
    region_source: Option<CaptureRegionSource>,
) -> Result<String, ConfigError> {
    let mut document = editable_document(source)?;
    let state = &mut document["ui"]["displays"][display_id];
    state["last_capture_tool"] = toml_edit::value(tool.as_str());
    if let Some(region) = region {
        state["last_region"]["x"] = toml_edit::value(i64::from(region.x));
        state["last_region"]["y"] = toml_edit::value(i64::from(region.y));
        state["last_region"]["width"] = toml_edit::value(i64::from(region.width));
        state["last_region"]["height"] = toml_edit::value(i64::from(region.height));
        if let Some(source) = region_source {
            state["last_region_source"]["width"] = toml_edit::value(i64::from(source.width));
            state["last_region_source"]["height"] = toml_edit::value(i64::from(source.height));
            state["last_region_source"]["rotation_degrees"] =
                toml_edit::value(i64::from(source.rotation_degrees));
        }
    }
    Ok(document.to_string())
}

fn editable_document(source: &str) -> Result<toml_edit::Document, ConfigError> {
    if source.trim().is_empty() {
        Ok(toml_edit::Document::new())
    } else {
        source.parse::<toml_edit::Document>().map_err(Into::into)
    }
}

fn read_optional_config_source(path: &Path) -> Result<String, ConfigError> {
    match fs::read_to_string(path) {
        Ok(source) => Ok(source),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(source) => Err(ConfigError::Read {
            path: path.display().to_string(),
            source,
        }),
    }
}

fn write_config_source(path: &Path, source: String) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: parent.display().to_string(),
            source,
        })?;
    }
    atomic_write(path, source.as_bytes(), replace_file).map_err(|source| ConfigError::Write {
        path: path.display().to_string(),
        source,
    })
}

fn atomic_write<F>(path: &Path, contents: &[u8], replace: F) -> std::io::Result<()>
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

fn quarantine_config(path: &Path) -> std::io::Result<Option<PathBuf>> {
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

fn maintain_config_artifacts(path: &Path, protected: Option<&Path>) {
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

fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    move_file(from, to, true)
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
        GetFileAttributesW, MoveFileExW, SetFileAttributesW, FILE_ATTRIBUTE_READONLY,
        FILE_FLAGS_AND_ATTRIBUTES, INVALID_FILE_ATTRIBUTES, MOVEFILE_REPLACE_EXISTING,
        MOVEFILE_WRITE_THROUGH,
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
        // SAFETY: path is a live, NUL-terminated UTF-16 buffer and attributes came from Windows.
        unsafe { SetFileAttributesW(PCWSTR(path.as_ptr()), attributes) }
            .map_err(|_| io::Error::last_os_error())
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

pub fn load_overlay_position() -> Result<Option<(i32, i32)>, ConfigError> {
    let config = AppConfig::load_default()?;
    Ok(config.ui.overlay_x.zip(config.ui.overlay_y))
}

pub fn save_overlay_position(x: i32, y: i32) -> Result<(), ConfigError> {
    let path = default_config_path().ok_or(ConfigError::HomeDirectoryUnavailable)?;
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.display().to_string(),
                source,
            });
        }
    };

    let updated = update_overlay_position(&source, x, y)?;
    write_config_source(&path, updated)
}

fn update_overlay_position(source: &str, x: i32, y: i32) -> Result<String, ConfigError> {
    let mut document = if source.trim().is_empty() {
        toml_edit::Document::new()
    } else {
        source.parse::<toml_edit::Document>()?
    };
    document["ui"]["overlay_x"] = toml_edit::value(i64::from(x));
    document["ui"]["overlay_y"] = toml_edit::value(i64::from(y));
    Ok(document.to_string())
}

pub fn save_display_confirmed_region(
    display_id: &str,
    region: CaptureRegion,
    source: CaptureRegionSource,
) -> Result<(), ConfigError> {
    UiStateStore::for_default_config().save_display_confirmed_region(display_id, region, source)
}

fn update_display_confirmed_region(
    source_text: &str,
    display_id: &str,
    region: CaptureRegion,
    source: CaptureRegionSource,
) -> Result<String, ConfigError> {
    if region.width == 0 || region.height == 0 || source.width == 0 || source.height == 0 {
        return Err(ConfigError::InvalidValue(
            "confirmed region and source dimensions must be greater than zero".to_owned(),
        ));
    }
    let mut document = editable_document(source_text)?;
    let state = &mut document["ui"]["displays"][display_id];
    state["last_confirmed_region"]["x"] = toml_edit::value(i64::from(region.x));
    state["last_confirmed_region"]["y"] = toml_edit::value(i64::from(region.y));
    state["last_confirmed_region"]["width"] = toml_edit::value(i64::from(region.width));
    state["last_confirmed_region"]["height"] = toml_edit::value(i64::from(region.height));
    state["last_confirmed_region_source"]["width"] = toml_edit::value(i64::from(source.width));
    state["last_confirmed_region_source"]["height"] = toml_edit::value(i64::from(source.height));
    state["last_confirmed_region_source"]["rotation_degrees"] =
        toml_edit::value(i64::from(source.rotation_degrees));
    Ok(document.to_string())
}

pub fn load_capture_history() -> Result<CaptureHistory, ConfigError> {
    let config = AppConfig::load_default()?;
    Ok(CaptureHistory {
        tool: config.ui.last_capture_tool,
        region: config.ui.last_region,
    })
}

pub fn save_capture_history(
    tool: CaptureTool,
    region: Option<CaptureRegion>,
) -> Result<(), ConfigError> {
    let path = default_config_path().ok_or(ConfigError::HomeDirectoryUnavailable)?;
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.display().to_string(),
                source,
            });
        }
    };
    let updated = update_capture_history(&source, tool, region)?;
    write_config_source(&path, updated)
}

fn update_capture_history(
    source: &str,
    tool: CaptureTool,
    region: Option<CaptureRegion>,
) -> Result<String, ConfigError> {
    let mut document = if source.trim().is_empty() {
        toml_edit::Document::new()
    } else {
        source.parse::<toml_edit::Document>()?
    };
    document["ui"]["last_capture_tool"] = toml_edit::value(tool.as_str());
    if let Some(region) = region {
        document["ui"]["last_region"]["x"] = toml_edit::value(i64::from(region.x));
        document["ui"]["last_region"]["y"] = toml_edit::value(i64::from(region.y));
        document["ui"]["last_region"]["width"] = toml_edit::value(i64::from(region.width));
        document["ui"]["last_region"]["height"] = toml_edit::value(i64::from(region.height));
    }
    Ok(document.to_string())
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
pub struct QueueFeatureConfig {
    pub enabled: bool,
    pub queue_capacity: usize,
}

impl Default for QueueFeatureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            queue_capacity: 1,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    pub enabled: bool,
    pub format: String,
    pub queue_capacity: usize,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            format: "png".to_owned(),
            queue_capacity: 2,
        }
    }
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
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "captastic-config-{label}-{}-{sequence}",
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
        AppConfig::default().validate().expect("valid defaults");
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

        write_config_source(&path, "after".to_owned()).expect("replace attributed config");

        let attributes = windows_file_move::file_attributes(&path).expect("read final attributes");
        assert_eq!(attributes & (READONLY | HIDDEN), READONLY | HIDDEN);
        assert_eq!(
            fs::read_to_string(&path).expect("read replacement"),
            "after"
        );
        windows_file_move::set_file_attributes(&path, NORMAL).expect("make test file removable");
    }

    #[test]
    fn atomic_write_replaces_an_existing_configuration() {
        let directory = TestDirectory::new("atomic-success");
        let path = directory.join(CONFIG_FILE_NAME);
        fs::write(&path, "schema_version = 0\n").expect("write original config");
        let replacement = AppConfig::default()
            .to_toml_pretty()
            .expect("serialize replacement config");

        write_config_source(&path, replacement.clone()).expect("atomically replace config");

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
        assert!(path.exists());
        assert!(!directory.join(CONFIG_FILE_NAME).exists());
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
    fn overlay_position_update_preserves_other_settings_and_comments() {
        let source = "# keep this comment\n[logging]\nlevel = \"debug\"\n\n[ui]\noverlay_x = 1\noverlay_y = 2\n";
        let updated = update_overlay_position(source, 640, 920).expect("updated TOML");
        assert!(updated.contains("# keep this comment"));
        assert!(updated.contains("level = \"debug\""));
        let config: AppConfig = toml::from_str(&updated).expect("valid Captastic config");
        assert_eq!(config.ui.overlay_x, Some(640));
        assert_eq!(config.ui.overlay_y, Some(920));
    }

    #[test]
    fn capture_history_update_preserves_comments_and_the_previous_region() {
        let source = "# keep this comment\n[logging]\nlevel = \"debug\"\n\n[ui]\nlast_capture_tool = \"region\"\n\n[ui.last_region]\nx = 120\ny = 80\nwidth = 640\nheight = 360\n";
        let updated = update_capture_history(source, CaptureTool::Window, None)
            .expect("updated capture history");
        assert!(updated.contains("# keep this comment"));
        assert!(updated.contains("level = \"debug\""));
        let config: AppConfig = toml::from_str(&updated).expect("valid Captastic config");
        assert_eq!(config.ui.last_capture_tool, Some(CaptureTool::Window));
        assert_eq!(
            config.ui.last_region,
            Some(CaptureRegion {
                x: 120,
                y: 80,
                width: 640,
                height: 360,
            })
        );
    }

    #[test]
    fn capture_history_update_records_a_confirmed_region() {
        let region = CaptureRegion {
            x: -100,
            y: 40,
            width: 960,
            height: 540,
        };
        let updated = update_capture_history("", CaptureTool::Region, Some(region))
            .expect("updated capture history");
        let config: AppConfig = toml::from_str(&updated).expect("valid Captastic config");
        assert_eq!(config.ui.last_capture_tool, Some(CaptureTool::Region));
        assert_eq!(config.ui.last_region, Some(region));
    }

    #[test]
    fn display_history_records_a_cancelled_region_interaction() {
        let source = "[ui.displays.main]\nlast_capture_tool = \"window\"\n";
        let region = CaptureRegion {
            x: 420,
            y: 260,
            width: 800,
            height: 450,
        };
        let region_source = CaptureRegionSource {
            width: 1_920,
            height: 1_080,
            rotation_degrees: 0,
        };
        let updated = update_display_interaction_state(
            source,
            "main",
            CaptureTool::Region,
            Some(region),
            Some(region_source),
        )
        .expect("cancelled interaction state");
        let config: AppConfig = toml::from_str(&updated).expect("valid Captastic config");
        assert_eq!(
            resolve_display_ui_state(&config.ui, "main"),
            DisplayUiState {
                tool: Some(CaptureTool::Region),
                region: Some(region),
                region_source: Some(region_source),
                region_is_display_local: true,
                ..DisplayUiState::default()
            }
        );
    }

    #[test]
    fn display_ui_state_is_independent_and_survives_serialization() {
        let source = "# preserve me\n[ui]\nlast_capture_tool = \"full_display\"\noverlay_x = 40\noverlay_y = 50\n";
        let updated =
            update_display_overlay_center(source, "laptop", 0.25, 0.75).expect("laptop toolbar");
        let updated = update_display_interaction_state(
            &updated,
            "laptop",
            CaptureTool::Region,
            Some(CaptureRegion {
                x: 100,
                y: 80,
                width: 960,
                height: 540,
            }),
            Some(CaptureRegionSource {
                width: 1920,
                height: 1080,
                rotation_degrees: 0,
            }),
        )
        .expect("laptop history");
        let updated = update_display_overlay_center(&updated, "external", 0.8, 0.2)
            .expect("external toolbar");
        let updated = update_display_interaction_state(
            &updated,
            "external",
            CaptureTool::Window,
            Some(CaptureRegion {
                x: 300,
                y: 200,
                width: 1280,
                height: 720,
            }),
            Some(CaptureRegionSource {
                width: 3840,
                height: 2160,
                rotation_degrees: 0,
            }),
        )
        .expect("external history");
        assert!(updated.contains("# preserve me"));

        let config: AppConfig = toml::from_str(&updated).expect("serialized config reloads");
        config.validate().expect("display UI state validates");
        assert_eq!(
            resolve_display_ui_state(&config.ui, "laptop"),
            DisplayUiState {
                overlay_center: Some((0.25, 0.75)),
                overlay_position: Some((40, 50)),
                tool: Some(CaptureTool::Region),
                region: Some(CaptureRegion {
                    x: 100,
                    y: 80,
                    width: 960,
                    height: 540,
                }),
                region_source: Some(CaptureRegionSource {
                    width: 1920,
                    height: 1080,
                    rotation_degrees: 0,
                }),
                confirmed_region: None,
                region_is_display_local: true,
            }
        );
        assert_eq!(
            resolve_display_ui_state(&config.ui, "external"),
            DisplayUiState {
                overlay_center: Some((0.8, 0.2)),
                overlay_position: Some((40, 50)),
                tool: Some(CaptureTool::Window),
                region: Some(CaptureRegion {
                    x: 300,
                    y: 200,
                    width: 1280,
                    height: 720,
                }),
                region_source: Some(CaptureRegionSource {
                    width: 3840,
                    height: 2160,
                    rotation_degrees: 0,
                }),
                confirmed_region: None,
                region_is_display_local: true,
            }
        );
    }

    #[test]
    fn display_ui_state_falls_back_to_legacy_global_values() {
        let config: AppConfig = toml::from_str(
            "[ui]\noverlay_x = 40\noverlay_y = 50\nlast_capture_tool = \"region\"\n\n[ui.last_region]\nx = -100\ny = 25\nwidth = 640\nheight = 360\n",
        )
        .expect("legacy config");
        assert_eq!(
            resolve_display_ui_state(&config.ui, "new-display"),
            DisplayUiState {
                overlay_center: None,
                overlay_position: Some((40, 50)),
                tool: Some(CaptureTool::Region),
                region: Some(CaptureRegion {
                    x: -100,
                    y: 25,
                    width: 640,
                    height: 360,
                }),
                region_source: None,
                confirmed_region: None,
                region_is_display_local: false,
            }
        );
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
    fn normalized_overlay_update_replaces_display_pixel_coordinates() {
        let source = "[ui.displays.external]\noverlay_x = 700\noverlay_y = 1200\n";
        let updated = update_display_overlay_center(source, "external", 0.625, 0.875)
            .expect("normalized toolbar position");
        let config: AppConfig = toml::from_str(&updated).expect("updated config reloads");
        let state = config.ui.displays.get("external").expect("display state");
        assert_eq!(state.overlay_center_x, Some(0.625));
        assert_eq!(state.overlay_center_y, Some(0.875));
        assert_eq!(state.overlay_x, None);
        assert_eq!(state.overlay_y, None);
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
    fn canonical_bindings_round_trip_and_ui_updates_preserve_them_and_comments() {
        let source = "# keep this comment\n[daemon]\ntrigger_queue_capacity = 9\n\n[hotkey]\nrepeat = \"ignore\"\n\n[hotkey.bindings]\nlast_workflow = \"Ctrl+Shift+F9\"\nregion = \"Alt+R\"\nfull_display = \"Win+F12\"\n";
        let config: AppConfig = toml::from_str(source).expect("canonical config");
        config.validate().expect("valid bindings");
        let serialized = config.to_toml_pretty().expect("serialize");
        let reloaded: AppConfig = toml::from_str(&serialized).expect("reload");
        assert_eq!(
            reloaded.hotkey.resolved_bindings().expect("bindings"),
            config.hotkey.resolved_bindings().expect("bindings")
        );
        assert_eq!(reloaded.daemon.trigger_queue_capacity, 9);

        let updated = update_display_confirmed_region(
            source,
            "display-a",
            CaptureRegion {
                x: 10,
                y: 20,
                width: 300,
                height: 200,
            },
            CaptureRegionSource {
                width: 3840,
                height: 2160,
                rotation_degrees: 0,
            },
        )
        .expect("UI update");
        assert!(updated.contains("# keep this comment"));
        assert!(updated.contains("full_display = \"Win+F12\""));
        assert!(updated.contains("trigger_queue_capacity = 9"));
        let updated: AppConfig = toml::from_str(&updated).expect("updated config reloads");
        assert_eq!(
            updated.confirmed_regions().get("display-a"),
            Some(&ConfirmedRegion {
                region: CaptureRegion {
                    x: 10,
                    y: 20,
                    width: 300,
                    height: 200,
                },
                source: CaptureRegionSource {
                    width: 3840,
                    height: 2160,
                    rotation_degrees: 0,
                },
            })
        );
    }
}
