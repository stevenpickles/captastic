use windows::core::Error;
use windows::Win32::Foundation::{BOOL, HWND, RECT, SIZE};
use windows::Win32::Graphics::Dwm::{
    DwmQueryThumbnailSourceSize, DwmRegisterThumbnail, DwmUnregisterThumbnail,
    DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY,
    DWM_TNP_RECTDESTINATION, DWM_TNP_VISIBLE,
};

/// Owns a DWM thumbnail registration and unregisters it before its destination disappears.
pub(crate) struct DwmThumbnail {
    handle: isize,
}

impl DwmThumbnail {
    pub(crate) fn register(destination: HWND, source: HWND) -> Result<Self, Error> {
        // SAFETY: Both handles identify top-level windows owned by live desktop processes. DWM
        // copies them into a compositor-managed relationship represented by the returned handle.
        let handle = unsafe { DwmRegisterThumbnail(destination, source) }?;
        Ok(Self { handle })
    }

    pub(crate) fn source_size(&self) -> Result<SIZE, Error> {
        // SAFETY: The handle remains registered for the lifetime of this owner.
        unsafe { DwmQueryThumbnailSourceSize(self.handle) }
    }

    /// Shows the whole source window, non-client area included.
    ///
    /// `DWM_TNP_SOURCECLIENTAREAONLY` is deliberately absent rather than set to `FALSE`. Window
    /// capture crops to the DWM extended frame bounds — the window as the user sees it, title bar
    /// and all — so a client-area-only preview would show strictly less than the frame it is
    /// previewing, and the picker would promise a capture it does not deliver. A fresh
    /// registration already defaults to whole-window, so naming the flag only to disable it stated
    /// an intent the value contradicted.
    pub(crate) fn show(&self, destination: RECT, opacity: u8) -> Result<(), Error> {
        let properties = DWM_THUMBNAIL_PROPERTIES {
            dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_OPACITY | DWM_TNP_VISIBLE,
            rcDestination: destination,
            opacity,
            fVisible: BOOL(1),
            ..Default::default()
        };
        // SAFETY: The properties value remains valid for the duration of the call and the handle
        // remains registered for the lifetime of this owner.
        unsafe { DwmUpdateThumbnailProperties(self.handle, &properties) }
    }

    pub(crate) fn hide(&self) -> Result<(), Error> {
        let properties = DWM_THUMBNAIL_PROPERTIES {
            dwFlags: DWM_TNP_VISIBLE,
            fVisible: BOOL(0),
            ..Default::default()
        };
        // SAFETY: The properties value remains valid for the duration of the call and the handle
        // remains registered for the lifetime of this owner.
        unsafe { DwmUpdateThumbnailProperties(self.handle, &properties) }
    }
}

impl Drop for DwmThumbnail {
    fn drop(&mut self) {
        // SAFETY: This owner unregisters its handle exactly once. DWM also tears down a thumbnail
        // when either endpoint disappears, so an error here requires no further recovery.
        if let Err(error) = unsafe { DwmUnregisterThumbnail(self.handle) } {
            log::debug!("DWM thumbnail unregister failed: {error}");
        }
    }
}

pub(crate) fn fit_source_in_bounds(source: SIZE, bounds: RECT) -> RECT {
    let source_width = i64::from(source.cx.max(1));
    let source_height = i64::from(source.cy.max(1));
    let bounds_width = i64::from((bounds.right - bounds.left).max(1));
    let bounds_height = i64::from((bounds.bottom - bounds.top).max(1));

    // Integer division above rounds toward zero, so an extreme aspect ratio (e.g. a source many
    // times wider than it is tall, fitted into a near-square destination) can otherwise round the
    // short dimension down to 0, producing a zero-area destination rect. Clamp to 1 so the
    // thumbnail always occupies at least a sliver rather than disappearing.
    let (width, height) = if source_width * bounds_height > bounds_width * source_height {
        (
            bounds_width,
            ((bounds_width * source_height) / source_width).max(1),
        )
    } else {
        (
            ((bounds_height * source_width) / source_height).max(1),
            bounds_height,
        )
    };
    let left = i64::from(bounds.left) + (bounds_width - width) / 2;
    let top = i64::from(bounds.top) + (bounds_height - height) / 2;
    RECT {
        left: left as i32,
        top: top as i32,
        right: (left + width) as i32,
        bottom: (top + height) as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_source_is_letterboxed_vertically() {
        let fitted = fit_source_in_bounds(
            SIZE { cx: 1600, cy: 900 },
            RECT {
                left: 10,
                top: 20,
                right: 410,
                bottom: 420,
            },
        );

        assert_eq!(
            fitted,
            RECT {
                left: 10,
                top: 107,
                right: 410,
                bottom: 332,
            }
        );
    }

    #[test]
    fn tall_source_is_pillarboxed_horizontally() {
        let fitted = fit_source_in_bounds(
            SIZE { cx: 900, cy: 1600 },
            RECT {
                left: 10,
                top: 20,
                right: 410,
                bottom: 420,
            },
        );

        assert_eq!(
            fitted,
            RECT {
                left: 97,
                top: 20,
                right: 322,
                bottom: 420,
            }
        );
    }

    #[test]
    fn extremely_wide_source_keeps_a_nonzero_height() {
        let fitted = fit_source_in_bounds(
            SIZE { cx: 100_000, cy: 1 },
            RECT {
                left: 0,
                top: 0,
                right: 10,
                bottom: 10,
            },
        );

        assert!(fitted.bottom - fitted.top >= 1, "{fitted:?}");
        assert!(fitted.right - fitted.left >= 1, "{fitted:?}");
    }

    #[test]
    fn extremely_tall_source_keeps_a_nonzero_width() {
        let fitted = fit_source_in_bounds(
            SIZE { cx: 1, cy: 100_000 },
            RECT {
                left: 0,
                top: 0,
                right: 10,
                bottom: 10,
            },
        );

        assert!(fitted.right - fitted.left >= 1, "{fitted:?}");
        assert!(fitted.bottom - fitted.top >= 1, "{fitted:?}");
    }
}
