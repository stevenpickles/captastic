#![deny(unsafe_code)]

mod capture;
mod detach;
mod display;
mod error;
mod fake;
mod frame;
mod metrics;
mod png;

pub use capture::{
    BackendCapabilities, CaptureBackend, CaptureId, CaptureMode, CaptureOutcome, CaptureRequest,
    CaptureSource, CursorMode, NativeFrame,
};
pub use detach::{process_detach_ledger, DetachCount, DetachKind, DetachLedger, DetachSummary};
pub use display::{DisplayId, DisplayInfo, DisplayTopology, DisplayTopologyError, Rect};
pub use error::{CaptureError, CaptureErrorKind, FrameError, MetricsError};
pub use fake::{FakeBackend, FakeBackendConfig, FakeFailure};
pub use frame::{
    ColorSpace, CpuFrame, FrameAlpha, FrameMetadata, FrameOrigin, PixelFormat, TimingProvenance,
};
pub use metrics::{validate_event_order, EventRecorder, LatencySummary, PerfEvent, PerfEventKind};
pub use png::{encode_frame, PngEffort, PngError};
