use captastic_core::{
    BackendCapabilities, CaptureBackend, CaptureError, CaptureErrorKind, CaptureOutcome,
    CaptureRequest, CaptureSource, DisplayId, DisplayInfo, EventRecorder,
};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::GetPhysicalCursorPos;

use crate::dxgi::{enumerate_displays, DxgiBackend};

struct DisplaySession {
    display_id: DisplayId,
    backend: DxgiBackend,
}

/// Retains one initialized Desktop Duplication session for every usable output.
pub struct DxgiDisplayManager {
    displays: Vec<DisplayInfo>,
    sessions: Vec<DisplaySession>,
    unavailable: Vec<(DisplayId, String)>,
    capabilities: BackendCapabilities,
}

impl DxgiDisplayManager {
    pub fn new() -> Result<Self, CaptureError> {
        let displays = enumerate_displays()?;
        if displays.is_empty() {
            return Err(manager_error(
                CaptureErrorKind::SourceUnavailable,
                "initialize",
                "no attached desktop displays were found",
                false,
            ));
        }

        let mut sessions = Vec::with_capacity(displays.len());
        let mut unavailable = Vec::new();
        for display in &displays {
            match DxgiBackend::new(&display.id) {
                Ok(backend) => {
                    log::info!(
                        "retained DXGI session initialized display={} name={:?} bounds={}x{}{:+}{:+}",
                        display.id.0,
                        display.name,
                        display.bounds.width,
                        display.bounds.height,
                        display.bounds.x,
                        display.bounds.y
                    );
                    sessions.push(DisplaySession {
                        display_id: display.id.clone(),
                        backend,
                    });
                }
                Err(error) => {
                    log::warn!(
                        "DXGI session unavailable display={} name={:?}: {error}",
                        display.id.0,
                        display.name
                    );
                    unavailable.push((display.id.clone(), error.to_string()));
                }
            }
        }
        let capabilities = sessions
            .first()
            .map(|session| session.backend.capabilities().clone())
            .ok_or_else(|| {
                manager_error(
                    CaptureErrorKind::SourceUnavailable,
                    "initialize",
                    format!(
                        "no attached display could initialize Desktop Duplication: {}",
                        unavailable
                            .iter()
                            .map(|(id, error)| format!("{} ({error})", id.0))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    true,
                )
            })?;
        Ok(Self {
            displays,
            sessions,
            unavailable,
            capabilities,
        })
    }

    fn session_index(&self, requested: &DisplayId) -> Option<usize> {
        let resolved = resolve_display_id(&self.displays, requested)?;
        self.sessions
            .iter()
            .position(|session| session.display_id == *resolved)
    }
}

impl CaptureBackend for DxgiDisplayManager {
    fn name(&self) -> &'static str {
        "dxgi"
    }

    fn capabilities(&self) -> &BackendCapabilities {
        &self.capabilities
    }

    fn displays(&self) -> &[DisplayInfo] {
        &self.displays
    }

    fn capture(
        &mut self,
        request: &CaptureRequest,
        recorder: &mut EventRecorder,
    ) -> Result<CaptureOutcome, CaptureError> {
        let CaptureSource::Display(requested) = &request.source;
        let index = self.session_index(requested).ok_or_else(|| {
            let unavailable = self
                .unavailable
                .iter()
                .find(|(id, _)| id == requested)
                .map(|(_, error)| format!("; initialization failed: {error}"))
                .unwrap_or_default();
            manager_error(
                CaptureErrorKind::SourceUnavailable,
                "route_capture",
                format!(
                    "display {} has no retained capture session{unavailable}",
                    requested.0
                ),
                true,
            )
        })?;
        self.sessions[index].backend.capture(request, recorder)
    }
}

/// Resolves the pointer once. This function does not poll or retain any input hooks.
pub fn display_containing_pointer(displays: &[DisplayInfo]) -> Result<DisplayId, CaptureError> {
    let mut point = POINT::default();
    // SAFETY: point is valid writable storage for the duration of this one-shot query.
    unsafe { GetPhysicalCursorPos(&mut point) }.map_err(|error| CaptureError {
        kind: if error.code().0 == 0x8007_0005_u32 as i32 {
            CaptureErrorKind::PermissionDenied
        } else {
            CaptureErrorKind::NativeFailure
        },
        backend: "windows",
        operation: "get_physical_cursor_position",
        message: error.to_string(),
        retryable: true,
        native_code: Some(i64::from(error.code().0)),
    })?;
    display_containing_point(displays, point.x, point.y)
        .map(|display| display.id.clone())
        .ok_or_else(|| {
            manager_error(
                CaptureErrorKind::TopologyChanged,
                "resolve_pointer_display",
                format!(
                    "pointer position ({}, {}) is outside the current display topology",
                    point.x, point.y
                ),
                true,
            )
        })
}

fn resolve_display_id<'a>(
    displays: &'a [DisplayInfo],
    requested: &DisplayId,
) -> Option<&'a DisplayId> {
    if requested.is_primary_alias() {
        displays
            .iter()
            .find(|display| display.is_primary)
            .or_else(|| displays.first())
            .map(|display| &display.id)
    } else {
        displays
            .iter()
            .find(|display| display.id == *requested)
            .map(|display| &display.id)
    }
}

fn display_containing_point(displays: &[DisplayInfo], x: i32, y: i32) -> Option<&DisplayInfo> {
    displays
        .iter()
        .filter(|display| display.bounds.contains(x, y))
        .min_by(|left, right| left.id.0.cmp(&right.id.0))
}

fn manager_error(
    kind: CaptureErrorKind,
    operation: &'static str,
    message: impl Into<String>,
    retryable: bool,
) -> CaptureError {
    CaptureError {
        kind,
        backend: "dxgi-manager",
        operation,
        message: message.into(),
        retryable,
        native_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use captastic_core::Rect;

    fn displays() -> Vec<DisplayInfo> {
        vec![
            DisplayInfo {
                id: DisplayId("laptop".to_owned()),
                name: "Laptop".to_owned(),
                bounds: Rect {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1200,
                },
                scale_factor: 1.25,
                rotation_degrees: 0,
                is_primary: true,
            },
            DisplayInfo {
                id: DisplayId("external".to_owned()),
                name: "External".to_owned(),
                bounds: Rect {
                    x: 1920,
                    y: -240,
                    width: 3840,
                    height: 2160,
                },
                scale_factor: 1.5,
                rotation_degrees: 0,
                is_primary: false,
            },
        ]
    }

    #[test]
    fn point_resolution_handles_mixed_resolutions_and_origins() {
        let displays = displays();
        assert_eq!(
            display_containing_point(&displays, 100, 100).map(|display| display.id.0.as_str()),
            Some("laptop")
        );
        assert_eq!(
            display_containing_point(&displays, 2000, -100).map(|display| display.id.0.as_str()),
            Some("external")
        );
        assert!(display_containing_point(&displays, 100, -100).is_none());
    }

    #[test]
    fn primary_alias_routes_to_the_enumerated_primary_identity() {
        let displays = displays();
        assert_eq!(
            resolve_display_id(&displays, &DisplayId::primary()).map(|id| id.0.as_str()),
            Some("laptop")
        );
        assert_eq!(
            resolve_display_id(&displays, &DisplayId("external".to_owned()))
                .map(|id| id.0.as_str()),
            Some("external")
        );
    }
}
