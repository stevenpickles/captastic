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
    pub fn synthetic(message: impl Into<String>) -> Self {
        Self {
            kind: CaptureErrorKind::NativeFailure,
            backend: "fake",
            operation: "capture",
            message: message.into(),
            retryable: true,
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
