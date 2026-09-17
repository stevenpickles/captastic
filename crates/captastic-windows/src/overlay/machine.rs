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
use super::snap::{
    guides_for, snap_coordinate, snap_point, snap_translation, ActiveSnaps, EdgeMask, RectEdges,
    SnapAxis, SnapHit, SnapTargets, NO_SNAPS, SNAP_THRESHOLD_DIP,
};
use super::{NativeWindowHandle, SelectionKind, WindowCandidate};

pub(super) const DRAG_THRESHOLD: i32 = 4;
pub(super) const MIN_REGION_SIZE: i64 = 8;
/// One arrow press moves one physical pixel — the whole point of the key being there at all, and
/// deliberately not DPI-scaled: the badge counts physical pixels and so must this.
pub(super) const NUDGE_STEP: i32 = 1;
/// With Shift held, ten. Enough to cross a window border in a couple of presses without losing
/// the ability to land on an exact pixel by letting go of Shift.
pub(super) const NUDGE_COARSE_STEP: i32 = 10;

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
    /// The window-chooser candidate under the pointer. The shell owns the thumbnail
    /// inventory and supplies hit results through inputs; the machine owns the decision.
    pub(super) hovered: Option<WindowCandidate>,
    /// Whether the region tool pulls edges onto nearby window, work-area and display edges.
    pub(super) snap_to_windows: bool,
    /// The edge inventory to snap against, once the shell has built it.
    ///
    /// Built at most once per run and deliberately never refreshed: enumerating windows is the
    /// expensive part of opening the chooser, and doing it again on every pointer move to catch a
    /// window that moved while the overlay was up would cost far more than the staleness is worth.
    /// `None` means "not asked for yet", which is why it is not simply an empty inventory.
    pub(super) snap_targets: Option<SnapTargets>,
    /// The guide lines the current selection's snapped edges justify, at most one per axis.
    /// Purely a consequence of the last adjustment: cleared on release, on a stolen capture, and
    /// on any tool change, because a line the selection is no longer sitting on is a lie.
    pub(super) active_snaps: ActiveSnaps,
}

/// A translated window message. The shell decodes `LPARAM`/`WPARAM`/message identity; the
/// machine never sees Win32 encodings.
///
/// `Clone` but not `Copy`: [`OverlayInput::SnapTargetsReady`] carries an owned inventory, which is
/// the point — the shell enumerates windows inside an effect handler and hands the result back as
/// a value, so the machine still never performs a query of its own.
#[derive(Clone, Debug)]
pub(super) enum OverlayInput {
    /// Pointer moved to a screen-space point. Under the Window tool the shell resolves the
    /// thumbnail under the pointer (it owns the inventory) and passes the candidate along.
    PointerMoved {
        point: POINT,
        window_hover: Option<WindowCandidate>,
        modifiers: Modifiers,
    },
    /// Primary button pressed. Under the Window tool the shell resolves which thumbnail slot
    /// (if any) the press landed on.
    ///
    /// Deliberately carries no modifiers: a press commits no geometry, so there is nothing for
    /// Ctrl to suppress yet. The anchor it records is snapped later, when the press becomes a
    /// drag, using that move's modifier state.
    PointerDown {
        point: POINT,
        window_slot: Option<NativeWindowHandle>,
    },
    /// Primary button released. Fully modeled for every tool.
    ///
    /// Carries the modifiers too, because the release recomputes the committed geometry from the
    /// button-up point: without them a drag made with Ctrl held would snap at the last instant,
    /// and the rectangle the user let go of would not be the one they got.
    PointerUp { point: POINT, modifiers: Modifiers },
    /// Primary button double-clicked anywhere.
    DoubleClicked { point: POINT },
    /// Pointer capture was taken by another window. Self-initiated releases are consumed by the
    /// shell's protocol flag and never reach the machine.
    PointerCaptureLost,
    /// The shell finished a requested window-preview update: `rect` is the captured frame's
    /// source rectangle when a ready preview now backs the selected window, `None` otherwise.
    WindowPreviewResolved { rect: Option<Rect> },
    /// The shell finished the enumeration requested by [`OverlayEffect::BuildSnapTargets`].
    SnapTargetsReady { targets: SnapTargets },
    /// An arrow key adjusted the region by `(dx, dy)` physical pixels. `resize` moves the right
    /// and bottom edges instead of the whole rectangle.
    ///
    /// The shell has already folded the modifier state into these numbers, so the machine never
    /// has to know which key was pressed — only what the user asked the region to do.
    Nudge { dx: i32, dy: i32, resize: bool },
    /// Enter pressed.
    ConfirmRequested,
    /// Escape, right-click, or an external close request.
    CancelRequested,
    /// The display configuration changed under the overlay; its geometry is no longer current.
    DisplayConfigurationInvalidated { reason: &'static str },
}

/// Keyboard modifier state sampled by the shell at the moment a pointer message was posted.
///
/// Only Ctrl matters to the pointer path, and it means one thing: place this edge exactly where I
/// am pointing. It is read through `GetKeyState` inside the window procedure rather than kept in
/// the model, because the machine owns no clock and no input device.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Modifiers {
    /// Ctrl held. On the pointer path: suppress snapping entirely for this message and clear any
    /// guide. On the arrow keys: adjust the region's size rather than its position.
    pub(super) ctrl: bool,
    /// Shift held: take the coarse nudge step instead of one pixel. Means nothing to the pointer.
    pub(super) shift: bool,
}

impl Modifiers {
    /// How far one arrow press moves the region.
    pub(super) const fn nudge_step(self) -> i32 {
        if self.shift {
            NUDGE_COARSE_STEP
        } else {
            NUDGE_STEP
        }
    }
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
    /// Clear the dimension-label placement hysteresis (render-scratch state).
    ClearDimensionLabel,
    /// Clear the captured frame backing a window selection (shell-owned pixels).
    ClearSelectedWindowFrame,
    /// Re-register live thumbnails and rebuild the window-overview cache.
    RefreshWindowChrome,
    /// Unregister every live DWM thumbnail so frozen fallbacks paint instead. Used while the
    /// toolbar is being dragged, because compositor-managed previews can cover overlay pixels.
    ClearLiveThumbnails,
    /// Hide (but keep registered) every live DWM thumbnail; leaving the Window tool.
    HideLiveThumbnails,
    /// Rebuild the composed window-overview surface from current thumbnails and chrome.
    RebuildOverviewCache,
    /// Enumerate windows and start the incremental thumbnail build for the chooser. The shell
    /// renders batch by batch through posted messages so the pump stays live; the machine only
    /// decides when a build is wanted.
    BuildWindowOverview,
    /// Enumerate windows and feed their visible frames back as
    /// [`OverlayInput::SnapTargetsReady`]. Emitted once per run, the first time the region tool
    /// could need an edge to snap to, so a run that never draws a region never pays for it.
    BuildSnapTargets,
    /// Capture (or retry) the preview for the given window and feed the outcome back as
    /// [`OverlayInput::WindowPreviewResolved`]. Synchronous and blocking in the shell today
    /// (M21); the non-sticky retry semantics live in the shell's preview cache.
    UpdateWindowPreview {
        window: Option<NativeWindowHandle>,
    },
    /// Persist the toolbar's resting position for this display.
    PersistToolbarCenter {
        position: POINT,
    },
    /// Persist the latest interaction (tool + region) for this display.
    PersistInteraction {
        region: Option<Rect>,
    },
    /// Persist the snap-to-window-edges preference. Global, not per display.
    PersistSnapPreference {
        enabled: bool,
    },
    /// Destroy the overlay window with the given outcome. Always the final effect.
    Close(CloseOutcome),
}

/// Advances the model by one input and returns the effects the shell must apply, in order.
pub(super) fn transition(model: &mut OverlayModel, input: OverlayInput) -> Vec<OverlayEffect> {
    match input {
        OverlayInput::PointerMoved {
            point,
            window_hover,
            modifiers,
        } => pointer_moved(model, point, window_hover, modifiers),
        OverlayInput::PointerDown { point, window_slot } => pointer_down(model, point, window_slot),
        OverlayInput::WindowPreviewResolved { rect } => window_preview_resolved(model, rect),
        OverlayInput::SnapTargetsReady { targets } => {
            model.snap_targets = Some(targets);
            // Nothing visible changed: the inventory is requested before the first pointer move
            // that could use it, and that move repaints on its own.
            Vec::new()
        }
        OverlayInput::PointerUp { point, modifiers } => pointer_up(model, point, modifiers),
        OverlayInput::DoubleClicked { point } => double_clicked(model, point),
        OverlayInput::PointerCaptureLost => pointer_capture_lost(model),
        OverlayInput::Nudge { dx, dy, resize } => nudge(model, dx, dy, resize),
        OverlayInput::ConfirmRequested => confirm(model),
        OverlayInput::CancelRequested => cancel(model),
        OverlayInput::DisplayConfigurationInvalidated { reason } => {
            vec![OverlayEffect::Close(
                CloseOutcome::DisplayConfigurationInvalidated { reason },
            )]
        }
    }
}

