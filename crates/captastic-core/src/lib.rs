#![deny(unsafe_code)]

mod bmp;
mod capture;
mod detach;
mod display;
mod encode;
mod error;
mod fake;
mod frame;
mod metrics;
mod png;
#[cfg(test)]
mod test_frames;

pub use capture::{
    BackendCapabilities, CaptureBackend, CaptureId, CaptureMode, CaptureOutcome, CaptureRequest,
    CaptureSource, CursorMode, NativeFrame,
};
pub use detach::{process_detach_ledger, DetachCount, DetachKind, DetachLedger, DetachSummary};
pub use display::{DisplayId, DisplayInfo, DisplayTopology, DisplayTopologyError, Rect};
pub use encode::{encode_capture, EncodeError, EncodeOptions, EncodedCapture, OutputFormat};
pub use error::{
    CaptureError, CaptureErrorKind, FrameError, MetricsError, DISPLAY_BINDING_REFUSED,
    DISPLAY_NOT_ATTACHED,
};
pub use fake::{FakeBackend, FakeBackendConfig, FakeFailure};
pub use frame::{
    ColorSpace, CpuFrame, CursorAbsence, CursorCapture, FrameAlpha, FrameMetadata, FrameOrigin,
    PixelEncoding, PixelFormat, TimingProvenance,
};
pub use metrics::{validate_event_order, EventRecorder, LatencySummary, PerfEvent, PerfEventKind};
pub use png::{encode_frame, PngEffort, PngError};
