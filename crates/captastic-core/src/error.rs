use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureErrorKind {
    Unsupported,
    PermissionDenied,
    SourceUnavailable,
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