fn pointer_moved(
    model: &mut OverlayModel,
    point: POINT,
    window_hover: Option<WindowCandidate>,
    modifiers: Modifiers,
) -> Vec<OverlayEffect> {
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
        model.hovered = None;
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
        model.hovered = None;
        effects.push(OverlayEffect::SetCursor(CursorIntent::Arrow));
    } else if let Some(resize) = model.resizing {
        let (rect, snaps) = resize_region_snapped(model, resize, point, modifiers);
        model.selection = Some(rect);
        model.active_snaps = snaps;
        model.hovered_handle = Some(resize.handle);
        effects.push(OverlayEffect::SetCursor(CursorIntent::Resize(
            resize.handle,
        )));
    } else if let Some(moving) = model.moving_region {
        let (rect, snaps) = move_region_snapped(model, moving, point, modifiers);
        model.selection = Some(rect);
        model.active_snaps = snaps;
        model.hovered_handle = None;
        effects.push(OverlayEffect::SetCursor(CursorIntent::Move));
    } else if let Some(anchor) = model.anchor {
        let latched = (point.x - anchor.x).abs() >= DRAG_THRESHOLD
            || (point.y - anchor.y).abs() >= DRAG_THRESHOLD;
        if latched && !model.dragging {
            // The anchor is snapped exactly once, here, at the moment the press becomes a drag.
            // Snapping it at button-down instead would bias the drag threshold: the snap radius
            // is wider than DRAG_THRESHOLD, so a press beside a window edge would have counted as
            // a drag before the pointer had moved at all.
            model.anchor = Some(snap_anchor(model, anchor, modifiers));
            model.dragging = true;
        }
        if model.dragging {
            let anchor = model.anchor.unwrap_or(anchor);
            let (rect, snaps) = rect_from_points_snapped(model, anchor, point, modifiers);
            model.selection = rect;
            model.active_snaps = snaps;
            model.selection_kind = Some(SelectionKind::Region);
            model.selected_window = None;
            model.hovered = None;
        }
        model.hovered_handle = None;
        effects.push(OverlayEffect::SetCursor(CursorIntent::Crosshair));
    } else if model.tool == CaptureTool::Region
        && model.selection_kind == Some(SelectionKind::Region)
    {
        model.hovered_handle = model.selection.and_then(|selection| {
            hit_test_resize_handle(selection, point, model.display_environment.metrics)
        });
        model.hovered = None;
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
    } else if model.tool == CaptureTool::Window {
        model.hovered = window_hover;
        model.hovered_handle = None;
        effects.push(OverlayEffect::SetCursor(CursorIntent::Arrow));
    } else {
        model.hovered = None;
        model.hovered_handle = None;
        if model.tool == CaptureTool::Region {
            effects.push(OverlayEffect::SetCursor(CursorIntent::Crosshair));
        } else {
            effects.push(OverlayEffect::SetCursor(CursorIntent::Arrow));
        }
    }
    // Window-tool hover repaints are the shell's policy: it skips the repaint entirely when
    // neither the hovered slot nor the hovered control changed, and otherwise forces a
    // synchronous paint. Every other tool repaints on each move, as before.
    if model.tool != CaptureTool::Window {
        effects.push(OverlayEffect::Invalidate);
    }
    effects
}

fn pointer_down(
    model: &mut OverlayModel,
    point: POINT,
    window_slot: Option<NativeWindowHandle>,
) -> Vec<OverlayEffect> {
    let local = local_point(model.source, point);
    let layout = ToolbarLayout::new(model.display_environment, model.toolbar_position);
    if let Some(control) = layout.hit_test(local, model.options_open) {
        return toolbar_control_pressed(model, control, layout.bounds.contains(local), local);
    }
    // Any press outside the toolbar starts a new adjustment; whatever the last one was snapped to
    // is no longer being maintained.
    model.active_snaps = NO_SNAPS;
    model.hovered_control = None;
    let options_were_open = model.options_open;
    model.options_open = false;
    match model.tool {
        CaptureTool::Window => {
            let mut effects = Vec::new();
            if options_were_open {
                effects.push(OverlayEffect::RefreshWindowChrome);
            }
            model.selected_window = window_slot;
            effects.push(OverlayEffect::UpdateWindowPreview {
                window: window_slot,
            });
            return effects;
        }
        CaptureTool::FullDisplay => return vec![OverlayEffect::Invalidate],
        CaptureTool::Region => {}
    }
    // The press that starts any region adjustment is the last moment the inventory can be built
    // without the user waiting on it mid-drag, and the first moment it is certainly wanted.
    let mut effects: Vec<OverlayEffect> = request_snap_targets(model).into_iter().collect();
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
        effects.extend([
            OverlayEffect::SetCursor(CursorIntent::Resize(handle)),
            OverlayEffect::CapturePointer,
            OverlayEffect::Invalidate,
        ]);
    } else if existing_region.is_some_and(|selection| contains(selection, point)) {
        model.moving_region = existing_region.map(|original| MoveDrag {
            original,
            pointer_origin: point,
        });
        model.anchor = None;
        model.dragging = false;
        model.hovered_handle = None;
        effects.extend([
            OverlayEffect::SetCursor(CursorIntent::Move),
            OverlayEffect::CapturePointer,
            OverlayEffect::Invalidate,
        ]);
    } else {
        model.anchor = Some(point);
        model.dragging = false;
        model.resizing = None;
        model.moving_region = None;
        model.selection = None;
        model.selection_kind = None;
        model.selected_window = None;
        model.hovered_handle = None;
        model.hovered = None;
        effects.extend([
            OverlayEffect::ClearDimensionLabel,
            OverlayEffect::SetCursor(CursorIntent::Crosshair),
            OverlayEffect::CapturePointer,
            OverlayEffect::Invalidate,
        ]);
    }
    effects
}

/// Asks the shell for the snap inventory the first time the region tool could use one.
///
/// Guarded on all three conditions rather than only on the tool: enumeration is the expensive part
/// of opening the chooser, and a run with snapping switched off, or one that never leaves the
/// full-display tool, must not pay for it. Once built it is never rebuilt — see
/// [`OverlayModel::snap_targets`].
fn request_snap_targets(model: &OverlayModel) -> Option<OverlayEffect> {
    (model.snap_to_windows && model.tool == CaptureTool::Region && model.snap_targets.is_none())
        .then_some(OverlayEffect::BuildSnapTargets)
}

fn toolbar_control_pressed(
    model: &mut OverlayModel,
    control: ToolbarControl,
    in_toolbar_bounds: bool,
    local: POINT,
) -> Vec<OverlayEffect> {
    model.anchor = None;
    model.dragging = false;
    model.resizing = None;
    model.moving_region = None;
    model.hovered_control = Some(control);
    let mut effects = Vec::new();
    match control {
        ToolbarControl::Background if in_toolbar_bounds => {
            model.options_open = false;
            if model.tool == CaptureTool::Window {
                // DWM thumbnails are compositor-managed and can cover overlay pixels. Use frozen
                // fallbacks while the toolbar is moving, then register only previews that do not
                // overlap its final position on button-up.
                effects.push(OverlayEffect::ClearLiveThumbnails);
                effects.push(OverlayEffect::RebuildOverviewCache);
            }
            model.toolbar_drag = Some(ToolbarDrag {
                pointer_offset: POINT {
                    x: local.x.saturating_sub(model.toolbar_position.x),
                    y: local.y.saturating_sub(model.toolbar_position.y),
                },
            });
            effects.push(OverlayEffect::CapturePointer);
        }
        ToolbarControl::Background => {}
        ToolbarControl::FullDisplay => {
            effects.extend(activate_tool(model, CaptureTool::FullDisplay));
        }
        ToolbarControl::Window => effects.extend(activate_tool(model, CaptureTool::Window)),
        ToolbarControl::Region => effects.extend(activate_tool(model, CaptureTool::Region)),
        ToolbarControl::Options => {
            model.options_open = !model.options_open;
            if model.tool == CaptureTool::Window {
                effects.push(OverlayEffect::RefreshWindowChrome);
            }
        }
        // Capture and Cancel end the run without touching the cursor or repainting: the window
        // is being destroyed (or, for a selection-less capture press, nothing changed).
        ToolbarControl::Capture => return confirm(model),
        ToolbarControl::DimBackground => {
            model.dim_background = !model.dim_background;
        }
        ToolbarControl::SnapToWindows => {
            model.snap_to_windows = !model.snap_to_windows;
            // Switching it off mid-run leaves a guide on screen describing a rule that no longer
            // applies; switching it on does not retro-snap the current selection.
            model.active_snaps = NO_SNAPS;
            effects.extend(request_snap_targets(model));
            effects.push(OverlayEffect::PersistSnapPreference {
                enabled: model.snap_to_windows,
            });
        }
        ToolbarControl::ClipboardDestination => {}
        ToolbarControl::Cancel => return cancel(model),
    }
    effects.push(OverlayEffect::SetCursor(CursorIntent::Arrow));
    effects.push(OverlayEffect::Invalidate);
    effects
}

