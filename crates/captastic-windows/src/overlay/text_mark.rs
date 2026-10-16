//! Typed text, rasterized for the editor (ADR 0010, Milestone 6 slice 4).
//!
//! The editor model paints text as a [`GlyphMask`] and knows nothing of fonts; this module makes
//! the mask. GDI draws the text white on black with grayscale antialiasing - never ClearType, whose
//! coloured fringes are tuned to one display's subpixels and would be baked into an image meant
//! for anywhere - and the green channel is the coverage. The size is in the capture's own pixels,
//! so text is as large in the published image as it looked on the display it came from.

use captastic_core::{CaptureError, GlyphMask};
use windows::core::w;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, GdiFlush, GetTextExtentPoint32W, GetTextMetricsW, SelectObject,
    SetBkMode, SetTextColor, TextOutW, ANTIALIASED_QUALITY, DEFAULT_CHARSET, DEFAULT_PITCH,
    FW_SEMIBOLD, HDC, HFONT, TEXTMETRICW, TRANSPARENT,
};

use super::desktop_ui::desktop_error;
use super::raster::rgb;
use super::shell::FrozenSurface;

/// Typed text laid out and rasterized, with where the caret goes after it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct TextLayout {
    /// Nothing when there is nothing to show: no text yet, or only line breaks.
    pub(super) mask: Option<GlyphMask>,
    /// Where the mask's top-left pixel goes, relative to the first line's top-left.
    pub(super) mask_offset: (i32, i32),
    /// The caret's top, relative to the first line's top-left, and its height.
    pub(super) caret: (i32, i32, i32),
    /// The height of a line, which places the first line around the point that was clicked.
    pub(super) line_height: i32,
}

/// Lays `text` out in lines at `size` pixels to the em, and rasterizes it.
pub(super) fn layout_text(text: &str, size: i32) -> Result<TextLayout, CaptureError> {
    let size = size.max(1);
    let measure = FrozenSurface::empty(1, 1)?;
    let font = Font::new(size)?;
    let lines: Vec<Vec<u16>> = text
        .split('\n')
        .map(|line| line.encode_utf16().collect())
        .collect();
    let (line_height, widths) = font.measure(measure.device, &lines);
    // Room either side for strokes that overhang their advance, as italics and some semibold
    // glyphs do, so no letter is clipped.
    let pad = (size / 4).max(2);
    let last = widths.last().copied().unwrap_or(0);
    let caret = (last, line_height * (lines.len() as i32 - 1), line_height);
    let widest = widths.iter().copied().max().unwrap_or(0);
    if widest == 0 {
        return Ok(TextLayout {
            mask: None,
            mask_offset: (-pad, 0),
            caret,
            line_height,
        });
    }
    let (width, height) = (widest + pad * 2, line_height * lines.len() as i32);
    let surface = FrozenSurface::empty(width as u32, height as u32)?;
    surface.clear();
    font.draw(surface.device, &lines, pad, line_height);
    // SAFETY: Completes GDI's drawing before its pixels are read.
    let _ = unsafe { GdiFlush() };
    // SAFETY: The surface's DIB holds exactly byte_length bytes, owned by this thread, and GDI
    // has finished writing them.
    let pixels = unsafe { std::slice::from_raw_parts(surface.bits, surface.byte_length) };
    let coverage = pixels.chunks_exact(4).map(|pixel| pixel[1]).collect();
    Ok(TextLayout {
        mask: GlyphMask::new(width as u32, height as u32, coverage),
        mask_offset: (-pad, 0),
        caret,
        line_height,
    })
}

/// The annotation face, owned for one layout.
struct Font(HFONT);

impl Font {
    fn new(size: i32) -> Result<Self, CaptureError> {
        // SAFETY: Creates a font from a face every supported Windows has; nothing is borrowed.
        let font = unsafe {
            CreateFontW(
                -size,
                0,
                0,
                0,
                FW_SEMIBOLD.0 as i32,
                0,
                0,
                0,
                u32::from(DEFAULT_CHARSET.0),
                0,
                0,
                u32::from(ANTIALIASED_QUALITY.0),
                u32::from(DEFAULT_PITCH.0),
                w!("Segoe UI"),
            )
        };
        if font.is_invalid() {
            return Err(desktop_error(
                "create_text_font",
                windows::core::Error::from_win32().to_string(),
            ));
        }
        Ok(Self(font))
    }

