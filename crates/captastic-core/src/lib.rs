#![deny(unsafe_code)]

mod capture;
mod display;
mod error;
mod fake;
mod frame;
mod metrics;
mod pipeline;

pub use capture::{
    BackendCapabilities, CaptureBackend, CaptureId, CaptureMode, CaptureOutcome, CaptureRequest,
    CaptureSource, CursorMode, NativeFrame,
};
pub use display::{DisplayId, DisplayInfo, DisplayTopology, DisplayTopologyError, Rect};
pub use error::{CaptureError, CaptureErrorKind, FrameError, MetricsError, PipelineError};
pub use fake::{FakeBackend, FakeBackendConfig};
pub use frame::{
    ColorSpace, CpuFrame, FrameAlpha, FrameMetadata, FrameOrigin, PixelFormat, TimingProvenance,
};
pub use metrics::{validate_event_order, EventRecorder, LatencySummary, PerfEvent, PerfEventKind};
pub use pipeline::{output_queue, trigger_queue, OutputJob, OutputQueue, TriggerQueue};