/// Reacts to the shell pruning destroyed source windows from the chooser inventory: product
/// state pointing at a dead handle is dropped, so a vanished window can neither stay hovered
/// nor remain the selection target. Alive windows are untouched, whatever the state of their
/// live thumbnails.
pub(super) fn window_sources_pruned(
    model: &mut OverlayModel,
    dead: &[NativeWindowHandle],
) -> Vec<OverlayEffect> {
    if model
        .hovered
        .is_some_and(|candidate| dead.contains(&candidate.handle))
    {
        model.hovered = None;
    }
    let mut effects = Vec::new();
    if model
        .selected_window
        .is_some_and(|handle| dead.contains(&handle))
    {
        model.selected_window = None;
        effects.push(OverlayEffect::ClearSelectedWindowFrame);
    }
    effects
}

/// Switches the active tool, restoring or resetting the selection to match, and returns the
/// window-chooser side work the shell must run for the transition.
pub(super) fn activate_tool(model: &mut OverlayModel, tool: CaptureTool) -> Vec<OverlayEffect> {
    model.anchor = None;
    model.dragging = false;
    model.resizing = None;
    model.moving_region = None;
    model.active_snaps = NO_SNAPS;
    let tool_changed = model.tool != tool;
    let mut effects = Vec::new();
    if tool_changed {
        model.last_region = latest_interaction_region(
            model.tool,
            model.selection,
            model.selection_kind,
            model.last_region,
        );
        model.selection = None;
        model.selection_kind = None;
        model.selected_window = None;
        model.hovered_handle = None;
        model.hovered = None;
        effects.push(OverlayEffect::ClearDimensionLabel);
        effects.push(OverlayEffect::ClearSelectedWindowFrame);
    }
    model.tool = tool;
    model.options_open = false;
    match tool {
        CaptureTool::FullDisplay => {
            model.selection = Some(model.source);
            model.selection_kind = Some(SelectionKind::Display);
            model.selected_window = None;
            effects.push(OverlayEffect::ClearSelectedWindowFrame);
        }
        CaptureTool::Window => effects.push(OverlayEffect::BuildWindowOverview),
        CaptureTool::Region if tool_changed => {
            (model.selection, model.selection_kind) =
                initial_selection(CaptureTool::Region, model.last_region, model.source);
        }
        CaptureTool::Region => {}
    }
    effects.extend(request_snap_targets(model));
    if tool != CaptureTool::Window {
        effects.push(OverlayEffect::HideLiveThumbnails);
    }
    effects
}

