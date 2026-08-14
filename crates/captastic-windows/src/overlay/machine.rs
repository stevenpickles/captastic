//! The overlay's product-state machine: pure decisions about drags, selection geometry, capture
//! ownership, and closing, separated from the Win32 window procedure that translates messages
//! into [`OverlayInput`]s and applies the returned [`OverlayEffect`]s.
//!
//! Nothing in this module touches a window, a device context, or a clock. The window shell owns
//! every handle and every reentrancy protocol (most notably the `ReleaseCapture` →
//! `WM_CAPTURECHANGED` round trip); the machine only ever sees the product-level fact that
//! pointer capture was lost to another window.

use windows::Win32::Foundation::POINT;

use captastic_core::Rect;

use super::layout::{DisplayEnvironment, ToolbarControl, ToolbarLayout, UiMetrics};
use super::{NativeWindowHandle, SelectionKind};

pub(super) const DRAG_THRESHOLD: i32 = 4;
pub(super) const MIN_REGION_SIZE: i64 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CaptureTool {
    FullDisplay,
    Window,
    Region,
}

impl CaptureTool {
    pub(super) const fn from_selection_kind(kind: SelectionKind) -> Self {
        match kind {
            SelectionKind::Display => Self::FullDisplay,
            SelectionKind::Region => Self::Region,
            SelectionKind::Window => Self::Window,
        }
    }

    pub(super) const fn from_config(tool: captastic_config::CaptureTool) -> Self {
        match tool {
            captastic_config::CaptureTool::FullDisplay => Self::FullDisplay,
            captastic_config::CaptureTool::Window => Self::Window,
            captastic_config::CaptureTool::Region => Self::Region,
        }
    }

