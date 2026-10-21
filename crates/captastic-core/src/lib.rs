#![deny(unsafe_code)]

mod animation;
mod bmp;
mod capture;
mod detach;
mod display;
mod edit;
mod encode;
mod error;
mod fake;
mod frame;
mod jpeg;
mod metrics;
mod playback;
mod png;
#[cfg(test)]
mod test_frames;

pub use animation::{fit_within, shrink_bgra, AnimationError, AnimationSummary, GifWriter};
pub use capture::{
    BackendCapabilities, CaptureBackend, CaptureId, CaptureMode, CaptureOutcome, CaptureRequest,
    CaptureSource, CursorMode, NativeFrame,
};
pub use detach::{process_detach_ledger, DetachCount, DetachKind, DetachLedger, DetachSummary};
pub use display::{DisplayId, DisplayInfo, DisplayTopology, DisplayTopologyError, Rect};
pub use edit::{paint_mark, Color, Edit, GlyphMask, Mark, StrokeStyle};
pub use encode::{
    encode_capture, EncodeError, EncodeOptions, EncodedCapture, OutputFormat, DEFAULT_JPEG_QUALITY,
};
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
pub use playback::{
    GifReader, KeptFrame, PixelRect, PlaybackError, RecordingEdit, RecordingFrames, Timeline,
};
pub use png::{encode_frame, PngEffort, PngError};