    /// The line height, and each line's width.
    fn measure(&self, device: HDC, lines: &[Vec<u16>]) -> (i32, Vec<i32>) {
        // SAFETY: device is a live memory DC and the font is live; the old font is put back.
        unsafe {
            let previous = SelectObject(device, self.0);
            let mut metrics = TEXTMETRICW::default();
            let line_height = if GetTextMetricsW(device, &mut metrics).as_bool() {
                metrics.tmHeight
            } else {
                0
            };
            let widths = lines
                .iter()
                .map(|line| {
                    let mut size = SIZE::default();
                    if line.is_empty() || !GetTextExtentPoint32W(device, line, &mut size).as_bool()
                    {
                        0
                    } else {
                        size.cx
                    }
                })
                .collect();
            SelectObject(device, previous);
            (line_height.max(1), widths)
        }
    }

    fn draw(&self, device: HDC, lines: &[Vec<u16>], left: i32, line_height: i32) {
        // SAFETY: device is a live memory DC with a DIB selected and the font is live; the old
        // font is put back.
        unsafe {
            let previous = SelectObject(device, self.0);
            SetBkMode(device, TRANSPARENT);
            SetTextColor(device, rgb(255, 255, 255));
            for (index, line) in lines.iter().enumerate() {
                if !line.is_empty() {
                    let _ = TextOutW(device, left, line_height * index as i32, line);
                }
            }
            SelectObject(device, previous);
        }
    }
}

impl Drop for Font {
    fn drop(&mut self) {
        // SAFETY: The font was created here and is selected into no DC by now.
        unsafe { DeleteObject(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_rasterized_as_grey_coverage_with_room_around_it() {
        let layout = layout_text("Wi", 24).expect("layout");
        let mask = layout.mask.expect("a mask");
        assert!(!mask.is_blank());
        assert_eq!(mask.height() as i32, layout.line_height);
        // Some pixels fully covered, some in between: antialiased, not a bitmap font.
        assert!(mask.coverage().contains(&255));
        assert!(mask
            .coverage()
            .iter()
            .any(|value| (40..=215).contains(value)));
        // The padding columns at either side hold nothing.
        let width = mask.width() as usize;
        assert!(mask
            .coverage()
            .chunks_exact(width)
            .all(|row| row[0] == 0 && row[width - 1] == 0));
        // The caret follows the last letter on the first line.
        assert_eq!(layout.caret.1, 0);
        assert!(layout.caret.0 > 0 && layout.caret.0 < width as i32);
    }

    #[test]
    fn lines_stack_and_the_caret_follows_the_last_one() {
        let one = layout_text("Hello", 20).expect("layout");
        let two = layout_text("Hello\nyou", 20).expect("layout");
        let (one_mask, two_mask) = (one.mask.expect("mask"), two.mask.expect("mask"));
        assert_eq!(two_mask.height(), one_mask.height() * 2);
        assert_eq!(two.caret.1, two.line_height);
        assert!(two.caret.0 < one.caret.0);
        // A trailing line break moves the caret to a new, empty line.
        let broken = layout_text("Hello\n", 20).expect("layout");
        assert_eq!((broken.caret.0, broken.caret.1), (0, broken.line_height));
    }

    #[test]
    fn nothing_typed_has_no_mask_but_still_a_caret() {
        let layout = layout_text("", 20).expect("layout");
        assert!(layout.mask.is_none());
        assert_eq!((layout.caret.0, layout.caret.1), (0, 0));
        assert!(layout.caret.2 >= 20);
        // Larger text, taller lines.
        assert!(layout_text("", 40).expect("layout").line_height > layout.line_height);
    }
}
