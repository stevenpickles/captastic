use windows::Win32::Foundation::POINT;

const TOOLBAR_WIDTH: i32 = 600;
const TOOLBAR_HEIGHT: i32 = 82;
const TOOLBAR_BOTTOM_MARGIN: i32 = 36;
const MENU_WIDTH: i32 = 320;
const MENU_HEIGHT: i32 = 164;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ToolbarControl {
    Background,
    FullDisplay,
    Window,
    Region,
    Options,
    Capture,
    DimBackground,
    ClipboardDestination,
    Cancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct UiRect {
    pub(super) left: i32,
    pub(super) top: i32,
    pub(super) right: i32,
    pub(super) bottom: i32,
}

impl UiRect {
    pub(super) fn contains(self, point: POINT) -> bool {
        point.x >= self.left && point.x < self.right && point.y >= self.top && point.y < self.bottom
    }

    pub(super) fn width(self) -> i32 {
        (self.right - self.left).max(1)
    }

    pub(super) fn height(self) -> i32 {
        (self.bottom - self.top).max(1)
    }

    fn intersects(self, other: Self) -> bool {
        self.left < other.right
            && self.right > other.left
            && self.top < other.bottom
            && self.bottom > other.top
    }

    fn intersection_area(self, other: Self) -> i64 {
        let width = (self.right.min(other.right) - self.left.max(other.left)).max(0);
        let height = (self.bottom.min(other.bottom) - self.top.max(other.top)).max(0);
        i64::from(width) * i64::from(height)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct UiSize {
    pub(super) width: i32,
    pub(super) height: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DimensionLabelPlacement {
    Inside,
    Top,
    Bottom,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DimensionLabelLayout {
    pub(super) bounds: UiRect,
    pub(super) placement: DimensionLabelPlacement,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RegionLayoutTokens {
    pub(super) label_font_height: i32,
    pub(super) label_padding_x: i32,
    pub(super) label_padding_y: i32,
    pub(super) label_corner_radius: i32,
    monitor_inset: i32,
    selection_gap: i32,
    handle_clearance: i32,
    inside_hysteresis: i32,
}

impl RegionLayoutTokens {
    fn new(metrics: UiMetrics) -> Self {
        Self {
            label_font_height: metrics.px(15),
            label_padding_x: metrics.px(10),
            label_padding_y: metrics.px(5),
            label_corner_radius: metrics.px(8),
            monitor_inset: metrics.px(8),
            selection_gap: metrics.px(8),
            handle_clearance: metrics.px(12),
            inside_hysteresis: metrics.px(8),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct UiMetrics {
    pub(super) dpi: u32,
}

impl UiMetrics {
    pub(super) const BASE_DPI: u32 = 96;

    pub(super) const fn new(dpi: u32) -> Self {
        Self {
            dpi: if dpi == 0 { Self::BASE_DPI } else { dpi },
        }
    }

    pub(super) fn px(self, dip: i32) -> i32 {
        let scaled = i64::from(dip) * i64::from(self.dpi) + i64::from(Self::BASE_DPI / 2);
        i32::try_from(scaled / i64::from(Self::BASE_DPI)).unwrap_or(i32::MAX)
    }

    pub(super) fn region_tokens(self) -> RegionLayoutTokens {
        RegionLayoutTokens::new(self)
    }

    pub(super) fn toolbar_width(self) -> i32 {
        self.px(TOOLBAR_WIDTH)
    }

    pub(super) fn toolbar_height(self) -> i32 {
        self.px(TOOLBAR_HEIGHT)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DisplayEnvironment {
    pub(super) work_area: UiRect,
    pub(super) metrics: UiMetrics,
}

#[derive(Clone, Copy)]
pub(super) struct ToolbarLayout {
    pub(super) bounds: UiRect,
    pub(super) drag_handle: UiRect,
    pub(super) full_display: UiRect,
    pub(super) window: UiRect,
    pub(super) region: UiRect,
    pub(super) options: UiRect,
    pub(super) capture: UiRect,
    pub(super) menu: UiRect,
    pub(super) dim_background: UiRect,
    pub(super) clipboard_destination: UiRect,
    pub(super) cancel: UiRect,
}

impl ToolbarLayout {
    pub(super) fn default_origin(environment: DisplayEnvironment) -> POINT {
        let work_area = environment.work_area;
        let metrics = environment.metrics;
        Self::clamp_origin(
            environment,
            POINT {
                x: work_area.left + (work_area.width() - metrics.toolbar_width()) / 2,
                y: work_area.bottom - metrics.toolbar_height() - metrics.px(TOOLBAR_BOTTOM_MARGIN),
            },
        )
    }

    pub(super) fn clamp_origin(environment: DisplayEnvironment, origin: POINT) -> POINT {
        let work_area = environment.work_area;
        let metrics = environment.metrics;
        let inset = metrics.px(8);
        POINT {
            x: origin.x.clamp(
                work_area.left + inset,
                (work_area.right - metrics.toolbar_width() - inset).max(work_area.left + inset),
            ),
            y: origin.y.clamp(
                work_area.top + inset,
                (work_area.bottom - metrics.toolbar_height() - inset).max(work_area.top + inset),
            ),
        }
    }

    pub(super) fn new(environment: DisplayEnvironment, origin: POINT) -> Self {
        let metrics = environment.metrics;
        let work_area = environment.work_area;
        let origin = Self::clamp_origin(environment, origin);
        let left = origin.x;
        let top = origin.y;
        let bounds = UiRect {
            left,
            top,
            right: left + metrics.toolbar_width(),
            bottom: top + metrics.toolbar_height(),
        };
        let menu_width = metrics.px(MENU_WIDTH);
        let menu_height = metrics.px(MENU_HEIGHT);
        let inset = metrics.px(8);
        let menu_left = (bounds.right - menu_width - metrics.px(12)).clamp(
            work_area.left + inset,
            (work_area.right - menu_width - inset).max(work_area.left + inset),
        );
        let menu_top = if top >= work_area.top + menu_height + metrics.px(16) {
            top - menu_height - metrics.px(10)
        } else {
            (bounds.bottom + metrics.px(10)).min(work_area.bottom - menu_height - inset)
        };
        let menu = UiRect {
            left: menu_left,
            top: menu_top,
            right: menu_left + menu_width,
            bottom: menu_top + menu_height,
        };
        Self {
            bounds,
            drag_handle: UiRect {
                left: left + metrics.px(8),
                top: top + metrics.px(10),
                right: left + metrics.px(44),
                bottom: top + metrics.px(72),
            },
            full_display: UiRect {
                left: left + metrics.px(48),
                top: top + metrics.px(10),
                right: left + metrics.px(112),
                bottom: top + metrics.px(72),
            },
            window: UiRect {
                left: left + metrics.px(112),
                top: top + metrics.px(10),
                right: left + metrics.px(176),
                bottom: top + metrics.px(72),
            },
            region: UiRect {
                left: left + metrics.px(176),
                top: top + metrics.px(10),
                right: left + metrics.px(240),
                bottom: top + metrics.px(72),
            },
            options: UiRect {
                left: left + metrics.px(270),
                top: top + metrics.px(10),
                right: left + metrics.px(402),
                bottom: top + metrics.px(72),
            },
            capture: UiRect {
                left: left + metrics.px(418),
                top: top + metrics.px(10),
                right: left + metrics.px(584),
                bottom: top + metrics.px(72),
            },
            dim_background: UiRect {
                left: menu.left + metrics.px(6),
                top: menu.top + metrics.px(6),
                right: menu.right - metrics.px(6),
                bottom: menu.top + metrics.px(56),
            },
            clipboard_destination: UiRect {
                left: menu.left + metrics.px(6),
                top: menu.top + metrics.px(56),
                right: menu.right - metrics.px(6),
                bottom: menu.top + metrics.px(106),
            },
            cancel: UiRect {
                left: menu.left + metrics.px(6),
                top: menu.top + metrics.px(106),
                right: menu.right - metrics.px(6),
                bottom: menu.bottom - metrics.px(6),
            },
            menu,
        }
    }

    pub(super) fn hit_test(self, point: POINT, options_open: bool) -> Option<ToolbarControl> {
        if options_open && self.menu.contains(point) {
            if self.dim_background.contains(point) {
                return Some(ToolbarControl::DimBackground);
            }
            if self.clipboard_destination.contains(point) {
                return Some(ToolbarControl::ClipboardDestination);
            }
            if self.cancel.contains(point) {
                return Some(ToolbarControl::Cancel);
            }
            return Some(ToolbarControl::Background);
        }
        if !self.bounds.contains(point) {
            return None;
        }
        if self.full_display.contains(point) {
            Some(ToolbarControl::FullDisplay)
        } else if self.window.contains(point) {
            Some(ToolbarControl::Window)
        } else if self.region.contains(point) {
            Some(ToolbarControl::Region)
        } else if self.options.contains(point) {
            Some(ToolbarControl::Options)
        } else if self.capture.contains(point) {
            Some(ToolbarControl::Capture)
        } else {
            Some(ToolbarControl::Background)
        }
    }
}

#[derive(Clone, Copy)]
struct DimensionCandidate {
    layout: DimensionLabelLayout,
    obstacle_overlap: bool,
    pointer_overlap: bool,
    clearance: i32,
}

pub(super) fn layout_dimension_label(
    monitor: UiRect,
    selection: UiRect,
    requested_size: UiSize,
    reserved: &[UiRect],
    pointer_exclusion: Option<UiRect>,
    previous: Option<DimensionLabelPlacement>,
    tokens: RegionLayoutTokens,
) -> DimensionLabelLayout {
    let safe = UiRect {
        left: monitor.left.saturating_add(tokens.monitor_inset),
        top: monitor.top.saturating_add(tokens.monitor_inset),
        right: monitor
            .right
            .saturating_sub(tokens.monitor_inset)
            .max(monitor.left.saturating_add(tokens.monitor_inset + 1)),
        bottom: monitor
            .bottom
            .saturating_sub(tokens.monitor_inset)
            .max(monitor.top.saturating_add(tokens.monitor_inset + 1)),
    };
    let size = UiSize {
        width: requested_size.width.max(1).min(safe.width()),
        height: requested_size.height.max(1).min(safe.height()),
    };
    let reentry_margin = if matches!(previous, Some(DimensionLabelPlacement::Inside) | None) {
        0
    } else {
        tokens.inside_hysteresis
    };
    let inside_fits = selection.width()
        >= size
            .width
            .saturating_add(tokens.handle_clearance.saturating_mul(2))
            .saturating_add(reentry_margin)
        && selection.height()
            >= size
                .height
                .saturating_add(tokens.handle_clearance.saturating_mul(2))
                .saturating_add(reentry_margin);

    let centered_left = selection
        .left
        .saturating_add((selection.width() - size.width) / 2);
    let centered_top = selection
        .top
        .saturating_add((selection.height() - size.height) / 2);
    let horizontal_left = clamp_coordinate(
        centered_left,
        safe.left,
        safe.right.saturating_sub(size.width),
    );
    let vertical_top = clamp_coordinate(
        centered_top,
        safe.top,
        safe.bottom.saturating_sub(size.height),
    );

    let mut candidates = Vec::with_capacity(5);
    if inside_fits {
        let top = selection.top.saturating_add(tokens.handle_clearance);
        push_dimension_candidate(
            &mut candidates,
            DimensionLabelLayout {
                bounds: UiRect {
                    left: horizontal_left,
                    top,
                    right: horizontal_left.saturating_add(size.width),
                    bottom: top.saturating_add(size.height),
                },
                placement: DimensionLabelPlacement::Inside,
            },
            safe,
            selection,
            reserved,
            pointer_exclusion,
            i32::MAX / 2,
        );
    }

    let top = selection
        .top
        .saturating_sub(tokens.selection_gap)
        .saturating_sub(size.height);
    push_dimension_candidate(
        &mut candidates,
        DimensionLabelLayout {
            bounds: UiRect {
                left: horizontal_left,
                top,
                right: horizontal_left.saturating_add(size.width),
                bottom: top.saturating_add(size.height),
            },
            placement: DimensionLabelPlacement::Top,
        },
        safe,
        selection,
        reserved,
        pointer_exclusion,
        top.saturating_sub(safe.top),
    );

    let bottom = selection.bottom.saturating_add(tokens.selection_gap);
    push_dimension_candidate(
        &mut candidates,
        DimensionLabelLayout {
            bounds: UiRect {
                left: horizontal_left,
                top: bottom,
                right: horizontal_left.saturating_add(size.width),
                bottom: bottom.saturating_add(size.height),
            },
            placement: DimensionLabelPlacement::Bottom,
        },
        safe,
        selection,
        reserved,
        pointer_exclusion,
        safe.bottom
            .saturating_sub(bottom.saturating_add(size.height)),
    );

    let left = selection
        .left
        .saturating_sub(tokens.selection_gap)
        .saturating_sub(size.width);
    push_dimension_candidate(
        &mut candidates,
        DimensionLabelLayout {
            bounds: UiRect {
                left,
                top: vertical_top,
                right: left.saturating_add(size.width),
                bottom: vertical_top.saturating_add(size.height),
            },
            placement: DimensionLabelPlacement::Left,
        },
        safe,
        selection,
        reserved,
        pointer_exclusion,
        left.saturating_sub(safe.left),
    );

    let right = selection.right.saturating_add(tokens.selection_gap);
    push_dimension_candidate(
        &mut candidates,
        DimensionLabelLayout {
            bounds: UiRect {
                left: right,
                top: vertical_top,
                right: right.saturating_add(size.width),
                bottom: vertical_top.saturating_add(size.height),
            },
            placement: DimensionLabelPlacement::Right,
        },
        safe,
        selection,
        reserved,
        pointer_exclusion,
        safe.right.saturating_sub(right.saturating_add(size.width)),
    );

    select_dimension_candidate(&candidates, previous)
        .map(|candidate| candidate.layout)
        .unwrap_or_else(|| {
            fallback_dimension_label(
                safe,
                selection,
                size,
                reserved,
                pointer_exclusion,
                previous,
                tokens.selection_gap,
            )
        })
}

#[allow(clippy::too_many_arguments)]
fn push_dimension_candidate(
    candidates: &mut Vec<DimensionCandidate>,
    layout: DimensionLabelLayout,
    safe: UiRect,
    selection: UiRect,
    reserved: &[UiRect],
    pointer_exclusion: Option<UiRect>,
    clearance: i32,
) {
    if layout.bounds.left < safe.left
        || layout.bounds.top < safe.top
        || layout.bounds.right > safe.right
        || layout.bounds.bottom > safe.bottom
        || (layout.placement != DimensionLabelPlacement::Inside
            && layout.bounds.intersects(selection))
    {
        return;
    }
    candidates.push(DimensionCandidate {
        layout,
        obstacle_overlap: reserved
            .iter()
            .any(|obstacle| layout.bounds.intersects(*obstacle)),
        pointer_overlap: pointer_exclusion.is_some_and(|pointer| layout.bounds.intersects(pointer)),
        clearance: clearance.max(0),
    });
}

fn select_dimension_candidate(
    candidates: &[DimensionCandidate],
    previous: Option<DimensionLabelPlacement>,
) -> Option<DimensionCandidate> {
    let best_collision = candidates
        .iter()
        .map(|candidate| (candidate.obstacle_overlap, candidate.pointer_overlap))
        .min()?;
    let viable = candidates.iter().filter(|candidate| {
        (candidate.obstacle_overlap, candidate.pointer_overlap) == best_collision
    });
    if let Some(candidate) = viable
        .clone()
        .find(|candidate| candidate.layout.placement == DimensionLabelPlacement::Inside)
    {
        return Some(*candidate);
    }
    if let Some(previous) = previous {
        if let Some(candidate) = viable
            .clone()
            .find(|candidate| candidate.layout.placement == previous)
        {
            return Some(*candidate);
        }
    }
    viable
        .max_by_key(|candidate| {
            (
                candidate.clearance,
                placement_tiebreak(candidate.layout.placement),
            )
        })
        .copied()
}

fn fallback_dimension_label(
    safe: UiRect,
    selection: UiRect,
    size: UiSize,
    reserved: &[UiRect],
    pointer_exclusion: Option<UiRect>,
    previous: Option<DimensionLabelPlacement>,
    gap: i32,
) -> DimensionLabelLayout {
    let centered_left = clamp_coordinate(
        selection
            .left
            .saturating_add((selection.width() - size.width) / 2),
        safe.left,
        safe.right.saturating_sub(size.width),
    );
    let centered_top = clamp_coordinate(
        selection
            .top
            .saturating_add((selection.height() - size.height) / 2),
        safe.top,
        safe.bottom.saturating_sub(size.height),
    );
    let raw = [
        (
            DimensionLabelPlacement::Top,
            centered_left,
            selection
                .top
                .saturating_sub(gap)
                .saturating_sub(size.height),
        ),
        (
            DimensionLabelPlacement::Bottom,
            centered_left,
            selection.bottom.saturating_add(gap),
        ),
        (
            DimensionLabelPlacement::Left,
            selection
                .left
                .saturating_sub(gap)
                .saturating_sub(size.width),
            centered_top,
        ),
        (
            DimensionLabelPlacement::Right,
            selection.right.saturating_add(gap),
            centered_top,
        ),
    ];
    raw.into_iter()
        .map(|(placement, left, top)| {
            let left = clamp_coordinate(left, safe.left, safe.right.saturating_sub(size.width));
            let top = clamp_coordinate(top, safe.top, safe.bottom.saturating_sub(size.height));
            let bounds = UiRect {
                left,
                top,
                right: left.saturating_add(size.width),
                bottom: top.saturating_add(size.height),
            };
            (
                DimensionLabelLayout { bounds, placement },
                bounds.intersection_area(selection),
                reserved
                    .iter()
                    .map(|obstacle| bounds.intersection_area(*obstacle))
                    .sum::<i64>(),
                pointer_exclusion.map_or(0, |pointer| bounds.intersection_area(pointer)),
                previous != Some(placement),
                -placement_tiebreak(placement),
            )
        })
        .min_by_key(
            |(_, selection_area, obstacle_area, pointer_area, changed, order)| {
                (
                    *selection_area,
                    *obstacle_area,
                    *pointer_area,
                    *changed,
                    *order,
                )
            },
        )
        .map(|(layout, ..)| layout)
        .unwrap_or(DimensionLabelLayout {
            bounds: safe,
            placement: DimensionLabelPlacement::Top,
        })
}

fn clamp_coordinate(value: i32, minimum: i32, maximum: i32) -> i32 {
    if maximum <= minimum {
        minimum
    } else {
        value.clamp(minimum, maximum)
    }
}

const fn placement_tiebreak(placement: DimensionLabelPlacement) -> i32 {
    match placement {
        DimensionLabelPlacement::Top => 4,
        DimensionLabelPlacement::Bottom => 3,
        DimensionLabelPlacement::Right => 2,
        DimensionLabelPlacement::Left => 1,
        DimensionLabelPlacement::Inside => 0,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpi_conversion_rounds_fractional_pixels_consistently() {
        assert_eq!(UiMetrics::new(96).px(82), 82);
        assert_eq!(UiMetrics::new(120).px(82), 103);
        assert_eq!(UiMetrics::new(144).px(82), 123);
        assert_eq!(UiMetrics::new(192).px(82), 164);
    }
    #[test]
    fn comfortable_region_keeps_dimensions_inside() {
        let layout = test_label_layout(
            UiRect {
                left: 200,
                top: 150,
                right: 600,
                bottom: 400,
            },
            &[],
            None,
            None,
        );
        assert_eq!(layout.placement, DimensionLabelPlacement::Inside);
        assert!(layout.bounds.left >= 200);
        assert!(layout.bounds.bottom <= 400);
    }

    #[test]
    fn tiny_region_uses_the_side_with_most_display_space() {
        let layout = test_label_layout(
            UiRect {
                left: 4,
                top: 4,
                right: 12,
                bottom: 12,
            },
            &[],
            None,
            None,
        );
        assert_eq!(layout.placement, DimensionLabelPlacement::Right);
        assert!(layout.bounds.left >= 8);
        assert!(layout.bounds.top >= 8);
        assert!(layout.bounds.right <= 792);
        assert!(layout.bounds.bottom <= 592);
    }

    #[test]
    fn edge_region_rejects_the_unavailable_side() {
        let layout = test_label_layout(
            UiRect {
                left: 760,
                top: 260,
                right: 800,
                bottom: 300,
            },
            &[],
            None,
            None,
        );
        assert_eq!(layout.placement, DimensionLabelPlacement::Left);
        assert!(layout.bounds.right < 760);
    }

    #[test]
    fn toolbar_and_pointer_exclusions_change_the_initial_side() {
        let selection = UiRect {
            left: 360,
            top: 260,
            right: 400,
            bottom: 300,
        };
        let right_candidate = UiRect {
            left: 408,
            top: 266,
            right: 528,
            bottom: 294,
        };
        let toolbar_avoided = test_label_layout(selection, &[right_candidate], None, None);
        assert_ne!(toolbar_avoided.placement, DimensionLabelPlacement::Right);
        let pointer_avoided = test_label_layout(selection, &[], Some(right_candidate), None);
        assert_ne!(pointer_avoided.placement, DimensionLabelPlacement::Right);
    }

    #[test]
    fn previous_external_side_is_stable_until_it_becomes_invalid() {
        let layout = test_label_layout(
            UiRect {
                left: 360,
                top: 260,
                right: 400,
                bottom: 300,
            },
            &[],
            None,
            Some(DimensionLabelPlacement::Top),
        );
        assert_eq!(layout.placement, DimensionLabelPlacement::Top);
    }

    #[test]
    fn outside_to_inside_transition_has_hysteresis() {
        let almost_comfortable = test_label_layout(
            UiRect {
                left: 300,
                top: 250,
                right: 450,
                bottom: 310,
            },
            &[],
            None,
            Some(DimensionLabelPlacement::Right),
        );
        assert_ne!(
            almost_comfortable.placement,
            DimensionLabelPlacement::Inside
        );
        let comfortably_larger = test_label_layout(
            UiRect {
                left: 300,
                top: 240,
                right: 460,
                bottom: 310,
            },
            &[],
            None,
            Some(DimensionLabelPlacement::Right),
        );
        assert_eq!(
            comfortably_larger.placement,
            DimensionLabelPlacement::Inside
        );
    }

    #[test]
    fn vertical_space_selects_top_or_bottom_deterministically() {
        let above = test_label_layout(
            UiRect {
                left: 360,
                top: 540,
                right: 400,
                bottom: 580,
            },
            &[],
            None,
            None,
        );
        assert_eq!(above.placement, DimensionLabelPlacement::Top);
        let below = test_label_layout(
            UiRect {
                left: 360,
                top: 20,
                right: 400,
                bottom: 60,
            },
            &[],
            None,
            None,
        );
        assert_eq!(below.placement, DimensionLabelPlacement::Bottom);
    }

    #[test]
    fn negative_monitor_origins_do_not_leak_out_of_bounds() {
        let monitor = UiRect {
            left: -1920,
            top: -200,
            right: 0,
            bottom: 1000,
        };
        let layout = layout_dimension_label(
            monitor,
            UiRect {
                left: -1910,
                top: -190,
                right: -1902,
                bottom: -182,
            },
            UiSize {
                width: 120,
                height: 28,
            },
            &[],
            None,
            None,
            UiMetrics::new(144).region_tokens(),
        );
        assert!(layout.bounds.left >= monitor.left);
        assert!(layout.bounds.top >= monitor.top);
        assert!(layout.bounds.right <= monitor.right);
        assert!(layout.bounds.bottom <= monitor.bottom);
        assert_eq!(layout.placement, DimensionLabelPlacement::Right);
    }

    fn test_label_layout(
        selection: UiRect,
        reserved: &[UiRect],
        pointer_exclusion: Option<UiRect>,
        previous: Option<DimensionLabelPlacement>,
    ) -> DimensionLabelLayout {
        layout_dimension_label(
            UiRect {
                left: 0,
                top: 0,
                right: 800,
                bottom: 600,
            },
            selection,
            UiSize {
                width: 120,
                height: 28,
            },
            reserved,
            pointer_exclusion,
            previous,
            UiMetrics::new(96).region_tokens(),
        )
    }
}
