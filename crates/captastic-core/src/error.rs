use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureErrorKind {
    Unsupported,
    PermissionDenied,
    SourceUnavailable,
    /// The session does not currently own a desktop to capture.
    ///
    /// Distinct from [`Self::SourceUnavailable`], which it used to be reported as. That kind means
    /// the display a caller asked for is not there — a monitor unplugged, a `display =` naming
    /// hardware that has gone. This means every display is missing for the same reason and it is
    /// not about the hardware at all: the workstation is locked, a secure prompt owns the desktop,
    /// or the session is disconnected. The two look identical from inside DXGI (no attached
    /// outputs, duplication denied) and could not be less alike in what a caller should do —
    /// this one fixes itself when the user signs back in, so it is worth waiting for rather than
    /// exiting over.
    DesktopUnavailable,
    Timeout,
    AccessLost,
    DeviceRemoved,
    TopologyChanged,
    BufferExhausted,
    /// Every capture worker the backend is allowed to run is already occupied.
    ///
    /// Distinct from [`Self::BufferExhausted`], which describes transient pressure a moment's wait
    /// relieves. This capacity is only reclaimed when a worker thread exits, and a worker blocked
    /// inside an unresponsive foreign process may never exit, so an immediate retry cannot succeed
    /// and the condition is worth explaining rather than silently retrying.
    WorkersExhausted,
    /// The mouse pointer is not on any known display, so the `pointer` policy names no source.
    ///
    /// Distinct from [`Self::TopologyChanged`], which it used to be reported as. That kind means
    /// the display arrangement moved underneath a cached view of it, and the response is to
    /// rebuild and re-enumerate. This means the arrangement is understood perfectly well and the
    /// pointer is simply not on it — every non-rectangular multi-monitor layout has coordinates
    /// inside its bounding box that belong to no display, and the pointer can rest in one. No
    /// amount of rebuilding changes where the mouse is, so this is not retryable and callers are
    /// expected to choose another display rather than fail.
    PointerOutsideDisplays,
    InvalidFrame,
    NativeFailure,
    ShuttingDown,
}

/// The `operation` a backend names when it refuses a capture because the display the request
/// asked for is not one *this* backend can capture: a single-output backend bound to a different
/// display.
///
/// The kind cannot carry this. `SourceUnavailable` is also what a backend says about a display it
/// owns and could not capture from — a duplication that failed to initialize, a retained frame
/// that does not exist yet — and those mean the capture pipeline had trouble, not that the desktop
/// moved. Only this refusal and [`DISPLAY_NOT_ATTACHED`] say the request named a display the engine
/// does not have, which after a live selection is evidence that the arrangement changed between
/// the overlay and its confirmation. Named here rather than matched as a message substring,
/// because the daemon that classifies them lives in another crate from the backends that raise
/// them.
pub const DISPLAY_BINDING_REFUSED: &str = "capture_display_binding";

/// The `operation` a backend names when the display a request asked for is not attached to it at
/// all — absent from the display list it enumerated. The multi-output manager and the fake backend
/// both answer this way; see [`DISPLAY_BINDING_REFUSED`] for why the spelling is shared.
pub const DISPLAY_NOT_ATTACHED: &str = "resolve_display";

#[derive(Clone, Debug, Error)]
#[error("{kind:?} in {backend}/{operation}: {message}")]
pub struct CaptureError {
    pub kind: CaptureErrorKind,
    pub backend: &'static str,
    pub operation: &'static str,
    pub message: String,
    pub retryable: bool,
    pub native_code: Option<i64>,
}

impl CaptureError {
    /// Builds an error for a deterministic, synthetic (test/fake-backend) failure.
    ///
    /// These are raised for conditions like a configured injected failure or an arithmetic
    /// overflow while building a fake frame: the same input always reproduces the same failure,
    /// so an immediate retry cannot succeed. `retryable` is therefore always `false`; callers
    /// that need a scripted *retryable* failure should construct a `CaptureError` (or, in
    /// `captastic-core::fake`, a `FakeFailure`) directly with the flag they want.
    pub fn synthetic(message: impl Into<String>) -> Self {
        Self {
            kind: CaptureErrorKind::NativeFailure,
            backend: "fake",
            operation: "capture",
            message: message.into(),
            retryable: false,
            native_code: None,
        }
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum FrameError {
    #[error("frame dimensions must be non-zero")]
    EmptyDimensions,
    #[error("stride {stride} is smaller than the minimum row size {minimum}")]
    InvalidStride { stride: u32, minimum: u32 },
    #[error("frame byte-size calculation overflowed")]
    SizeOverflow,
    #[error("pixel buffer contains {actual} bytes but requires at least {required}")]
    BufferTooShort { actual: usize, required: usize },
    #[error("crop rectangle must have nonzero dimensions")]
    EmptyCrop,
    #[error("crop rectangle lies outside the captured source")]
    CropOutsideSource,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum MetricsError {
    #[error("capture {capture_id} emitted {current} before CPU frame readiness")]
    OutputBeforeCpuFrame {
        capture_id: u64,
        current: &'static str,
    },
    #[error(
        "capture {capture_id} event order regressed from rank {previous_rank} to {current_rank}"
    )]
    EventOrderRegression {
        capture_id: u64,
        previous_rank: u8,
        current_rank: u8,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_failures_are_not_retryable() {
        // A synthetic failure reproduces deterministically from the same input (a configured
        // injected failure, an arithmetic overflow while building a fake frame, ...), so an
        // immediate retry can never turn it into a success.
        let error = CaptureError::synthetic("configured deterministic failure");
        assert!(!error.retryable);
        assert_eq!(error.kind, CaptureErrorKind::NativeFailure);
    }
}
