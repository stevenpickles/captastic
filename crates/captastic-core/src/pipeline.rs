use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError, TrySendError};

use crate::{CaptureId, CaptureRequest, CpuFrame, PipelineError};

#[derive(Clone, Debug)]
pub struct OutputJob {
    pub capture_id: CaptureId,
    pub frame: CpuFrame,
}

#[derive(Clone)]
pub struct TriggerQueue {
    sender: Sender<CaptureRequest>,
    receiver: Receiver<CaptureRequest>,
}

impl TriggerQueue {
    pub fn try_send(&self, request: CaptureRequest) -> Result<(), PipelineError> {
        self.sender.try_send(request).map_err(map_send_error)
    }

    pub fn try_recv(&self) -> Result<CaptureRequest, PipelineError> {
        self.receiver.try_recv().map_err(map_recv_error)
    }

    pub fn capacity(&self) -> Option<usize> {
        self.sender.capacity()
    }
}

#[derive(Clone)]
pub struct OutputQueue {
    sender: Sender<OutputJob>,
    receiver: Receiver<OutputJob>,
}

impl OutputQueue {
    pub fn try_send(&self, job: OutputJob) -> Result<(), PipelineError> {
        self.sender.try_send(job).map_err(map_send_error)
    }

    pub fn try_recv(&self) -> Result<OutputJob, PipelineError> {
        self.receiver.try_recv().map_err(map_recv_error)
    }

    pub fn capacity(&self) -> Option<usize> {
        self.sender.capacity()
    }
}

pub fn trigger_queue(capacity: usize) -> Result<TriggerQueue, PipelineError> {
    if capacity == 0 {
        return Err(PipelineError::InvalidCapacity);
    }
    let (sender, receiver) = bounded(capacity);
    Ok(TriggerQueue { sender, receiver })
}

pub fn output_queue(capacity: usize) -> Result<OutputQueue, PipelineError> {
    if capacity == 0 {
        return Err(PipelineError::InvalidCapacity);
    }
    let (sender, receiver) = bounded(capacity);
    Ok(OutputQueue { sender, receiver })
}

fn map_send_error<T>(error: TrySendError<T>) -> PipelineError {
    match error {
        TrySendError::Full(_) => PipelineError::Full,
        TrySendError::Disconnected(_) => PipelineError::Disconnected,
    }
}

fn map_recv_error(error: TryRecvError) -> PipelineError {
    match error {
        TryRecvError::Empty => PipelineError::Empty,
        TryRecvError::Disconnected => PipelineError::Disconnected,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::{CaptureMode, CaptureSource, CursorMode, DisplayId};

    fn request(id: u64) -> CaptureRequest {
        CaptureRequest {
            id: CaptureId(id),
            triggered_at: Instant::now(),
            source: CaptureSource::Display(DisplayId::primary()),
            mode: CaptureMode::Latest { max_age_ms: None },
            cpu_frame: false,
            retain_native_frame: false,
            cursor: CursorMode::Exclude,
        }
    }

    #[test]
    fn bounded_trigger_queue_reports_full() {
        let queue = trigger_queue(1).expect("valid queue");
        queue.try_send(request(1)).expect("first item");
        assert_eq!(queue.try_send(request(2)), Err(PipelineError::Full));
    }

    #[test]
    fn zero_capacity_is_rejected() {
        assert!(matches!(
            trigger_queue(0),
            Err(PipelineError::InvalidCapacity)
        ));
    }

    #[test]
    fn bounded_output_queue_reports_full() {
        use std::sync::Arc;

        use crate::{ColorSpace, FrameMetadata, FrameOrigin, PixelFormat, Rect, TimingProvenance};

        let metadata = FrameMetadata {
            capture_id: CaptureId(1),
            backend: "test".to_owned(),
            display_id: DisplayId::primary(),
            source_rect: Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            rotation_degrees: 0,
            capture_mode: CaptureMode::Latest { max_age_ms: None },
            presentation_offset_ns: Some(0),
            timing_provenance: TimingProvenance::Synthetic,
            native_ready_offset_ns: 1,
            cpu_ready_offset_ns: Some(2),
            frame_age_ns: Some(0),
            frame_generation: Some(1),
            copy_count: 1,
            pool_slot: None,
        };
        let frame = crate::CpuFrame::new(
            Arc::from([0_u8; 4]),
            1,
            1,
            4,
            PixelFormat::Bgra8Unorm,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            metadata,
        )
        .expect("valid frame");
        let queue = output_queue(1).expect("valid queue");
        queue
            .try_send(OutputJob {
                capture_id: CaptureId(1),
                frame: frame.clone(),
            })
            .expect("first output");
        assert_eq!(
            queue.try_send(OutputJob {
                capture_id: CaptureId(2),
                frame,
            }),
            Err(PipelineError::Full)
        );
    }
}
