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
}