    pub(super) const fn to_config(self) -> captastic_config::CaptureTool {
        match self {
            Self::FullDisplay => captastic_config::CaptureTool::FullDisplay,
            Self::Window => captastic_config::CaptureTool::Window,
            Self::Region => captastic_config::CaptureTool::Region,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ResizeHandle {
    NorthWest,
    North,
    NorthEast,
    East,
    SouthEast,
    South,
    SouthWest,
    West,
}

#[derive(Clone, Copy)]
pub(super) struct ResizeDrag {
    pub(super) handle: ResizeHandle,
    pub(super) original: Rect,
}

#[derive(Clone, Copy)]
pub(super) struct MoveDrag {
    pub(super) original: Rect,
    pub(super) pointer_origin: POINT,
}

#[derive(Clone, Copy)]
pub(super) struct ToolbarDrag {
    pub(super) pointer_offset: POINT,
}

/// Product state the overlay owns: selection geometry, drag ownership, and toolbar posture.
///
/// Deliberately excluded: window handles, GDI surfaces and caches, the window-chooser thumbnail
/// inventory, telemetry clocks, and the persistence sink. Those belong to the shell; the machine
/// reaches them only through effects.
pub(super) struct OverlayModel {
    pub(super) source: Rect,
    pub(super) display_environment: DisplayEnvironment,
    pub(super) tool: CaptureTool,
    pub(super) selection: Option<Rect>,
    pub(super) selection_kind: Option<SelectionKind>,
    pub(super) selected_window: Option<NativeWindowHandle>,
    pub(super) anchor: Option<POINT>,
    pub(super) dragging: bool,
    pub(super) resizing: Option<ResizeDrag>,
    pub(super) moving_region: Option<MoveDrag>,
    pub(super) hovered_handle: Option<ResizeHandle>,
    pub(super) last_region: Option<Rect>,
    pub(super) toolbar_position: POINT,
    pub(super) toolbar_drag: Option<ToolbarDrag>,
    pub(super) options_open: bool,
    pub(super) dim_background: bool,
    pub(super) hovered_control: Option<ToolbarControl>,
    pub(super) pointer_local: Option<POINT>,
}

/// A translated window message. The shell decodes `LPARAM`/`WPARAM`/message identity; the
/// machine never sees Win32 encodings.
#[derive(Clone, Copy, Debug)]
pub(super) enum OverlayInput {
    /// Pointer moved to a screen-space point. Routed here unless the window-chooser hover pass
    /// (not yet modeled) owns the message.
    PointerMoved { point: POINT },
    /// Primary button pressed outside the toolbar while the region tool is active. The shell's
    /// router has already dispatched toolbar hits and non-region tools to unmodeled paths.
    PointerDown { point: POINT },
    /// Primary button released. Fully modeled for every tool.
    PointerUp { point: POINT },
    /// Primary button double-clicked anywhere.
    DoubleClicked { point: POINT },
    /// Pointer capture was taken by another window. Self-initiated releases are consumed by the
    /// shell's protocol flag and never reach the machine.
    PointerCaptureLost,
    /// Enter pressed.
    ConfirmRequested,
    /// Escape, right-click, or an external close request.
    CancelRequested,
    /// The display configuration changed under the overlay; its geometry is no longer current.
    DisplayConfigurationInvalidated { reason: &'static str },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CursorIntent {
    Arrow,
    Move,
    /// The region crosshair cursor.
    Crosshair,
    Resize(ResizeHandle),
}

/// How the run ends. `Confirmed` carries only product data; the shell attaches the captured
/// window frame and telemetry when it builds the final selection.
#[derive(Clone, Copy, Debug)]
pub(super) enum CloseOutcome {
    Confirmed {
        rect: Rect,
        kind: SelectionKind,
        window: Option<NativeWindowHandle>,
    },
    Cancelled,
    /// Close without persisting geometry from a configuration that is no longer current.
    DisplayConfigurationInvalidated {
        reason: &'static str,
    },
}

/// A declared side effect. Effects carry owned data so the shell can apply them after the model
/// borrow has ended — reentrant messages triggered by an effect can therefore never observe a
/// live `&mut` of the model.
#[derive(Clone, Copy, Debug)]
pub(super) enum OverlayEffect {
    SetCursor(CursorIntent),
    Invalidate,
    CapturePointer,
    /// Release pointer capture. The shell owns the `releasing_pointer_capture` protocol flag
    /// that distinguishes the resulting self-generated `WM_CAPTURECHANGED` from a real loss.
    ReleasePointer,
    /// Clear the window-chooser hover highlight (shell-owned state, not yet modeled).
    ClearWindowHover,
    /// Clear the dimension-label placement hysteresis (render-scratch state).
    ClearDimensionLabel,
    /// Re-register live thumbnails and rebuild the window-overview cache.
    RefreshWindowChrome,
    /// Persist the toolbar's resting position for this display.
    PersistToolbarCenter {
        position: POINT,
    },
    /// Persist the latest interaction (tool + region) for this display.
    PersistInteraction {
        region: Option<Rect>,
    },
    /// Destroy the overlay window with the given outcome. Always the final effect.
    Close(CloseOutcome),
}

/// Advances the model by one input and returns the effects the shell must apply, in order.
pub(super) fn transition(model: &mut OverlayModel, input: OverlayInput) -> Vec<OverlayEffect> {
    match input {
        OverlayInput::PointerMoved { point } => pointer_moved(model, point),
        OverlayInput::PointerDown { point } => pointer_down(model, point),
        OverlayInput::PointerUp { point } => pointer_up(model, point),
        OverlayInput::DoubleClicked { point } => double_clicked(model, point),
        OverlayInput::PointerCaptureLost => pointer_capture_lost(model),
        OverlayInput::ConfirmRequested => confirm(model),
        OverlayInput::CancelRequested => cancel(model),
        OverlayInput::DisplayConfigurationInvalidated { reason } => {
            vec![OverlayEffect::Close(
                CloseOutcome::DisplayConfigurationInvalidated { reason },
            )]
        }
    }
}

fn pointer_moved(model: &mut OverlayModel, point: POINT) -> Vec<OverlayEffect> {
    let local = local_point(model.source, point);
    model.pointer_local = Some(local);
    let mut effects = Vec::new();
    if let Some(drag) = model.toolbar_drag {
        model.toolbar_position = ToolbarLayout::clamp_origin(
            model.display_environment,
            POINT {
                x: local.x.saturating_sub(drag.pointer_offset.x),
                y: local.y.saturating_sub(drag.pointer_offset.y),
            },
        );
        model.hovered_control = Some(ToolbarControl::Background);
        model.hovered_handle = None;
        effects.push(OverlayEffect::ClearWindowHover);
        effects.push(OverlayEffect::SetCursor(CursorIntent::Arrow));
        effects.push(OverlayEffect::Invalidate);
        return effects;
    }
    let layout = ToolbarLayout::new(model.display_environment, model.toolbar_position);
    let region_drag_active =
        model.resizing.is_some() || model.moving_region.is_some() || model.anchor.is_some();
    model.hovered_control = (!region_drag_active)
        .then(|| layout.hit_test(local, model.options_open))
        .flatten();
    if model.hovered_control.is_some() {
        effects.push(OverlayEffect::ClearWindowHover);
        effects.push(OverlayEffect::SetCursor(CursorIntent::Arrow));
    } else if let Some(resize) = model.resizing {
        model.selection = Some(resize_region(
            resize.original,
            resize.handle,
            point,
            model.source,
        ));
        model.hovered_handle = Some(resize.handle);
        effects.push(OverlayEffect::SetCursor(CursorIntent::Resize(
            resize.handle,
        )));
    } else if let Some(moving) = model.moving_region {
        model.selection = Some(move_region(
            moving.original,
            moving.pointer_origin,
            point,
            model.source,
        ));
        model.hovered_handle = None;
        effects.push(OverlayEffect::SetCursor(CursorIntent::Move));
    } else if let Some(anchor) = model.anchor {
        model.dragging |= (point.x - anchor.x).abs() >= DRAG_THRESHOLD
            || (point.y - anchor.y).abs() >= DRAG_THRESHOLD;
        if model.dragging {
            model.selection = rect_from_points(model.source, anchor, point);
            model.selection_kind = Some(SelectionKind::Region);
            model.selected_window = None;
            effects.push(OverlayEffect::ClearWindowHover);
        }
        model.hovered_handle = None;
        effects.push(OverlayEffect::SetCursor(CursorIntent::Crosshair));
    } else if model.tool == CaptureTool::Region
        && model.selection_kind == Some(SelectionKind::Region)
    {
        model.hovered_handle = model.selection.and_then(|selection| {
            hit_test_resize_handle(selection, point, model.display_environment.metrics)
        });
        effects.push(OverlayEffect::ClearWindowHover);
        if model.hovered_handle.is_none()
            && model
                .selection
                .is_some_and(|selection| contains(selection, point))
        {
            effects.push(OverlayEffect::SetCursor(CursorIntent::Move));
        } else {
            effects.push(OverlayEffect::SetCursor(cursor_for_handle(
                model.hovered_handle,
            )));
        }
    } else {
        effects.push(OverlayEffect::ClearWindowHover);
        model.hovered_handle = None;
        if model.tool == CaptureTool::Region {
            effects.push(OverlayEffect::SetCursor(CursorIntent::Crosshair));
        } else {
            effects.push(OverlayEffect::SetCursor(CursorIntent::Arrow));
        }
    }
    effects.push(OverlayEffect::Invalidate);
    effects
}

fn pointer_down(model: &mut OverlayModel, point: POINT) -> Vec<OverlayEffect> {
    debug_assert_eq!(model.tool, CaptureTool::Region);
    model.hovered_control = None;
    model.options_open = false;
    let existing_region = (model.selection_kind == Some(SelectionKind::Region))
        .then_some(model.selection)
        .flatten();
    let resize_handle = existing_region.and_then(|selection| {
        hit_test_resize_handle(selection, point, model.display_environment.metrics)
    });
    if let (Some(original), Some(handle)) = (existing_region, resize_handle) {
        model.resizing = Some(ResizeDrag { handle, original });
        model.moving_region = None;
        model.hovered_handle = Some(handle);
        model.anchor = None;
        model.dragging = false;
        vec![
            OverlayEffect::SetCursor(CursorIntent::Resize(handle)),
            OverlayEffect::CapturePointer,
            OverlayEffect::Invalidate,
        ]
    } else if existing_region.is_some_and(|selection| contains(selection, point)) {
        model.moving_region = existing_region.map(|original| MoveDrag {
            original,
            pointer_origin: point,
        });
        model.anchor = None;
        model.dragging = false;
        model.hovered_handle = None;
        vec![
            OverlayEffect::SetCursor(CursorIntent::Move),
            OverlayEffect::CapturePointer,
            OverlayEffect::Invalidate,
        ]
    } else {
        model.anchor = Some(point);
        model.dragging = false;
        model.resizing = None;
        model.moving_region = None;
        model.selection = None;
        model.selection_kind = None;
        model.selected_window = None;
        model.hovered_handle = None;
        vec![
            OverlayEffect::ClearWindowHover,
            OverlayEffect::ClearDimensionLabel,
            OverlayEffect::SetCursor(CursorIntent::Crosshair),
            OverlayEffect::CapturePointer,
            OverlayEffect::Invalidate,
        ]
    }
}

fn pointer_up(model: &mut OverlayModel, point: POINT) -> Vec<OverlayEffect> {
    // The release always comes first so the shell's protocol flag is armed before any
    // reentrant WM_CAPTURECHANGED can arrive — the commit below is already in the model by
    // then, and a self-generated capture change must not erase it.
    let mut effects = vec![OverlayEffect::ReleasePointer];
    if model.toolbar_drag.take().is_some() {
        effects.push(OverlayEffect::PersistToolbarCenter {
            position: model.toolbar_position,
        });
        if model.tool == CaptureTool::Window {
            effects.push(OverlayEffect::RefreshWindowChrome);
        }
        model.hovered_control = Some(ToolbarControl::Background);
        effects.push(OverlayEffect::SetCursor(CursorIntent::Arrow));
        effects.push(OverlayEffect::Invalidate);
        return effects;
    }
    if model.tool != CaptureTool::Region {
        return effects;
    }
    if let Some(resize) = model.resizing.take() {
        model.selection = Some(resize_region(
            resize.original,
            resize.handle,
            point,
            model.source,
        ));
        model.selection_kind = Some(SelectionKind::Region);
        model.selected_window = None;
    } else if let Some(moving) = model.moving_region.take() {
        model.selection = Some(move_region(
            moving.original,
            moving.pointer_origin,
            point,
            model.source,
        ));
        model.selection_kind = Some(SelectionKind::Region);
        model.selected_window = None;
    } else if let Some(anchor) = model.anchor.take() {
        if model.dragging {
            model.selection = rect_from_points(model.source, anchor, point);
            model.selection_kind = Some(SelectionKind::Region);
            model.selected_window = None;
        }
    }
    model.dragging = false;
    model.hovered_handle = if model.selection_kind == Some(SelectionKind::Region) {
        model.selection.and_then(|selection| {
            hit_test_resize_handle(selection, point, model.display_environment.metrics)
        })
    } else {
        None
    };
    if model.hovered_handle.is_none()
        && model
            .selection
            .is_some_and(|selection| contains(selection, point))
    {
        effects.push(OverlayEffect::SetCursor(CursorIntent::Move));
    } else {
        effects.push(OverlayEffect::SetCursor(cursor_for_handle(
            model.hovered_handle,
        )));
    }
    effects.push(OverlayEffect::Invalidate);
    effects
}

fn double_clicked(model: &mut OverlayModel, point: POINT) -> Vec<OverlayEffect> {
    let local = local_point(model.source, point);
    let layout = ToolbarLayout::new(model.display_environment, model.toolbar_position);
    if layout.hit_test(local, model.options_open).is_some() {
        return Vec::new();
    }
    confirm(model)
}

fn pointer_capture_lost(model: &mut OverlayModel) -> Vec<OverlayEffect> {
    let toolbar_dragging = model.toolbar_drag.take().is_some();
    model.resizing = None;
    model.moving_region = None;
    model.anchor = None;
    model.dragging = false;
    model.hovered_handle = None;
    let mut effects = Vec::new();
    // An abandoned toolbar drag deliberately does not persist the position: the pointer never
    // came to rest, so the last committed position stays authoritative.
    if toolbar_dragging && model.tool == CaptureTool::Window {
        effects.push(OverlayEffect::RefreshWindowChrome);
    }
    effects.push(OverlayEffect::SetCursor(CursorIntent::Arrow));
    effects.push(OverlayEffect::Invalidate);
    effects
}

fn confirm(model: &mut OverlayModel) -> Vec<OverlayEffect> {
    let (Some(rect), Some(kind)) = (model.selection, model.selection_kind) else {
        return Vec::new();
    };
    debug_assert_eq!(model.tool, CaptureTool::from_selection_kind(kind));
    let mut effects = persist_interaction(model);
    effects.push(OverlayEffect::Close(CloseOutcome::Confirmed {
        rect,
        kind,
        window: model.selected_window,
    }));
    effects
}

fn cancel(model: &mut OverlayModel) -> Vec<OverlayEffect> {
    let mut effects = persist_interaction(model);
    effects.push(OverlayEffect::Close(CloseOutcome::Cancelled));
    effects
}

fn persist_interaction(model: &mut OverlayModel) -> Vec<OverlayEffect> {
    let region = latest_interaction_region(
        model.tool,
        model.selection,
        model.selection_kind,
        model.last_region,
    );
    model.last_region = region;
    vec![OverlayEffect::PersistInteraction { region }]
}

const fn cursor_for_handle(handle: Option<ResizeHandle>) -> CursorIntent {
    match handle {
        Some(handle) => CursorIntent::Resize(handle),
        None => CursorIntent::Crosshair,
    }
}

pub(super) fn local_point(source: Rect, point: POINT) -> POINT {
    POINT {
        x: point.x.saturating_sub(source.x),
        y: point.y.saturating_sub(source.y),
    }
}

pub(super) fn latest_interaction_region(
    tool: CaptureTool,
    selection: Option<Rect>,
    selection_kind: Option<SelectionKind>,
    last_region: Option<Rect>,
) -> Option<Rect> {
    if tool == CaptureTool::Region && selection_kind == Some(SelectionKind::Region) {
        selection.or(last_region)
    } else {
        last_region
    }
}

pub(super) fn rect_from_points(source: Rect, first: POINT, second: POINT) -> Option<Rect> {
    let source_right = i64::from(source.x) + i64::from(source.width);
    let source_bottom = i64::from(source.y) + i64::from(source.height);
    let left = i64::from(first.x.min(second.x)).clamp(i64::from(source.x), source_right);
    let top = i64::from(first.y.min(second.y)).clamp(i64::from(source.y), source_bottom);
    let right = i64::from(first.x.max(second.x)).clamp(i64::from(source.x), source_right);
    let bottom = i64::from(first.y.max(second.y)).clamp(i64::from(source.y), source_bottom);
    (right > left && bottom > top).then_some(Rect {
        x: left as i32,
        y: top as i32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

pub(super) fn contains(rect: Rect, point: POINT) -> bool {
    let right = i64::from(rect.x) + i64::from(rect.width);
    let bottom = i64::from(rect.y) + i64::from(rect.height);
    i64::from(point.x) >= i64::from(rect.x)
        && i64::from(point.y) >= i64::from(rect.y)
        && i64::from(point.x) < right
        && i64::from(point.y) < bottom
}

pub(super) fn hit_test_resize_handle(
    rect: Rect,
    point: POINT,
    metrics: UiMetrics,
) -> Option<ResizeHandle> {
    let left = i64::from(rect.x);
    let top = i64::from(rect.y);
    let right = left + i64::from(rect.width);
    let bottom = top + i64::from(rect.height);
    let x = i64::from(point.x);
    let y = i64::from(point.y);
    let radius = i64::from(metrics.region_tokens().handle_hit_radius);
    let near_left = (x - left).abs() <= radius;
    let near_right = (x - right).abs() <= radius;
    let near_top = (y - top).abs() <= radius;
    let near_bottom = (y - bottom).abs() <= radius;
    if near_left && near_top {
        Some(ResizeHandle::NorthWest)
    } else if near_right && near_top {
        Some(ResizeHandle::NorthEast)
    } else if near_right && near_bottom {
        Some(ResizeHandle::SouthEast)
    } else if near_left && near_bottom {
        Some(ResizeHandle::SouthWest)
    } else if near_top && x >= left && x <= right {
        Some(ResizeHandle::North)
    } else if near_right && y >= top && y <= bottom {
        Some(ResizeHandle::East)
    } else if near_bottom && x >= left && x <= right {
        Some(ResizeHandle::South)
    } else if near_left && y >= top && y <= bottom {
        Some(ResizeHandle::West)
    } else {
        None
    }
}

pub(super) fn resize_region(
    original: Rect,
    handle: ResizeHandle,
    point: POINT,
    source: Rect,
) -> Rect {
    let source_left = i64::from(source.x);
    let source_top = i64::from(source.y);
    let source_right = source_left + i64::from(source.width);
    let source_bottom = source_top + i64::from(source.height);
    let mut left = i64::from(original.x);
    let mut top = i64::from(original.y);
    let mut right = left + i64::from(original.width);
    let mut bottom = top + i64::from(original.height);
    let point_x = i64::from(point.x);
    let point_y = i64::from(point.y);

    if matches!(
        handle,
        ResizeHandle::NorthWest | ResizeHandle::West | ResizeHandle::SouthWest
    ) {
        let maximum_left = (right - MIN_REGION_SIZE).max(source_left);
        left = point_x.clamp(source_left, maximum_left);
    }
    if matches!(
        handle,
        ResizeHandle::NorthEast | ResizeHandle::East | ResizeHandle::SouthEast
    ) {
        let minimum_right = (left + MIN_REGION_SIZE).min(source_right);
        right = point_x.clamp(minimum_right, source_right);
    }
    if matches!(
        handle,
        ResizeHandle::NorthWest | ResizeHandle::North | ResizeHandle::NorthEast
    ) {
        let maximum_top = (bottom - MIN_REGION_SIZE).max(source_top);
        top = point_y.clamp(source_top, maximum_top);
    }
    if matches!(
        handle,
        ResizeHandle::SouthWest | ResizeHandle::South | ResizeHandle::SouthEast
    ) {
        let minimum_bottom = (top + MIN_REGION_SIZE).min(source_bottom);
        bottom = point_y.clamp(minimum_bottom, source_bottom);
    }

    Rect {
        x: left as i32,
        y: top as i32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    }
}

pub(super) fn move_region(
    original: Rect,
    pointer_origin: POINT,
    point: POINT,
    source: Rect,
) -> Rect {
    let delta_x = i64::from(point.x) - i64::from(pointer_origin.x);
    let delta_y = i64::from(point.y) - i64::from(pointer_origin.y);
    let source_left = i64::from(source.x);
    let source_top = i64::from(source.y);
    let maximum_x = source_left + i64::from(source.width.saturating_sub(original.width));
    let maximum_y = source_top + i64::from(source.height.saturating_sub(original.height));
    Rect {
        x: (i64::from(original.x) + delta_x).clamp(source_left, maximum_x) as i32,
        y: (i64::from(original.y) + delta_y).clamp(source_top, maximum_y) as i32,
        width: original.width,
        height: original.height,
    }
}

#[cfg(test)]
mod tests {
    use super::super::layout::UiRect;
    use super::*;

    const SOURCE: Rect = Rect {
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
    };

    fn environment() -> DisplayEnvironment {
        DisplayEnvironment {
            work_area: UiRect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1040,
            },
            metrics: UiMetrics::new(UiMetrics::BASE_DPI),
        }
    }

    fn region_model() -> OverlayModel {
        OverlayModel {
            source: SOURCE,
            display_environment: environment(),
            tool: CaptureTool::Region,
            selection: None,
            selection_kind: None,
            selected_window: None,
            anchor: None,
            dragging: false,
            resizing: None,
            moving_region: None,
            hovered_handle: None,
            last_region: None,
            // Off-screen-ish corner so pointer coordinates in tests never hit the toolbar.
            toolbar_position: POINT { x: 1500, y: 950 },
            toolbar_drag: None,
            options_open: false,
            dim_background: true,
            hovered_control: None,
            pointer_local: None,
        }
    }

    fn point(x: i32, y: i32) -> POINT {
        POINT { x, y }
    }

    fn has_effect(effects: &[OverlayEffect], matcher: impl Fn(&OverlayEffect) -> bool) -> bool {
        effects.iter().any(matcher)
    }

    fn close_outcome(effects: &[OverlayEffect]) -> Option<CloseOutcome> {
        effects.iter().find_map(|effect| match effect {
            OverlayEffect::Close(outcome) => Some(*outcome),
            _ => None,
        })
    }

    fn persisted_region(effects: &[OverlayEffect]) -> Option<Option<Rect>> {
        effects.iter().find_map(|effect| match effect {
            OverlayEffect::PersistInteraction { region } => Some(*region),
            _ => None,
        })
    }

    #[test]
    fn a_new_region_drag_latches_at_the_threshold_and_commits_on_release() {
        let mut model = region_model();
        let effects = transition(
            &mut model,
            OverlayInput::PointerDown {
                point: point(100, 100),
            },
        );
        assert!(model.anchor.is_some());
        assert!(!model.dragging);
        assert!(has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::CapturePointer
        )));
        assert!(has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::ClearDimensionLabel
        )));

        // One pixel under the threshold: still a click, not a drag.
        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(103, 100),
            },
        );
        assert!(!model.dragging);
        assert_eq!(model.selection, None);

        // Crossing the threshold starts the rubber band.
        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(104, 100),
            },
        );
        assert!(model.dragging);
        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(300, 240),
            },
        );
        assert_eq!(model.selection_kind, Some(SelectionKind::Region));

        let effects = transition(
            &mut model,
            OverlayInput::PointerUp {
                point: point(300, 240),
            },
        );
        assert!(matches!(effects[0], OverlayEffect::ReleasePointer));
        assert_eq!(
            model.selection,
            Some(Rect {
                x: 100,
                y: 100,
                width: 200,
                height: 140,
            })
        );
        assert_eq!(model.selection_kind, Some(SelectionKind::Region));
        assert_eq!(model.anchor, None);
        assert!(!model.dragging);
        assert!(close_outcome(&effects).is_none(), "button-up never closes");
    }

    #[test]
    fn a_sub_threshold_click_commits_nothing() {
        let mut model = region_model();
        transition(
            &mut model,
            OverlayInput::PointerDown {
                point: point(100, 100),
            },
        );
        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(102, 101),
            },
        );
        let effects = transition(
            &mut model,
            OverlayInput::PointerUp {
                point: point(102, 101),
            },
        );
        assert!(matches!(effects[0], OverlayEffect::ReleasePointer));
        assert_eq!(model.selection, None);
        assert_eq!(model.selection_kind, None);
        assert_eq!(model.anchor, None);
    }

    #[test]
    fn a_resize_drag_updates_the_selection_and_commits_on_release() {
        let region = Rect {
            x: 100,
            y: 100,
            width: 200,
            height: 200,
        };
        let mut model = region_model();
        model.selection = Some(region);
        model.selection_kind = Some(SelectionKind::Region);

        let effects = transition(
            &mut model,
            OverlayInput::PointerDown {
                point: point(300, 300),
            },
        );
        let resize = model
            .resizing
            .expect("south-east handle grabs a resize drag");
        assert_eq!(resize.handle, ResizeHandle::SouthEast);
        assert!(has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::CapturePointer
        )));
        assert!(has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::SetCursor(CursorIntent::Resize(ResizeHandle::SouthEast))
        )));

        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(340, 360),
            },
        );
        assert_eq!(
            model.selection,
            Some(Rect {
                x: 100,
                y: 100,
                width: 240,
                height: 260,
            })
        );

        transition(
            &mut model,
            OverlayInput::PointerUp {
                point: point(340, 360),
            },
        );
        assert_eq!(model.resizing.map(|drag| drag.handle), None);
        assert_eq!(
            model.selection,
            Some(Rect {
                x: 100,
                y: 100,
                width: 240,
                height: 260,
            })
        );
        assert_eq!(model.selection_kind, Some(SelectionKind::Region));
    }

    #[test]
    fn a_move_drag_preserves_dimensions_and_commits_on_release() {
        let region = Rect {
            x: 100,
            y: 100,
            width: 200,
            height: 150,
        };
        let mut model = region_model();
        model.selection = Some(region);
        model.selection_kind = Some(SelectionKind::Region);

        transition(
            &mut model,
            OverlayInput::PointerDown {
                point: point(180, 160),
            },
        );
        assert!(model.moving_region.is_some());

        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(230, 200),
            },
        );
        transition(
            &mut model,
            OverlayInput::PointerUp {
                point: point(230, 200),
            },
        );
        assert_eq!(model.moving_region.map(|drag| drag.original), None);
        assert_eq!(
            model.selection,
            Some(Rect {
                x: 150,
                y: 140,
                width: 200,
                height: 150,
            })
        );
    }

    #[test]
    fn an_externally_stolen_capture_abandons_the_drag_without_committing() {
        let mut model = region_model();
        transition(
            &mut model,
            OverlayInput::PointerDown {
                point: point(100, 100),
            },
        );
        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(200, 200),
            },
        );
        assert!(model.dragging);

        let effects = transition(&mut model, OverlayInput::PointerCaptureLost);
        assert_eq!(model.anchor, None);
        assert!(!model.dragging);
        assert!(model.resizing.is_none());
        assert!(model.moving_region.is_none());
        assert_eq!(model.hovered_handle, None);
        assert!(close_outcome(&effects).is_none());
        assert!(persisted_region(&effects).is_none());
        assert!(!has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::ReleasePointer
        )));
    }

    #[test]
    fn a_self_initiated_release_never_reaches_the_machine_so_the_commit_survives() {
        // The shell consumes the protocol flag for the reentrant WM_CAPTURECHANGED; the machine
        // therefore commits on PointerUp without any capture-lost input in between. This test
        // pins the machine half of that contract: the commit happens inside the PointerUp
        // transition itself, with the release emitted first for the shell to arm its flag.
        let mut model = region_model();
        transition(
            &mut model,
            OverlayInput::PointerDown {
                point: point(10, 10),
            },
        );
        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(90, 60),
            },
        );
        let effects = transition(
            &mut model,
            OverlayInput::PointerUp {
                point: point(90, 60),
            },
        );
        assert!(matches!(effects[0], OverlayEffect::ReleasePointer));
        assert_eq!(
            model.selection,
            Some(Rect {
                x: 10,
                y: 10,
                width: 80,
                height: 50,
            })
        );
    }

    #[test]
    fn confirm_persists_the_interaction_then_closes_with_a_consistent_kind() {
        let cases = [
            (CaptureTool::FullDisplay, SelectionKind::Display),
            (CaptureTool::Region, SelectionKind::Region),
            (CaptureTool::Window, SelectionKind::Window),
        ];
        for (tool, kind) in cases {
            let mut model = region_model();
            model.tool = tool;
            model.selection = Some(Rect {
                x: 5,
                y: 6,
                width: 70,
                height: 80,
            });
            model.selection_kind = Some(kind);

            let effects = transition(&mut model, OverlayInput::ConfirmRequested);
            let positions: Vec<_> = effects
                .iter()
                .map(|effect| match effect {
                    OverlayEffect::PersistInteraction { .. } => "persist",
                    OverlayEffect::Close(_) => "close",
                    _ => "other",
                })
                .collect();
            assert_eq!(positions, ["persist", "close"], "tool {tool:?}");
            match close_outcome(&effects) {
                Some(CloseOutcome::Confirmed { kind: closed, .. }) => {
                    assert_eq!(closed, kind);
                    assert_eq!(tool, CaptureTool::from_selection_kind(closed));
                }
                other => panic!("expected a confirmed close, got {other:?}"),
            }
        }
    }

    #[test]
    fn confirm_without_a_selection_does_nothing() {
        let mut model = region_model();
        assert!(transition(&mut model, OverlayInput::ConfirmRequested).is_empty());
    }

    #[test]
    fn cancel_persists_the_latest_region_and_closes() {
        let region = Rect {
            x: 40,
            y: 40,
            width: 100,
            height: 100,
        };
        let mut model = region_model();
        model.selection = Some(region);
        model.selection_kind = Some(SelectionKind::Region);

        let effects = transition(&mut model, OverlayInput::CancelRequested);
        assert_eq!(persisted_region(&effects), Some(Some(region)));
        assert_eq!(model.last_region, Some(region));
        assert!(matches!(
            close_outcome(&effects),
            Some(CloseOutcome::Cancelled)
        ));
    }

    #[test]
    fn a_display_change_closes_without_persisting() {
        let mut model = region_model();
        model.selection = Some(Rect {
            x: 1,
            y: 1,
            width: 10,
            height: 10,
        });
        model.selection_kind = Some(SelectionKind::Region);

        let effects = transition(
            &mut model,
            OverlayInput::DisplayConfigurationInvalidated {
                reason: "overlay_dpi_changed",
            },
        );
        assert_eq!(effects.len(), 1);
        assert!(matches!(
            close_outcome(&effects),
            Some(CloseOutcome::DisplayConfigurationInvalidated { .. })
        ));
        assert!(persisted_region(&effects).is_none());
    }

    #[test]
    fn a_toolbar_drag_tracks_clamped_and_persists_only_on_release() {
        let mut model = region_model();
        model.toolbar_position = POINT { x: 300, y: 300 };
        model.toolbar_drag = Some(ToolbarDrag {
            pointer_offset: POINT { x: 10, y: 10 },
        });

        let effects = transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(500, 400),
            },
        );
        assert_eq!(model.hovered_control, Some(ToolbarControl::Background));
        assert!(!has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::PersistToolbarCenter { .. }
        )));
        let tracked = model.toolbar_position;
        assert_eq!(
            POINT {
                x: tracked.x,
                y: tracked.y
            },
            ToolbarLayout::clamp_origin(model.display_environment, POINT { x: 490, y: 390 })
        );

        let effects = transition(
            &mut model,
            OverlayInput::PointerUp {
                point: point(500, 400),
            },
        );
        assert!(matches!(effects[0], OverlayEffect::ReleasePointer));
        assert!(has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::PersistToolbarCenter { position }
                if position.x == tracked.x && position.y == tracked.y
        )));
        assert_eq!(model.toolbar_drag.map(|drag| drag.pointer_offset.x), None);
        // Region tool: no window chrome to refresh.
        assert!(!has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::RefreshWindowChrome
        )));
    }

    #[test]
    fn an_abandoned_toolbar_drag_does_not_persist_but_refreshes_window_chrome() {
        let mut model = region_model();
        model.tool = CaptureTool::Window;
        model.toolbar_drag = Some(ToolbarDrag {
            pointer_offset: POINT { x: 0, y: 0 },
        });

        let effects = transition(&mut model, OverlayInput::PointerCaptureLost);
        assert!(model.toolbar_drag.is_none());
        assert!(!has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::PersistToolbarCenter { .. }
        )));
        assert!(has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::RefreshWindowChrome
        )));
    }

    #[test]
    fn button_up_outside_the_region_tool_only_releases_capture() {
        for tool in [CaptureTool::FullDisplay, CaptureTool::Window] {
            let mut model = region_model();
            model.tool = tool;
            let effects = transition(
                &mut model,
                OverlayInput::PointerUp {
                    point: point(50, 50),
                },
            );
            assert_eq!(effects.len(), 1, "tool {tool:?}");
            assert!(matches!(effects[0], OverlayEffect::ReleasePointer));
        }
    }

    #[test]
    fn a_double_click_on_the_toolbar_is_inert_and_elsewhere_confirms() {
        let mut model = region_model();
        model.selection = Some(Rect {
            x: 10,
            y: 10,
            width: 50,
            height: 50,
        });
        model.selection_kind = Some(SelectionKind::Region);

        let on_toolbar = POINT {
            x: model.toolbar_position.x + 4,
            y: model.toolbar_position.y + 4,
        };
        assert!(transition(
            &mut model,
            OverlayInput::DoubleClicked { point: on_toolbar }
        )
        .is_empty());

        let effects = transition(
            &mut model,
            OverlayInput::DoubleClicked {
                point: point(20, 20),
            },
        );
        assert!(matches!(
            close_outcome(&effects),
            Some(CloseOutcome::Confirmed { .. })
        ));
    }

    #[test]
    fn close_is_always_the_final_effect() {
        let inputs = [
            OverlayInput::ConfirmRequested,
            OverlayInput::CancelRequested,
            OverlayInput::DisplayConfigurationInvalidated { reason: "test" },
            OverlayInput::DoubleClicked {
                point: point(20, 20),
            },
        ];
        for input in inputs {
            let mut model = region_model();
            model.selection = Some(Rect {
                x: 10,
                y: 10,
                width: 50,
                height: 50,
            });
            model.selection_kind = Some(SelectionKind::Region);
            let effects = transition(&mut model, input);
            if let Some(position) = effects
                .iter()
                .position(|effect| matches!(effect, OverlayEffect::Close(_)))
            {
                assert_eq!(position, effects.len() - 1, "input {input:?}");
            }
        }
    }
}