fn pointer_up(model: &mut OverlayModel, point: POINT, modifiers: Modifiers) -> Vec<OverlayEffect> {
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
    // The release recomputes the geometry from the button-up point with the same snapping the
    // last move applied, so what is committed is what was on screen. The guides then go: the
    // pointer has come to rest and there is no adjustment left for them to explain.
    if let Some(resize) = model.resizing.take() {
        let (rect, _) = resize_region_snapped(model, resize, point, modifiers);
        model.selection = Some(rect);
        model.selection_kind = Some(SelectionKind::Region);
        model.selected_window = None;
    } else if let Some(moving) = model.moving_region.take() {
        let (rect, _) = move_region_snapped(model, moving, point, modifiers);
        model.selection = Some(rect);
        model.selection_kind = Some(SelectionKind::Region);
        model.selected_window = None;
    } else if let Some(anchor) = model.anchor.take() {
        if model.dragging {
            let (rect, _) = rect_from_points_snapped(model, anchor, point, modifiers);
            model.selection = rect;
            model.selection_kind = Some(SelectionKind::Region);
            model.selected_window = None;
        }
    }
    model.active_snaps = NO_SNAPS;
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

/// Adjusts the region by an exact number of physical pixels from the keyboard.
///
/// The only way to place an edge on a pixel the mouse cannot reach — a trackpad that skips, a
/// high-DPI display where one mouse count is two pixels, or simply a hand that will not hold
/// still. Deliberately never snaps: the user typing an exact number of pixels has already said
/// where they want the edge, and a rule that pulled it somewhere else would make the keys useless
/// for the one job they exist for. It clears any guide for the same reason.
///
/// Inert unless the region tool has a region and no drag is in flight. A drag owns the geometry
/// while it runs, and a key arriving mid-drag would move a rectangle the next mouse message is
/// about to recompute from its own anchor anyway.
fn nudge(model: &mut OverlayModel, dx: i32, dy: i32, resize: bool) -> Vec<OverlayEffect> {
    if model.tool != CaptureTool::Region
        || model.selection_kind != Some(SelectionKind::Region)
        || model.anchor.is_some()
        || model.resizing.is_some()
        || model.moving_region.is_some()
    {
        return Vec::new();
    }
    let Some(region) = model.selection else {
        return Vec::new();
    };
    let adjusted = if resize {
        resize_region_by(region, dx, dy, model.source)
    } else {
        region.translated(i64::from(dx), i64::from(dy))
    }
    .clamp_within(model.source);
    let had_guides = model.active_snaps != NO_SNAPS;
    model.active_snaps = NO_SNAPS;
    if adjusted == region && !had_guides {
        // Held against the edge of the display: autorepeat keeps delivering, and repainting an
        // unchanged overlay sixty times a second for it would be pure waste.
        return Vec::new();
    }
    model.selection = Some(adjusted);
    // No persistence effect: confirming and cancelling both persist the latest region already,
    // and those are the only two ways this overlay ends.
    vec![OverlayEffect::Invalidate]
}

/// Moves the region's right and bottom edges by `(dx, dy)`, never below the minimum size and
/// never past the display.
fn resize_region_by(region: Rect, dx: i32, dy: i32, source: Rect) -> Rect {
    let left = i64::from(region.x);
    let top = i64::from(region.y);
    // The floors are themselves capped at the display, exactly as `resize_region` does it: a
    // region already pressed against the right edge has no room for the minimum and the clamp
    // range would otherwise be inverted.
    let minimum_right = (left + MIN_REGION_SIZE).min(source.right());
    let minimum_bottom = (top + MIN_REGION_SIZE).min(source.bottom());
    let right = (region.right() + i64::from(dx)).clamp(minimum_right, source.right());
    let bottom = (region.bottom() + i64::from(dy)).clamp(minimum_bottom, source.bottom());
    Rect::from_edges(left, top, right, bottom).unwrap_or(region)
}

fn window_preview_resolved(model: &mut OverlayModel, rect: Option<Rect>) -> Vec<OverlayEffect> {
    model.selection = rect;
    model.selection_kind = rect.map(|_| SelectionKind::Window);
    if model.selection.is_some() {
        // A ready preview confirms immediately; the shell attaches the captured frame.
        confirm(model)
    } else {
        // Not ready (or empty space): stay in the chooser. The preview cache deliberately does
        // not latch failures, so the next click retries.
        vec![
            OverlayEffect::RebuildOverviewCache,
            OverlayEffect::Invalidate,
        ]
    }
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
    model.active_snaps = NO_SNAPS;
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

pub(super) fn initial_selection(
    tool: CaptureTool,
    last_region: Option<Rect>,
    source: Rect,
) -> (Option<Rect>, Option<SelectionKind>) {
    match tool {
        CaptureTool::FullDisplay => (Some(source), Some(SelectionKind::Display)),
        CaptureTool::Window => (None, None),
        CaptureTool::Region => {
            let region = last_region
                .map(|region| fit_region_to_source(region, source))
                .unwrap_or_else(|| default_region_for_source(source));
            (Some(region), Some(SelectionKind::Region))
        }
    }
}

pub(super) fn default_region_for_source(source: Rect) -> Rect {
    let width = (source.width / 2).max(1);
    let height = (source.height / 2).max(1);
    Rect {
        x: source
            .x
            .saturating_add(((source.width - width) / 2).min(i32::MAX as u32) as i32),
        y: source
            .y
            .saturating_add(((source.height - height) / 2).min(i32::MAX as u32) as i32),
        width,
        height,
    }
}

pub(super) fn fit_region_to_source(region: Rect, source: Rect) -> Rect {
    region.clamp_within(source)
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

/// The snap radius in physical pixels for the display this overlay covers.
fn snap_threshold(model: &OverlayModel) -> i64 {
    i64::from(model.display_environment.metrics.px(SNAP_THRESHOLD_DIP))
}

/// The inventory to snap against for this message, or `None` when nothing should snap.
///
/// Three ways to get nothing: the option is off, Ctrl is held (place the edge exactly here), or
/// the shell has not answered the enumeration request yet — which is the normal state for the
/// first few messages of a run and must behave exactly like an empty desktop rather than stall.
fn active_targets(model: &OverlayModel, modifiers: Modifiers) -> Option<&SnapTargets> {
    (model.snap_to_windows && !modifiers.ctrl)
        .then_some(model.snap_targets.as_ref())
        .flatten()
}

fn as_point(x: i64, y: i64) -> POINT {
    POINT {
        x: i32::try_from(x).unwrap_or(if x < 0 { i32::MIN } else { i32::MAX }),
        y: i32::try_from(y).unwrap_or(if y < 0 { i32::MIN } else { i32::MAX }),
    }
}

/// The anchor corner of a new region, pulled onto a nearby edge once as the drag latches.
fn snap_anchor(model: &OverlayModel, anchor: POINT, modifiers: Modifiers) -> POINT {
    let Some(targets) = active_targets(model, modifiers) else {
        return anchor;
    };
    let ((x, y), _) = snap_point(
        i64::from(anchor.x),
        i64::from(anchor.y),
        targets,
        snap_threshold(model),
    );
    as_point(x, y)
}

/// A rubber-band drag's rectangle with its free corner snapped, and the guides that justifies.
pub(super) fn rect_from_points_snapped(
    model: &OverlayModel,
    anchor: POINT,
    pointer: POINT,
    modifiers: Modifiers,
) -> (Option<Rect>, ActiveSnaps) {
    let Some(targets) = active_targets(model, modifiers) else {
        return (rect_from_points(model.source, anchor, pointer), NO_SNAPS);
    };
    let ((x, y), hits) = snap_point(
        i64::from(pointer.x),
        i64::from(pointer.y),
        targets,
        snap_threshold(model),
    );
    // Snap first, clamp second: `rect_from_points` is what keeps the band inside the display, and
    // an edge it pulls back in is an edge no longer sitting on a target, which `guides_for` then
    // refuses to draw a line for.
    let rect = rect_from_points(model.source, anchor, as_point(x, y));
    (rect, rect.map_or(NO_SNAPS, |rect| guides_for(rect, hits)))
}

/// A resize with the handle's own edges snapped, and the guides that justifies.
///
/// The pointer is snapped on the axes the handle moves and then fed through the ordinary
/// [`resize_region`], so the display clamp and `MIN_REGION_SIZE` still have the last word — and a
/// guide whose edge those rules pulled back is dropped.
pub(super) fn resize_region_snapped(
    model: &OverlayModel,
    resize: ResizeDrag,
    pointer: POINT,
    modifiers: Modifiers,
) -> (Rect, ActiveSnaps) {
    let Some(targets) = active_targets(model, modifiers) else {
        return (
            resize_region(resize.original, resize.handle, pointer, model.source),
            NO_SNAPS,
        );
    };
    let threshold = snap_threshold(model);
    let mask = EdgeMask::for_handle(resize.handle);
    let mut snapped = [i64::from(pointer.x), i64::from(pointer.y)];
    let mut hits: [Option<SnapHit>; 2] = [None, None];
    for axis in [SnapAxis::X, SnapAxis::Y] {
        if !mask.covers(axis) {
            continue;
        }
        if let Some(hit) = snap_coordinate(snapped[axis.index()], targets, axis, threshold) {
            snapped[axis.index()] = hit.position;
            hits[axis.index()] = Some(hit);
        }
    }
    let rect = resize_region(
        resize.original,
        resize.handle,
        as_point(snapped[SnapAxis::X.index()], snapped[SnapAxis::Y.index()]),
        model.source,
    );
    (rect, guides_for(rect, hits))
}

/// A move with whichever side of the region is nearest a target pulled onto it, size preserved.
pub(super) fn move_region_snapped(
    model: &OverlayModel,
    moving: MoveDrag,
    pointer: POINT,
    modifiers: Modifiers,
) -> (Rect, ActiveSnaps) {
    let moved = move_region(
        moving.original,
        moving.pointer_origin,
        pointer,
        model.source,
    );
    let Some(targets) = active_targets(model, modifiers) else {
        return (moved, NO_SNAPS);
    };
    let translations = snap_translation(RectEdges::of(moved), targets, snap_threshold(model));
    let snapped = moved
        .translated(
            translations[SnapAxis::X.index()].map_or(0, |(delta, _)| delta),
            translations[SnapAxis::Y.index()].map_or(0, |(delta, _)| delta),
        )
        // A move never resizes, so the region is pushed back inside the display rather than
        // trimmed by it — which can undo the snap, hence the guide check below.
        .clamp_within(model.source);
    let hits = [
        translations[SnapAxis::X.index()].map(|(_, hit)| hit),
        translations[SnapAxis::Y.index()].map(|(_, hit)| hit),
    ];
    (snapped, guides_for(snapped, hits))
}

#[cfg(test)]
mod tests {
    use super::super::layout::UiRect;
    use super::super::snap::{SnapGuide, SnapTarget, SnapTargetKind};
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
            hovered: None,
            snap_to_windows: true,
            snap_targets: Some(SnapTargets::default()),
            active_snaps: NO_SNAPS,
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
                window_slot: None,
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
                window_hover: None,
                modifiers: Modifiers::default(),
            },
        );
        assert!(!model.dragging);
        assert_eq!(model.selection, None);

        // Crossing the threshold starts the rubber band.
        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(104, 100),
                window_hover: None,
                modifiers: Modifiers::default(),
            },
        );
        assert!(model.dragging);
        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(300, 240),
                window_hover: None,
                modifiers: Modifiers::default(),
            },
        );
        assert_eq!(model.selection_kind, Some(SelectionKind::Region));

        let effects = transition(
            &mut model,
            OverlayInput::PointerUp {
                point: point(300, 240),
                modifiers: Modifiers::default(),
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
                window_slot: None,
            },
        );
        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(102, 101),
                window_hover: None,
                modifiers: Modifiers::default(),
            },
        );
        let effects = transition(
            &mut model,
            OverlayInput::PointerUp {
                point: point(102, 101),
                modifiers: Modifiers::default(),
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
                window_slot: None,
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
                window_hover: None,
                modifiers: Modifiers::default(),
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
                modifiers: Modifiers::default(),
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
                window_slot: None,
            },
        );
        assert!(model.moving_region.is_some());

        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(230, 200),
                window_hover: None,
                modifiers: Modifiers::default(),
            },
        );
        transition(
            &mut model,
            OverlayInput::PointerUp {
                point: point(230, 200),
                modifiers: Modifiers::default(),
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
                window_slot: None,
            },
        );
        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(200, 200),
                window_hover: None,
                modifiers: Modifiers::default(),
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
                window_slot: None,
            },
        );
        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(90, 60),
                window_hover: None,
                modifiers: Modifiers::default(),
            },
        );
        let effects = transition(
            &mut model,
            OverlayInput::PointerUp {
                point: point(90, 60),
                modifiers: Modifiers::default(),
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
    fn a_display_change_names_its_reason_so_the_shell_can_report_the_loss() {
        // The reason is not decoration. It is the only thing separating this close from the
        // Cancelled one above, and the shell carries the distinction out to the log line and the
        // notification balloon that tell the user their press is gone. Every reason the window
        // procedure maps - a DPI change, a monitor change, a work-area change - arrives here.
        for reason in [
            "overlay_dpi_changed",
            crate::DISPLAY_CHANGE_MONITORS,
            "overlay_display_setting_changed",
        ] {
            let mut model = region_model();
            let effects = transition(
                &mut model,
                OverlayInput::DisplayConfigurationInvalidated { reason },
            );
            assert!(
                matches!(
                    close_outcome(&effects),
                    Some(CloseOutcome::DisplayConfigurationInvalidated { reason: closed })
                        if closed == reason
                ),
                "{reason}"
            );
        }
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
                window_hover: None,
                modifiers: Modifiers::default(),
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
                modifiers: Modifiers::default(),
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
                    modifiers: Modifiers::default(),
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

    fn center(rect: super::super::layout::UiRect) -> POINT {
        POINT {
            x: (rect.left + rect.right) / 2,
            y: (rect.top + rect.bottom) / 2,
        }
    }

    fn layout_for(model: &OverlayModel) -> ToolbarLayout {
        ToolbarLayout::new(model.display_environment, model.toolbar_position)
    }

    #[test]
    fn every_toolbar_control_dispatches_with_drags_cleared() {
        // (control point, expected hovered_control, closes) driven through the real hit test so
        // the dispatch table stays honest against layout changes.
        let model = region_model();
        let layout = layout_for(&model);
        let cases = [
            (
                center(layout.drag_handle),
                ToolbarControl::Background,
                false,
            ),
            (
                center(layout.full_display),
                ToolbarControl::FullDisplay,
                false,
            ),
            (center(layout.window), ToolbarControl::Window, false),
            (center(layout.region), ToolbarControl::Region, false),
            (center(layout.options), ToolbarControl::Options, false),
            (center(layout.capture), ToolbarControl::Capture, false),
            (
                center(layout.dim_background),
                ToolbarControl::DimBackground,
                false,
            ),
            (
                center(layout.snap_to_windows),
                ToolbarControl::SnapToWindows,
                false,
            ),
            (
                center(layout.clipboard_destination),
                ToolbarControl::ClipboardDestination,
                false,
            ),
            (center(layout.cancel), ToolbarControl::Cancel, true),
        ];
        for (local, expected, closes) in cases {
            let mut model = region_model();
            // The menu rows only hit while the options menu is open.
            model.options_open = matches!(
                expected,
                ToolbarControl::DimBackground
                    | ToolbarControl::SnapToWindows
                    | ToolbarControl::ClipboardDestination
                    | ToolbarControl::Cancel
            );
            // Give the machine an active drag to prove every dispatch clears it.
            model.anchor = Some(point(1, 1));
            model.dragging = true;
            let screen = POINT {
                x: model.source.x + local.x,
                y: model.source.y + local.y,
            };
            let effects = transition(
                &mut model,
                OverlayInput::PointerDown {
                    point: screen,
                    window_slot: None,
                },
            );
            assert_eq!(model.hovered_control, Some(expected));
            assert_eq!(model.anchor, None, "{expected:?}");
            assert!(!model.dragging, "{expected:?}");
            assert_eq!(close_outcome(&effects).is_some(), closes, "{expected:?}");
        }
    }

    #[test]
    fn the_tool_switch_matrix_restores_and_resets_selections() {
        let tools = [
            CaptureTool::FullDisplay,
            CaptureTool::Window,
            CaptureTool::Region,
        ];
        let region = Rect {
            x: 100,
            y: 100,
            width: 300,
            height: 200,
        };
        for from in tools {
            for to in tools {
                let mut model = region_model();
                model.tool = from;
                let (selection, kind) = initial_selection(from, Some(region), model.source);
                model.selection = selection;
                model.selection_kind = kind;
                model.last_region = Some(region);
                model.options_open = true;
                model.hovered = Some(candidate(0x42));

                let effects = activate_tool(&mut model, to);
                let changed = from != to;

                assert_eq!(model.tool, to, "{from:?}->{to:?}");
                assert!(!model.options_open, "{from:?}->{to:?}");
                match to {
                    CaptureTool::FullDisplay => {
                        assert_eq!(model.selection, Some(model.source));
                        assert_eq!(model.selection_kind, Some(SelectionKind::Display));
                        assert!(has_effect(&effects, |e| matches!(
                            e,
                            OverlayEffect::ClearSelectedWindowFrame
                        )));
                    }
                    CaptureTool::Window => {
                        if changed {
                            assert_eq!(model.selection, None);
                            assert_eq!(model.selection_kind, None);
                        }
                        assert!(has_effect(&effects, |e| matches!(
                            e,
                            OverlayEffect::BuildWindowOverview
                        )));
                        assert!(!has_effect(&effects, |e| matches!(
                            e,
                            OverlayEffect::HideLiveThumbnails
                        )));
                    }
                    CaptureTool::Region => {
                        // Region restores the latest interaction region on a switch and leaves
                        // an existing selection alone on re-activation.
                        assert_eq!(model.selection, Some(region), "{from:?}->{to:?}");
                        assert_eq!(model.selection_kind, Some(SelectionKind::Region));
                    }
                }
                if to != CaptureTool::Window {
                    assert!(has_effect(&effects, |e| matches!(
                        e,
                        OverlayEffect::HideLiveThumbnails
                    )));
                }
                // Leaving a region selection behind records it for the next region activation.
                if changed && from == CaptureTool::Region {
                    assert_eq!(model.last_region, Some(region), "{from:?}->{to:?}");
                }
                assert_eq!(model.selected_window, None);
                // The hover highlight only survives a same-tool re-activation.
                assert_eq!(model.hovered.is_none(), changed, "{from:?}->{to:?}");
            }
        }
    }

    #[test]
    fn a_toolbar_drag_start_clears_live_thumbnails_only_under_the_window_tool() {
        for (tool, expects_clear) in [(CaptureTool::Window, true), (CaptureTool::Region, false)] {
            let mut model = region_model();
            model.tool = tool;
            let layout = layout_for(&model);
            let local = center(layout.drag_handle);
            let effects = transition(
                &mut model,
                OverlayInput::PointerDown {
                    point: local,
                    window_slot: None,
                },
            );
            let drag = model.toolbar_drag.expect("drag-handle press starts a drag");
            assert_eq!(drag.pointer_offset.x, local.x - model.toolbar_position.x);
            assert!(!model.options_open);
            assert!(has_effect(&effects, |e| matches!(
                e,
                OverlayEffect::CapturePointer
            )));
            assert_eq!(
                has_effect(&effects, |e| matches!(
                    e,
                    OverlayEffect::ClearLiveThumbnails
                )),
                expects_clear,
                "{tool:?}"
            );
            assert_eq!(
                has_effect(&effects, |e| matches!(
                    e,
                    OverlayEffect::RebuildOverviewCache
                )),
                expects_clear,
                "{tool:?}"
            );
        }
    }

    #[test]
    fn the_options_menu_toggles_and_its_background_is_inert() {
        let mut model = region_model();
        let layout = layout_for(&model);
        transition(
            &mut model,
            OverlayInput::PointerDown {
                point: center(layout.options),
                window_slot: None,
            },
        );
        assert!(model.options_open);

        // A menu-background press keeps the menu open and starts no drag.
        let menu_background = POINT {
            x: layout.menu.left + 2,
            y: layout.menu.top + 2,
        };
        let effects = transition(
            &mut model,
            OverlayInput::PointerDown {
                point: menu_background,
                window_slot: None,
            },
        );
        assert!(model.options_open);
        assert!(model.toolbar_drag.is_none());
        assert!(close_outcome(&effects).is_none());

        // Pressing Options again closes it.
        transition(
            &mut model,
            OverlayInput::PointerDown {
                point: center(layout.options),
                window_slot: None,
            },
        );
        assert!(!model.options_open);

        // An outside press under the region tool closes the menu too.
        model.options_open = true;
        transition(
            &mut model,
            OverlayInput::PointerDown {
                point: point(5, 5),
                window_slot: None,
            },
        );
        assert!(!model.options_open);
    }

    #[test]
    fn options_under_the_window_tool_refreshes_chrome_on_both_edges() {
        for expected_open in [true, false] {
            let mut model = region_model();
            model.tool = CaptureTool::Window;
            model.options_open = !expected_open;
            let layout = layout_for(&model);
            let effects = transition(
                &mut model,
                OverlayInput::PointerDown {
                    point: center(layout.options),
                    window_slot: None,
                },
            );
            assert_eq!(model.options_open, expected_open);
            assert!(has_effect(&effects, |e| matches!(
                e,
                OverlayEffect::RefreshWindowChrome
            )));
        }
    }

    #[test]
    fn dim_background_toggles_without_closing_the_menu() {
        let mut model = region_model();
        model.options_open = true;
        let dimmed_before = model.dim_background;
        let layout = layout_for(&model);
        let effects = transition(
            &mut model,
            OverlayInput::PointerDown {
                point: center(layout.dim_background),
                window_slot: None,
            },
        );
        assert_eq!(model.dim_background, !dimmed_before);
        assert!(
            model.options_open,
            "the menu stays open for further toggles"
        );
        assert_eq!(model.hovered_control, Some(ToolbarControl::DimBackground));
        assert!(close_outcome(&effects).is_none());
    }

    #[test]
    fn the_snap_row_toggles_persists_and_keeps_the_menu_open() {
        let mut model = region_model();
        model.options_open = true;
        model.snap_targets = None;
        model.active_snaps = [
            Some(SnapGuide {
                axis: SnapAxis::X,
                position: 400,
                span: (300, 500),
                trailing: false,
            }),
            None,
        ];
        let layout = layout_for(&model);

        // On by default, so the first press turns it off.
        let effects = transition(
            &mut model,
            OverlayInput::PointerDown {
                point: center(layout.snap_to_windows),
                window_slot: None,
            },
        );
        assert!(!model.snap_to_windows);
        assert!(
            model.options_open,
            "the menu stays open for further toggles"
        );
        assert_eq!(model.hovered_control, Some(ToolbarControl::SnapToWindows));
        assert_eq!(
            model.active_snaps, NO_SNAPS,
            "a guide describing a rule that no longer applies must go"
        );
        assert!(has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::PersistSnapPreference { enabled: false }
        )));
        assert!(
            !has_effect(&effects, |e| matches!(e, OverlayEffect::BuildSnapTargets)),
            "switching it off must not pay for an enumeration"
        );
        assert!(close_outcome(&effects).is_none());

        // Switching it back on asks for the inventory it will now need.
        let effects = transition(
            &mut model,
            OverlayInput::PointerDown {
                point: center(layout.snap_to_windows),
                window_slot: None,
            },
        );
        assert!(model.snap_to_windows);
        assert!(has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::PersistSnapPreference { enabled: true }
        )));
        assert!(has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::BuildSnapTargets
        )));
    }

    #[test]
    fn the_capture_button_confirms_only_with_a_selection() {
        let layout = layout_for(&region_model());
        let capture = center(layout.capture);

        let mut without = region_model();
        let effects = transition(
            &mut without,
            OverlayInput::PointerDown {
                point: capture,
                window_slot: None,
            },
        );
        assert!(
            effects.is_empty(),
            "no selection: nothing to do, no repaint"
        );

        let mut with = region_model();
        with.selection = Some(Rect {
            x: 10,
            y: 10,
            width: 40,
            height: 40,
        });
        with.selection_kind = Some(SelectionKind::Region);
        let effects = transition(
            &mut with,
            OverlayInput::PointerDown {
                point: capture,
                window_slot: None,
            },
        );
        assert!(matches!(
            close_outcome(&effects),
            Some(CloseOutcome::Confirmed { .. })
        ));
        assert!(persisted_region(&effects).is_some());
    }

    #[test]
    fn the_cancel_row_persists_and_closes() {
        let mut model = region_model();
        model.options_open = true;
        let region = Rect {
            x: 30,
            y: 30,
            width: 60,
            height: 60,
        };
        model.selection = Some(region);
        model.selection_kind = Some(SelectionKind::Region);
        let layout = layout_for(&model);
        let effects = transition(
            &mut model,
            OverlayInput::PointerDown {
                point: center(layout.cancel),
                window_slot: None,
            },
        );
        assert_eq!(persisted_region(&effects), Some(Some(region)));
        assert!(matches!(
            close_outcome(&effects),
            Some(CloseOutcome::Cancelled)
        ));
    }

    fn candidate(raw: isize) -> WindowCandidate {
        WindowCandidate {
            handle: NativeWindowHandle::from_raw(raw),
            frame: Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            },
        }
    }

    #[test]
    fn a_window_slot_click_requests_a_preview_and_confirms_when_it_resolves() {
        let mut model = region_model();
        model.tool = CaptureTool::Window;
        let handle = NativeWindowHandle::from_raw(0x1234);

        let effects = transition(
            &mut model,
            OverlayInput::PointerDown {
                point: point(400, 300),
                window_slot: Some(handle),
            },
        );
        assert_eq!(model.selected_window, Some(handle));
        assert!(has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::UpdateWindowPreview {
                window: Some(window)
            } if *window == handle
        )));
        assert!(
            close_outcome(&effects).is_none(),
            "nothing closes before the preview resolves"
        );

        let frame_rect = Rect {
            x: 200,
            y: 150,
            width: 640,
            height: 480,
        };
        let effects = transition(
            &mut model,
            OverlayInput::WindowPreviewResolved {
                rect: Some(frame_rect),
            },
        );
        assert_eq!(model.selection, Some(frame_rect));
        assert_eq!(model.selection_kind, Some(SelectionKind::Window));
        assert_eq!(
            persisted_region(&effects),
            Some(None),
            "window confirms do not persist a region"
        );
        match close_outcome(&effects) {
            Some(CloseOutcome::Confirmed { kind, window, rect }) => {
                assert_eq!(kind, SelectionKind::Window);
                assert_eq!(window, Some(handle));
                assert_eq!(rect, frame_rect);
            }
            other => panic!("expected a confirmed close, got {other:?}"),
        }
    }

    #[test]
    fn an_unresolved_preview_stays_in_the_chooser_for_a_retry() {
        let mut model = region_model();
        model.tool = CaptureTool::Window;
        let handle = NativeWindowHandle::from_raw(0x1234);

        for _ in 0..2 {
            let effects = transition(
                &mut model,
                OverlayInput::PointerDown {
                    point: point(400, 300),
                    window_slot: Some(handle),
                },
            );
            // Every click re-requests the preview: the shell's cache is deliberately
            // non-sticky about failures, so the machine must always ask again.
            assert!(has_effect(&effects, |e| matches!(
                e,
                OverlayEffect::UpdateWindowPreview { .. }
            )));

            let effects = transition(
                &mut model,
                OverlayInput::WindowPreviewResolved { rect: None },
            );
            assert_eq!(model.selection, None);
            assert_eq!(model.selection_kind, None);
            assert!(close_outcome(&effects).is_none());
            assert!(has_effect(&effects, |e| matches!(
                e,
                OverlayEffect::RebuildOverviewCache
            )));
            assert!(has_effect(&effects, |e| matches!(
                e,
                OverlayEffect::Invalidate
            )));
        }
    }

    #[test]
    fn an_empty_space_window_click_clears_the_selection_target() {
        let mut model = region_model();
        model.tool = CaptureTool::Window;
        model.selected_window = Some(NativeWindowHandle::from_raw(0x1234));

        let effects = transition(
            &mut model,
            OverlayInput::PointerDown {
                point: point(400, 300),
                window_slot: None,
            },
        );
        assert_eq!(model.selected_window, None);
        assert!(has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::UpdateWindowPreview { window: None }
        )));
    }

    #[test]
    fn window_hover_tracks_the_supplied_candidate_without_forcing_a_repaint() {
        let mut model = region_model();
        model.tool = CaptureTool::Window;

        let effects = transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(400, 300),
                window_hover: Some(candidate(0x77)),
                modifiers: Modifiers::default(),
            },
        );
        assert_eq!(
            model.hovered.map(|c| c.handle),
            Some(NativeWindowHandle::from_raw(0x77))
        );
        assert_eq!(model.pointer_local, Some(point(400, 300)));
        // Repaint policy belongs to the shell in the window pass; the machine stays silent.
        assert!(!has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::Invalidate
        )));
        assert!(has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::SetCursor(CursorIntent::Arrow)
        )));

        // Hovering the toolbar overrides the window hover.
        let layout = layout_for(&model);
        let effects = transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: center(layout.capture),
                window_hover: Some(candidate(0x77)),
                modifiers: Modifiers::default(),
            },
        );
        assert_eq!(model.hovered_control, Some(ToolbarControl::Capture));
        assert!(model.hovered.is_none());
        assert!(!has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::Invalidate
        )));

        // Leaving every slot clears the hover.
        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(10, 10),
                window_hover: None,
                modifiers: Modifiers::default(),
            },
        );
        assert!(model.hovered.is_none());
    }

    #[test]
    fn an_outside_click_under_the_window_tool_closes_options_and_refreshes() {
        let mut model = region_model();
        model.tool = CaptureTool::Window;
        model.options_open = true;

        let effects = transition(
            &mut model,
            OverlayInput::PointerDown {
                point: point(400, 300),
                window_slot: None,
            },
        );
        assert!(!model.options_open);
        assert_eq!(model.hovered_control, None);
        let kinds: Vec<_> = effects
            .iter()
            .map(|effect| match effect {
                OverlayEffect::RefreshWindowChrome => "refresh",
                OverlayEffect::UpdateWindowPreview { .. } => "preview",
                _ => "other",
            })
            .collect();
        // The chrome refresh from closing the menu runs before the preview request, as the
        // legacy handler ordered it.
        assert_eq!(kinds, ["refresh", "preview"]);
    }

    #[test]
    fn pruned_sources_clear_only_matching_selection_and_hover() {
        let dead = NativeWindowHandle::from_raw(0xDEAD);
        let alive = NativeWindowHandle::from_raw(0xA11E);

        // Selection and hover pointing at the pruned window are dropped together.
        let mut model = region_model();
        model.tool = CaptureTool::Window;
        model.selected_window = Some(dead);
        model.hovered = Some(candidate(0xDEAD));
        let effects = window_sources_pruned(&mut model, &[dead]);
        assert_eq!(model.selected_window, None);
        assert!(model.hovered.is_none());
        assert!(has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::ClearSelectedWindowFrame
        )));

        // State pointing at windows that are still alive is untouched, and no frame-clearing
        // effect is emitted for a prune that missed everything.
        let mut model = region_model();
        model.tool = CaptureTool::Window;
        model.selected_window = Some(alive);
        model.hovered = Some(candidate(0xA11E));
        let effects = window_sources_pruned(&mut model, &[dead]);
        assert_eq!(model.selected_window, Some(alive));
        assert_eq!(model.hovered.map(|c| c.handle), Some(alive));
        assert!(effects.is_empty());
    }

    /// A window at (400,300) 200x200: columns 400..=599, rows 300..=499.
    const WINDOW: Rect = Rect {
        x: 400,
        y: 300,
        width: 200,
        height: 200,
    };

    const CTRL: Modifiers = Modifiers {
        ctrl: true,
        shift: false,
    };

    fn snapping_model() -> OverlayModel {
        let mut model = region_model();
        model.snap_targets = Some(SnapTargets::new(vec![
            SnapTarget {
                rect: WINDOW,
                kind: SnapTargetKind::Window,
            },
            SnapTarget {
                rect: SOURCE,
                kind: SnapTargetKind::Display,
            },
        ]));
        model
    }

    fn drag(model: &mut OverlayModel, from: POINT, to: POINT, modifiers: Modifiers) {
        transition(
            model,
            OverlayInput::PointerDown {
                point: from,
                window_slot: None,
            },
        );
        transition(
            model,
            OverlayInput::PointerMoved {
                point: to,
                window_hover: None,
                modifiers,
            },
        );
    }

    fn guide(model: &OverlayModel, axis: SnapAxis) -> Option<SnapGuide> {
        model.active_snaps[axis.index()]
    }

    #[test]
    fn a_drawn_region_lands_on_the_exclusive_window_edge_and_shows_why() {
        let mut model = snapping_model();
        // Released four pixels short of the window's bottom-right corner (600, 500).
        drag(
            &mut model,
            point(100, 100),
            point(596, 496),
            Modifiers::default(),
        );
        let region = model.selection.expect("a region");
        assert_eq!(
            (region.right(), region.bottom()),
            (600, 500),
            "x + width is the window's right edge exactly, never one less"
        );
        let horizontal = guide(&model, SnapAxis::X).expect("a vertical guide");
        assert_eq!(horizontal.position, 600);
        assert!(horizontal.trailing);
        assert_eq!(horizontal.span, (300, 500));
        assert!(guide(&model, SnapAxis::Y).is_some());

        // Releasing commits exactly what was shown and then drops the guides: nothing is being
        // adjusted any more, so there is nothing left for them to explain.
        transition(
            &mut model,
            OverlayInput::PointerUp {
                point: point(596, 496),
                modifiers: Modifiers::default(),
            },
        );
        assert_eq!(model.selection, Some(region));
        assert_eq!(model.active_snaps, NO_SNAPS);
    }

    #[test]
    fn the_anchor_snaps_once_the_press_becomes_a_drag_and_not_before() {
        let mut model = snapping_model();
        // Press three pixels inside the window's top-left corner: within the snap radius, but a
        // press is not yet a drag and the threshold must be measured from where the user pressed.
        transition(
            &mut model,
            OverlayInput::PointerDown {
                point: point(403, 303),
                window_slot: None,
            },
        );
        assert_eq!(model.anchor, Some(point(403, 303)));
        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(405, 304),
                window_hover: None,
                modifiers: Modifiers::default(),
            },
        );
        assert!(
            !model.dragging,
            "a two-pixel move is still a click; snapping the anchor at press would have made it a drag"
        );
        assert_eq!(model.anchor, Some(point(403, 303)));

        // Crossing the threshold latches the drag and pulls the anchor onto the window corner.
        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(450, 350),
                window_hover: None,
                modifiers: Modifiers::default(),
            },
        );
        assert!(model.dragging);
        assert_eq!(model.anchor, Some(point(400, 300)));
        assert_eq!(
            model.selection,
            Some(Rect {
                x: 400,
                y: 300,
                width: 50,
                height: 50,
            })
        );
    }

    #[test]
    fn ctrl_places_the_edge_exactly_where_the_pointer_is_and_clears_the_guides() {
        let mut model = snapping_model();
        drag(
            &mut model,
            point(100, 100),
            point(596, 496),
            Modifiers::default(),
        );
        assert!(model.active_snaps.iter().any(Option::is_some));

        // Same pointer position, Ctrl down: the region keeps the pixel the user asked for.
        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(596, 496),
                window_hover: None,
                modifiers: CTRL,
            },
        );
        let region = model.selection.expect("a region");
        assert_eq!((region.right(), region.bottom()), (596, 496));
        assert_eq!(model.active_snaps, NO_SNAPS);

        // The option being off behaves identically without Ctrl being held at all.
        let mut off = snapping_model();
        off.snap_to_windows = false;
        drag(
            &mut off,
            point(100, 100),
            point(596, 496),
            Modifiers::default(),
        );
        let region = off.selection.expect("a region");
        assert_eq!((region.right(), region.bottom()), (596, 496));
        assert_eq!(off.active_snaps, NO_SNAPS);
    }

    #[test]
    fn an_east_resize_snaps_only_the_edge_it_is_dragging() {
        let mut model = snapping_model();
        model.selection = Some(Rect {
            x: 405,
            y: 305,
            width: 191,
            height: 191,
        });
        model.selection_kind = Some(SelectionKind::Region);
        // Grab the east handle: the midpoint of the right edge.
        transition(
            &mut model,
            OverlayInput::PointerDown {
                point: point(596, 400),
                window_slot: None,
            },
        );
        assert_eq!(
            model.resizing.map(|drag| drag.handle),
            Some(ResizeHandle::East)
        );
        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(597, 400),
                window_hover: None,
                modifiers: Modifiers::default(),
            },
        );
        let region = model.selection.expect("a region");
        assert_eq!(region.right(), 600, "the dragged edge snapped");
        assert_eq!(
            region.x, 405,
            "the left edge is not being dragged and must not move, however close it is to 400"
        );
        assert!(guide(&model, SnapAxis::X).is_some());
        assert!(
            guide(&model, SnapAxis::Y).is_none(),
            "the east handle moves nothing vertically"
        );
    }

    #[test]
    fn a_move_snaps_the_nearest_side_and_keeps_the_regions_size() {
        let mut model = snapping_model();
        let original = Rect {
            x: 100,
            y: 100,
            width: 100,
            height: 100,
        };
        model.selection = Some(original);
        model.selection_kind = Some(SelectionKind::Region);
        transition(
            &mut model,
            OverlayInput::PointerDown {
                point: point(150, 150),
                window_slot: None,
            },
        );
        assert!(model.moving_region.is_some());
        // Drag so the region's top-left lands three pixels short of the window's top-left.
        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(453, 353),
                window_hover: None,
                modifiers: Modifiers::default(),
            },
        );
        let region = model.selection.expect("a region");
        assert_eq!(
            (region.width, region.height),
            (100, 100),
            "a move never resizes"
        );
        assert_eq!((region.x, region.y), (400, 300));
        assert_eq!(
            guide(&model, SnapAxis::X).map(|guide| (guide.position, guide.trailing)),
            Some((400, false))
        );
    }

    #[test]
    fn the_minimum_size_wins_over_a_snap_and_takes_the_guide_with_it() {
        // A target edge at 104, inside the minimum-size floor of a region anchored at 100: the
        // resize clamp pulls the edge back to 108, so a guide would be pointing at a coordinate
        // the selection does not reach.
        let mut model = region_model();
        model.snap_targets = Some(SnapTargets::new(vec![SnapTarget {
            rect: Rect {
                x: 104,
                y: 100,
                width: 100,
                height: 200,
            },
            kind: SnapTargetKind::Window,
        }]));
        model.selection = Some(Rect {
            x: 100,
            y: 100,
            width: 40,
            height: 40,
        });
        model.selection_kind = Some(SelectionKind::Region);
        transition(
            &mut model,
            OverlayInput::PointerDown {
                point: point(140, 120),
                window_slot: None,
            },
        );
        assert_eq!(
            model.resizing.map(|drag| drag.handle),
            Some(ResizeHandle::East)
        );
        transition(
            &mut model,
            OverlayInput::PointerMoved {
                point: point(102, 120),
                window_hover: None,
                modifiers: Modifiers::default(),
            },
        );
        let region = model.selection.expect("a region");
        assert_eq!(
            (region.x, region.width),
            (100, MIN_REGION_SIZE as u32),
            "the floor wins over the snap"
        );
        assert_eq!(guide(&model, SnapAxis::X), None);
    }

    #[test]
    fn arrows_move_the_region_by_exact_pixels_and_ctrl_resizes_it() {
        let start = Rect {
            x: 200,
            y: 200,
            width: 100,
            height: 100,
        };
        let cases = [
            // (dx, dy, resize, expected)
            (
                -1,
                0,
                false,
                Rect {
                    x: 199,
                    y: 200,
                    width: 100,
                    height: 100,
                },
            ),
            (
                0,
                10,
                false,
                Rect {
                    x: 200,
                    y: 210,
                    width: 100,
                    height: 100,
                },
            ),
            (
                1,
                0,
                true,
                Rect {
                    x: 200,
                    y: 200,
                    width: 101,
                    height: 100,
                },
            ),
            (
                0,
                -10,
                true,
                Rect {
                    x: 200,
                    y: 200,
                    width: 100,
                    height: 90,
                },
            ),
        ];
        for (dx, dy, resize, expected) in cases {
            let mut model = snapping_model();
            model.selection = Some(start);
            model.selection_kind = Some(SelectionKind::Region);
            let effects = transition(&mut model, OverlayInput::Nudge { dx, dy, resize });
            assert_eq!(model.selection, Some(expected), "{dx},{dy} resize={resize}");
            // A nudge only repaints. Confirming and cancelling are the only ways this overlay
            // ends and both already persist the latest region.
            assert_eq!(effects.len(), 1, "{dx},{dy} resize={resize}");
            assert!(matches!(effects[0], OverlayEffect::Invalidate));
        }
    }

    #[test]
    fn a_nudge_never_snaps_and_clears_any_guide() {
        // Parked exactly one pixel off a window edge — inside the snap radius — and then nudged
        // away from it. The keys are the way to place a pixel the mouse cannot reach, so a rule
        // that pulled the edge back onto the window would make them useless for their one job.
        let mut model = snapping_model();
        model.selection = Some(Rect {
            x: 399,
            y: 299,
            width: 100,
            height: 100,
        });
        model.selection_kind = Some(SelectionKind::Region);
        model.active_snaps = [
            Some(SnapGuide {
                axis: SnapAxis::X,
                position: 400,
                span: (300, 500),
                trailing: false,
            }),
            None,
        ];
        transition(
            &mut model,
            OverlayInput::Nudge {
                dx: -1,
                dy: 0,
                resize: false,
            },
        );
        assert_eq!(
            model.selection.map(|region| region.x),
            Some(398),
            "the region went exactly where it was told"
        );
        assert_eq!(model.active_snaps, NO_SNAPS);
    }

    #[test]
    fn a_nudge_is_inert_without_a_region_and_while_a_drag_owns_the_geometry() {
        let region = Rect {
            x: 200,
            y: 200,
            width: 100,
            height: 100,
        };
        let nudge = OverlayInput::Nudge {
            dx: 1,
            dy: 0,
            resize: false,
        };

        // No region selection: the full-display and window tools have nothing to nudge.
        for tool in [CaptureTool::FullDisplay, CaptureTool::Window] {
            let mut model = region_model();
            model.tool = tool;
            model.selection = Some(region);
            model.selection_kind = Some(SelectionKind::Display);
            assert!(transition(&mut model, nudge.clone()).is_empty(), "{tool:?}");
            assert_eq!(model.selection, Some(region));
        }

        // Mid-drag: the next mouse message would recompute the rectangle from its own anchor
        // anyway, so a key arriving now could only make the overlay flicker.
        let mut model = region_model();
        model.selection = Some(region);
        model.selection_kind = Some(SelectionKind::Region);
        transition(
            &mut model,
            OverlayInput::PointerDown {
                point: point(250, 250),
                window_slot: None,
            },
        );
        assert!(model.moving_region.is_some());
        assert!(transition(&mut model, nudge.clone()).is_empty());
        assert_eq!(model.selection, Some(region));
    }

    #[test]
    fn a_nudge_held_against_a_wall_stops_repainting() {
        let mut model = region_model();
        // Flush against the left edge of the display already.
        model.selection = Some(Rect {
            x: SOURCE.x,
            y: 100,
            width: 100,
            height: 100,
        });
        model.selection_kind = Some(SelectionKind::Region);
        let effects = transition(
            &mut model,
            OverlayInput::Nudge {
                dx: -10,
                dy: 0,
                resize: false,
            },
        );
        assert!(
            effects.is_empty(),
            "autorepeat against a wall must not repaint sixty times a second"
        );
        assert_eq!(model.selection.map(|region| region.x), Some(SOURCE.x));
    }

    #[test]
    fn shift_takes_the_coarse_step() {
        assert_eq!(Modifiers::default().nudge_step(), NUDGE_STEP);
        assert_eq!(
            Modifiers {
                ctrl: false,
                shift: true,
            }
            .nudge_step(),
            NUDGE_COARSE_STEP
        );
        assert_eq!(
            CTRL.nudge_step(),
            NUDGE_STEP,
            "Ctrl alone is still one pixel"
        );
    }

    #[test]
    fn guides_do_not_survive_a_stolen_capture_or_a_tool_switch() {
        for steal in [true, false] {
            let mut model = snapping_model();
            drag(
                &mut model,
                point(100, 100),
                point(596, 496),
                Modifiers::default(),
            );
            assert!(model.active_snaps.iter().any(Option::is_some));
            if steal {
                transition(&mut model, OverlayInput::PointerCaptureLost);
            } else {
                activate_tool(&mut model, CaptureTool::FullDisplay);
            }
            assert_eq!(model.active_snaps, NO_SNAPS, "steal={steal}");
        }
    }

    mod snap_properties {
        use super::*;
        use proptest::prelude::*;

        fn any_point() -> impl Strategy<Value = POINT> {
            (0i32..=1_919, 0i32..=1_079).prop_map(|(x, y)| POINT { x, y })
        }

        proptest! {
            /// However a region is drawn against the inventory it stays a legal selection: inside
            /// the captured display, and committed as what was on screen. Snapping runs before
            /// the display clamp and must never be able to overrule it.
            #[test]
            fn a_drawn_region_stays_inside_the_display(
                from in any_point(),
                to in any_point(),
                ctrl in proptest::bool::ANY,
            ) {
                let modifiers = Modifiers { ctrl, shift: false };
                let mut model = snapping_model();
                drag(&mut model, from, to, modifiers);
                let shown = model.selection;
                transition(
                    &mut model,
                    OverlayInput::PointerUp { point: to, modifiers },
                );
                prop_assert_eq!(model.selection, shown, "the release commits what was shown");
                if let Some(region) = model.selection {
                    prop_assert_eq!(region.intersection(SOURCE), Some(region));
                }
                if ctrl {
                    prop_assert_eq!(model.active_snaps, NO_SNAPS);
                }
            }

            /// Moving a region preserves its size exactly, wherever it is dragged to and whatever
            /// it snaps against on the way.
            #[test]
            fn a_moved_region_keeps_its_size_and_stays_inside(
                to in any_point(),
                ctrl in proptest::bool::ANY,
            ) {
                let original = Rect { x: 100, y: 100, width: 300, height: 200 };
                let mut model = snapping_model();
                model.selection = Some(original);
                model.selection_kind = Some(SelectionKind::Region);
                transition(
                    &mut model,
                    OverlayInput::PointerDown { point: point(200, 150), window_slot: None },
                );
                transition(
                    &mut model,
                    OverlayInput::PointerMoved {
                        point: to,
                        window_hover: None,
                        modifiers: Modifiers { ctrl, shift: false },
                    },
                );
                let moved = model.selection.expect("a moved region");
                prop_assert_eq!((moved.width, moved.height), (original.width, original.height));
                prop_assert_eq!(moved.intersection(SOURCE), Some(moved));
            }

            /// However many arrows are held down, and in whatever order, the region stays a legal
            /// selection: inside the display and never under the floor. Autorepeat delivers these
            /// by the dozen, so "eventually illegal" would be reached in about a second.
            #[test]
            fn any_run_of_nudges_leaves_a_legal_region(
                steps in proptest::collection::vec(
                    (-10i32..=10, -10i32..=10, proptest::bool::ANY),
                    1..=24,
                ),
            ) {
                let mut model = region_model();
                model.selection = Some(Rect { x: 900, y: 500, width: 120, height: 90 });
                model.selection_kind = Some(SelectionKind::Region);
                for (dx, dy, resize) in steps {
                    transition(&mut model, OverlayInput::Nudge { dx, dy, resize });
                    let region = model.selection.expect("a nudge never drops the selection");
                    prop_assert_eq!(region.intersection(SOURCE), Some(region));
                    prop_assert!(i64::from(region.width) >= MIN_REGION_SIZE);
                    prop_assert!(i64::from(region.height) >= MIN_REGION_SIZE);
                }
            }

            /// Re-delivering the same pointer position is a no-op. Windows coalesces and repeats
            /// mouse messages freely, and a snap that crept would walk a still region.
            #[test]
            fn repeating_a_pointer_position_changes_nothing(
                from in any_point(),
                to in any_point(),
            ) {
                let mut model = snapping_model();
                drag(&mut model, from, to, Modifiers::default());
                let once = (model.selection, model.active_snaps);
                transition(
                    &mut model,
                    OverlayInput::PointerMoved {
                        point: to,
                        window_hover: None,
                        modifiers: Modifiers::default(),
                    },
                );
                prop_assert_eq!((model.selection, model.active_snaps), once);
            }
        }
    }

    #[test]
    fn the_snap_inventory_is_requested_once_and_only_where_it_could_be_used() {
        // A region press with no inventory yet asks for one.
        let mut model = region_model();
        model.snap_targets = None;
        let effects = transition(
            &mut model,
            OverlayInput::PointerDown {
                point: point(100, 100),
                window_slot: None,
            },
        );
        assert!(has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::BuildSnapTargets
        )));

        // The shell answers, and no later press asks again: enumeration is the expensive part of
        // opening the chooser and a drag must not repeat it.
        let effects = transition(
            &mut model,
            OverlayInput::SnapTargetsReady {
                targets: SnapTargets::default(),
            },
        );
        assert!(effects.is_empty(), "nothing visible changed");
        assert!(model.snap_targets.is_some());
        let effects = transition(
            &mut model,
            OverlayInput::PointerDown {
                point: point(400, 400),
                window_slot: None,
            },
        );
        assert!(!has_effect(&effects, |e| matches!(
            e,
            OverlayEffect::BuildSnapTargets
        )));

        // Switching to Region asks for it; the other tools never do, and neither does a run with
        // snapping switched off.
        for (tool, snap, expected) in [
            (CaptureTool::Region, true, true),
            (CaptureTool::Region, false, false),
            (CaptureTool::FullDisplay, true, false),
            (CaptureTool::Window, true, false),
        ] {
            let mut model = region_model();
            model.tool = CaptureTool::FullDisplay;
            model.snap_targets = None;
            model.snap_to_windows = snap;
            let effects = activate_tool(&mut model, tool);
            assert_eq!(
                has_effect(&effects, |e| matches!(e, OverlayEffect::BuildSnapTargets)),
                expected,
                "{tool:?} snap={snap}"
            );
        }
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
            let effects = transition(&mut model, input.clone());
            if let Some(position) = effects
                .iter()
                .position(|effect| matches!(effect, OverlayEffect::Close(_)))
            {
                assert_eq!(position, effects.len() - 1, "input {input:?}");
            }
        }
    }
}
