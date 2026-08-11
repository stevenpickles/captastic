#![deny(unsafe_code)]

use std::collections::BTreeMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CONFIG_SCHEMA_VERSION: u32 = 1;
pub const CONFIG_FILE_NAME: &str = "captastic.toml";

/// Returns the per-user directory for Captastic configuration, state, and logs.
pub fn storage_directory() -> Option<PathBuf> {
    storage_directory_from(
        env::var_os("USERPROFILE").map(PathBuf::from),
        env::var_os("HOME").map(PathBuf::from),
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

fn storage_directory_from(user_profile: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    user_profile
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| home.filter(|path| !path.as_os_str().is_empty()))
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
    /// Per-display regions are monitor-local. A legacy global fallback remains desktop-absolute.
    pub region_is_display_local: bool,
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
        match Self::load(&path) {
            Ok(config) => Ok(config),
            Err(ConfigError::Read { source, .. }) if source.kind() == ErrorKind::NotFound => {
                Ok(Self::default())
            }
            Err(error) => Err(error),
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchema(self.schema_version));
        }
        validate_capacity(
            "daemon.trigger_queue_capacity",
            self.daemon.trigger_queue_capacity,
        )?;
        if !matches!(self.daemon.display.as_str(), "primary" | "pointer")
            && self
                .daemon
                .display
                .strip_prefix("display:")
                .is_none_or(|id| id.trim().is_empty())
        {
            return Err(ConfigError::InvalidValue(
                "daemon.display must be pointer, primary, or display:<persistent-id>".to_owned(),
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
        if self.hotkey.binding != "Ctrl+Shift+F9" {
            return Err(ConfigError::InvalidValue(
                "hotkey.binding currently supports only Ctrl+Shift+F9".to_owned(),
            ));
        }
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
        }
        Ok(())
    }

    pub fn to_toml_pretty(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(ConfigError::Serialize)
    }
}

pub fn load_display_ui_state(display_id: &str) -> Result<DisplayUiState, ConfigError> {
    let config = AppConfig::load_default()?;
    Ok(resolve_display_ui_state(&config.ui, display_id))
}

fn resolve_display_ui_state(ui: &UiConfig, display_id: &str) -> DisplayUiState {
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
        region_is_display_local: display_region.is_some(),
    }
}

pub fn save_display_overlay_center(
    display_id: &str,
    center_x: f64,
    center_y: f64,
) -> Result<(), ConfigError> {
    let path = default_config_path().ok_or(ConfigError::HomeDirectoryUnavailable)?;
    let source = read_optional_config_source(&path)?;
    let updated = update_display_overlay_center(&source, display_id, center_x, center_y)?;
    write_config_source(&path, updated)
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

pub fn save_display_capture_history(
    display_id: &str,
    tool: CaptureTool,
    region: Option<CaptureRegion>,
) -> Result<(), ConfigError> {
    let path = default_config_path().ok_or(ConfigError::HomeDirectoryUnavailable)?;
    let source = read_optional_config_source(&path)?;
    let updated = update_display_capture_history(&source, display_id, tool, region)?;
    write_config_source(&path, updated)
}

fn update_display_capture_history(
    source: &str,
    display_id: &str,
    tool: CaptureTool,
    region: Option<CaptureRegion>,
) -> Result<String, ConfigError> {
    let mut document = editable_document(source)?;
    let state = &mut document["ui"]["displays"][display_id];
    state["last_capture_tool"] = toml_edit::value(tool.as_str());
    if let Some(region) = region {
        state["last_region"]["x"] = toml_edit::value(i64::from(region.x));
        state["last_region"]["y"] = toml_edit::value(i64::from(region.y));
        state["last_region"]["width"] = toml_edit::value(i64::from(region.width));
        state["last_region"]["height"] = toml_edit::value(i64::from(region.height));
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
    fs::write(path, source).map_err(|source| ConfigError::Write {
        path: path.display().to_string(),
        source,
    })
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: parent.display().to_string(),
            source,
        })?;
    }
    fs::write(&path, updated).map_err(|source| ConfigError::Write {
        path: path.display().to_string(),
        source,
    })
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: parent.display().to_string(),
            source,
        })?;
    }
    fs::write(&path, updated).map_err(|source| ConfigError::Write {
        path: path.display().to_string(),
        source,
    })
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
    pub binding: String,
    pub repeat: String,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            binding: "Ctrl+Shift+F9".to_owned(),
            repeat: "ignore".to_owned(),
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
    #[error("unsupported configuration schema version {0}")]
    UnsupportedSchema(u32),
    #[error("invalid configuration: {0}")]
    InvalidValue(String),
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

    #[test]
    fn defaults_are_valid() {
        AppConfig::default().validate().expect("valid defaults");
    }

    #[test]
    fn storage_directory_is_hidden_under_the_user_home() {
        let user_profile = PathBuf::from("user-profile-home");
        let home = PathBuf::from("home");
        assert_eq!(
            storage_directory_from(Some(user_profile.clone()), Some(home.clone())),
            Some(user_profile.join(".captastic"))
        );
        assert_eq!(
            storage_directory_from(None, Some(home.clone())),
            Some(home.join(".captastic"))
        );
        assert_eq!(
            storage_directory_from(Some(PathBuf::new()), Some(home.clone())),
            Some(home.join(".captastic"))
        );
        assert_eq!(storage_directory_from(None, None), None);
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
    fn display_ui_state_is_independent_and_survives_serialization() {
        let source = "# preserve me\n[ui]\nlast_capture_tool = \"full_display\"\noverlay_x = 40\noverlay_y = 50\n";
        let updated =
            update_display_overlay_center(source, "laptop", 0.25, 0.75).expect("laptop toolbar");
        let updated = update_display_capture_history(
            &updated,
            "laptop",
            CaptureTool::Region,
            Some(CaptureRegion {
                x: 100,
                y: 80,
                width: 960,
                height: 540,
            }),
        )
        .expect("laptop history");
        let updated = update_display_overlay_center(&updated, "external", 0.8, 0.2)
            .expect("external toolbar");
        let updated = update_display_capture_history(
            &updated,
            "external",
            CaptureTool::Window,
            Some(CaptureRegion {
                x: 300,
                y: 200,
                width: 1280,
                height: 720,
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
    fn accepts_pointer_primary_or_a_persistent_display_id() {
        let mut config = AppConfig::default();
        config.daemon.display = "pointer".to_owned();
        config.validate().expect("pointer display policy");
        config.daemon.display = "primary".to_owned();
        config.validate().expect("primary display policy");
        config.daemon.display = "display:windows-monitor-0123456789abcdef".to_owned();
        config.validate().expect("fixed display policy");
    }

    #[test]
    fn rejects_unimplemented_or_empty_display_policies() {
        for value in ["virtual_desktop", "display:", "windows-monitor-id"] {
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
}
