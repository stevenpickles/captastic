use std::any::Any;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::{CaptureError, CpuFrame, DisplayId, DisplayInfo, EventRecorder, FrameMetadata};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CaptureId(pub u64);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum CaptureMode {
    Fresh { timeout_ms: u64 },
    Latest { max_age_ms: Option<u64> },
}

impl CaptureMode {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Fresh { .. } => "fresh",
            Self::Latest { .. } => "latest",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CaptureSource {
    Display(DisplayId),
    VirtualDesktop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorMode {
    Include,
    Exclude,
}

#[derive(Clone, Debug)]
pub struct CaptureRequest {
    pub id: CaptureId,
    pub triggered_at: Instant,
    pub source: CaptureSource,
    pub mode: CaptureMode,
    pub cpu_frame: bool,
    /// Retain an immutable platform-native frame for downstream GPU materialization.
    ///
    /// Backends may ignore this request when they do not expose a native frame. Keeping this
    /// explicit prevents ordinary capture and benchmark paths from paying for an extra GPU copy.
    pub retain_native_frame: bool,
    pub cursor: CursorMode,
}

/// Type-erased ownership of a platform-native frame.
///
/// The common crate deliberately does not expose GPU handles or platform API types. Consumers
/// hand this value back to the platform crate that created it.
pub trait NativeFrame: Any + Debug + Send + Sync {
    fn as_any(&self) -> &dyn Any;
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BackendCapabilities {
    pub display_capture: bool,
    pub window_capture: bool,
    pub virtual_desktop_capture: bool,
    pub fresh_mode: bool,
    pub latest_mode: bool,
    pub cursor_control: bool,
    pub hdr: bool,
    pub presentation_time: bool,
    pub warm_stream: bool,
}

#[derive(Clone, Debug)]
pub struct CaptureOutcome {
    pub metadata: FrameMetadata,
    pub frame: Option<CpuFrame>,
    pub native_frame: Option<Arc<dyn NativeFrame>>,
    pub backend_duration: Duration,
}

pub trait CaptureBackend {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> &BackendCapabilities;
    fn displays(&self) -> &[DisplayInfo];
    fn capture(
        &mut self,
        request: &CaptureRequest,
        recorder: &mut EventRecorder,
    ) -> Result<CaptureOutcome, CaptureError>;
}
