use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("backend unavailable: {0}")]
    BackendUnavailable(String),
    #[error("failed to write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Capture(#[from] captastic_core::CaptureError),
    #[error(transparent)]
    Metrics(#[from] captastic_core::MetricsError),
    #[error(transparent)]
    Config(#[from] captastic_config::ConfigError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl AppError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::InvalidArgument(_) | Self::Config(_) | Self::Json(_) => 2,
            Self::BackendUnavailable(_) => 4,
            Self::Capture(error) if error.kind == captastic_core::CaptureErrorKind::Unsupported => {
                4
            }
            Self::Capture(_) => 6,
            Self::Metrics(_) => 10,
            Self::Write { .. } => 8,
        }
    }
}
