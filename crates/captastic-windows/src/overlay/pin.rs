//! Pinned captures: always-on-top windows that keep a confirmed capture on screen (ADR 0010).
//!
//! Pins live on the `captastic-desktop-ui` thread, which `desktop_ui` owns. A pin shows its capture
//! at the capture's physical pixel size, at the place it was captured, inside a thin accent border
//! that is what tells it apart from the screen it is a copy of. It can be dragged anywhere, made
//! translucent with the mouse wheel or its menu, made click-through, and closed with Escape or its
//! menu. A click-through pin cannot be clicked to undo that, which is why the notification area
//! can make every pin clickable again.

use captastic_core::{CaptureError, CpuFrame, FrameAlpha, PixelFormat, Rect};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{GetDC, ReleaseDC, AC_SRC_ALPHA, BLENDFUNCTION};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GetCursorPos, GetWindowLongPtrW, PostMessageW, SetForegroundWindow, SetWindowLongPtrW,
    ShowWindow, TrackPopupMenu, UpdateLayeredWindow, GWLP_USERDATA, GWL_EXSTYLE, HTCAPTION,
    MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING, SW_SHOWNOACTIVATE, TPM_RIGHTBUTTON, ULW_ALPHA,
    WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_KEYDOWN, WM_MOUSEWHEEL, WM_NCDESTROY, WM_NCHITTEST,
    WM_NCLBUTTONDBLCLK, WM_NCRBUTTONDOWN, WM_NCRBUTTONUP, WM_NULL, WS_EX_LAYERED, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use super::desktop_ui::{
    desktop_error, forget_window, host_window, release_window_state, window_state, HOST_CLOSE_PINS,
};
use super::shell::{query_display_environment, FrozenSurface};

pub(super) const PIN_CLASS_NAME: PCWSTR = w!("CaptasticPinnedCapture-v1");
const PIN_WINDOW_NAME: PCWSTR = w!("Captastic pinned capture");

const BORDER_DIP: i32 = 2;
/// The accent the overlay draws its selection in, so a pin reads as Captastic's.
const BORDER_BGRA: [u8; 4] = [0xF6, 0x7D, 0x2D, 0xFF];
const OPACITY_LEVELS: [u8; 5] = [255, 204, 153, 102, 51];

const MENU_CLICK_THROUGH: usize = 1;
const MENU_CLOSE: usize = 2;
const MENU_CLOSE_ALL: usize = 3;
const MENU_OPACITY_BASE: usize = 100;

/// A pin's own state, owned by its window.
struct PinState {
    surface: FrozenSurface,
    /// Where the window's top-left corner was placed, used for the first presentation only;
    /// afterwards the window moves itself and is presented without a position.
    origin: POINT,
    opacity: u8,
    click_through: bool,
}

pub(super) fn create_pin(
    instance: HINSTANCE,
    frame: &CpuFrame,
    at: Rect,
) -> Result<HWND, CaptureError> {
    if frame.format() != PixelFormat::Bgra8Unorm {
        return Err(desktop_error(
            "pin_frame_format",
            format!("cannot pin a {:?} frame", frame.format()),
        ));
    }
    let border = query_display_environment(at).metrics.px(BORDER_DIP).max(1);
    let surface = bordered_surface(frame, border)?;
    let origin = POINT {
        x: at.x - border,
        y: at.y - border,
    };
    let state = Box::new(PinState {
        surface,
        origin,
        opacity: OPACITY_LEVELS[0],
        click_through: false,
    });
    let (width, height) = (state.surface.width, state.surface.height);
    let state_pointer = Box::into_raw(state);
    // SAFETY: A layered top-level window of a class registered on this thread; the state pointer
    // is handed to WM_NCCREATE and reclaimed in WM_NCDESTROY.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PIN_CLASS_NAME,
            PIN_WINDOW_NAME,
            WS_POPUP,
            origin.x,
            origin.y,
            width,
            height,
            None,
            None,
            instance,
            Some(state_pointer.cast()),
        )
    };
    if hwnd.0 == 0 {
        // SAFETY: Creation failed before WM_NCCREATE could take ownership of the state.
        drop(unsafe { Box::from_raw(state_pointer) });
        return Err(desktop_error(
            "create_pin_window",
            windows::core::Error::from_win32().to_string(),
        ));
    }
    // SAFETY: The window was just created on this thread with this state.
    if let Err(error) = present(hwnd, unsafe { &*state_pointer }, true) {
        // SAFETY: hwnd is the window created above.
        let _ = unsafe { DestroyWindow(hwnd) };
        return Err(error);
    }
    // SAFETY: hwnd is live. Not activated: pinning should not take focus from what the user was
    // doing, and clicking the pin activates it when they want it.
    unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
    Ok(hwnd)
}

/// The capture inside an opaque accent border, premultiplied for `UpdateLayeredWindow`.
fn bordered_surface(frame: &CpuFrame, border: i32) -> Result<FrozenSurface, CaptureError> {
    let (width, height) = (frame.width() as usize, frame.height() as usize);
    let border = border as usize;
    let outer_width = width + border * 2;
    let outer_height = height + border * 2;
    let surface = FrozenSurface::empty(outer_width as u32, outer_height as u32)?;
    // SAFETY: The surface's DIB holds exactly byte_length writable bytes, owned by this thread.
    let target = unsafe { std::slice::from_raw_parts_mut(surface.bits, surface.byte_length) };
    compose_bordered(
        target,
        frame.pixels(),
        frame.stride_bytes() as usize,
        width,
        height,
        border,
        frame.alpha(),
    );
    Ok(surface)
}

/// Writes `source` (BGRA, `stride` bytes per row) into `target` (tight BGRA, `border` pixels wider
/// on every side), surrounded by the accent border. Opaque sources are forced opaque, because a
/// desktop capture's alpha byte is not meaningful; straight-alpha window captures are
/// premultiplied, as `UpdateLayeredWindow` requires.
fn compose_bordered(
    target: &mut [u8],
    source: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    border: usize,
    alpha: FrameAlpha,
) {
    let outer_width = width + border * 2;
    for pixel in target.chunks_exact_mut(4) {
        pixel.copy_from_slice(&BORDER_BGRA);
    }
    for row in 0..height {
        let source_row = &source[row * stride..row * stride + width * 4];
        let target_start = ((row + border) * outer_width + border) * 4;
        let target_row = &mut target[target_start..target_start + width * 4];
        for (to, from) in target_row
            .chunks_exact_mut(4)
            .zip(source_row.chunks_exact(4))
        {
            match alpha {
                FrameAlpha::Opaque => {
                    to[..3].copy_from_slice(&from[..3]);
                    to[3] = u8::MAX;
                }
                FrameAlpha::Straight => {
                    let a = u32::from(from[3]);
                    for channel in 0..3 {
                        to[channel] = ((u32::from(from[channel]) * a + 127) / 255) as u8;
                    }
                    to[3] = from[3];
                }
            }
        }
    }
}

fn present(hwnd: HWND, state: &PinState, place: bool) -> Result<(), CaptureError> {
    let size = SIZE {
        cx: state.surface.width,
        cy: state.surface.height,
    };
    let source = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: 0,
        BlendFlags: 0,
        SourceConstantAlpha: state.opacity,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    // SAFETY: A null HWND obtains the screen DC, released below on this thread.
    let screen = unsafe { GetDC(None) };
    // SAFETY: hwnd is this thread's live layered window, and every pointer argument outlives the
    // call. Without a destination the window keeps the position it has moved itself to.
    let result = unsafe {
        UpdateLayeredWindow(
            hwnd,
            screen,
            place.then_some(&state.origin as *const POINT),
            Some(&size),
            state.surface.device,
            Some(&source),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
    };
    // SAFETY: Balances the GetDC above.
    unsafe { ReleaseDC(None, screen) };
    result.map_err(|error| desktop_error("present_pin", error.to_string()))
}

pub(super) fn set_click_through(hwnd: HWND, click_through: bool) {
    let state_pointer = {
        // SAFETY: Reads the pointer installed in WM_NCCREATE on this thread.
        unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PinState }
    };
    if state_pointer.is_null() {
        return;
    }
    // SAFETY: The pin's state lives until WM_NCDESTROY on this thread; the borrow ends here.
    unsafe { (*state_pointer).click_through = click_through };
    // SAFETY: hwnd is a live pin on this thread.
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    let transparent = WS_EX_TRANSPARENT.0 as isize;
    let style = if click_through {
        style | transparent
    } else {
        style & !transparent
    };
    // SAFETY: As above; only the transparency bit changes.
    unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style) };
}

pub(super) unsafe extern "system" fn pin_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state_pointer = window_state::<PinState>(hwnd, message, lparam);
    if state_pointer.is_null() {
        // SAFETY: Forwards a message that arrived before WM_NCCREATE or after WM_NCDESTROY.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    match message {
        // The whole pin is its own drag handle.
        WM_NCHITTEST => LRESULT(HTCAPTION as isize),
        // A double-click on a caption asks to maximize; a pin has nothing to maximize into.
        WM_NCLBUTTONDBLCLK => LRESULT(0),
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xffff) as u16 as i16;
            // SAFETY: The pin's state lives until WM_NCDESTROY on this thread.
            let current = unsafe { (*state_pointer).opacity };
            let next = step_opacity(current, delta > 0);
            if next != current {
                set_opacity(hwnd, state_pointer, next);
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 == usize::from(VK_ESCAPE.0) => {
            // SAFETY: hwnd is this thread's live pin.
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        // The whole pin answers as a caption, so a right-click arrives as a non-client click. Left
        // to DefWindowProc, the press starts the caption's own system-menu tracking - a pin has
        // no system menu - and the release never becomes WM_CONTEXTMENU, so the menu is shown from
        // the release here, and the press is swallowed so that tracking never starts.
        WM_NCRBUTTONDOWN => LRESULT(0),
        WM_NCRBUTTONUP => {
            show_pin_menu(hwnd, state_pointer);
            LRESULT(0)
        }
        // Still reached from the keyboard: the menu key, or Shift+F10.
        WM_CONTEXTMENU => {
            show_pin_menu(hwnd, state_pointer);
            LRESULT(0)
        }
        WM_COMMAND => {
            let command = wparam.0 & 0xffff;
            match command {
                MENU_CLICK_THROUGH => set_click_through(hwnd, true),
                MENU_CLOSE => {
                    // SAFETY: hwnd is this thread's live pin.
                    let _ = unsafe { DestroyWindow(hwnd) };
                }
                MENU_CLOSE_ALL => {
                    // Routed through the host so the list of pins stays its own.
                    if let Some(host) = host_window() {
                        // SAFETY: Posting to this thread's own host window.
                        let _ =
                            unsafe { PostMessageW(host, HOST_CLOSE_PINS, WPARAM(0), LPARAM(0)) };
                    }
                }
                level
                    if level >= MENU_OPACITY_BASE
                        && level < MENU_OPACITY_BASE + OPACITY_LEVELS.len() =>
                {
                    set_opacity(
                        hwnd,
                        state_pointer,
                        OPACITY_LEVELS[level - MENU_OPACITY_BASE],
                    );
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            forget_window(hwnd);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            drop(release_window_state::<PinState>(hwnd));
            LRESULT(0)
        }
        // SAFETY: Forwards an unhandled message with the original arguments.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

/// The next opacity level in the direction asked for, stopping at either end.
fn step_opacity(current: u8, more_opaque: bool) -> u8 {
    let index = OPACITY_LEVELS
        .iter()
        .position(|level| *level <= current)
        .unwrap_or(OPACITY_LEVELS.len() - 1);
    let next = if more_opaque {
        index.saturating_sub(1)
    } else {
        (index + 1).min(OPACITY_LEVELS.len() - 1)
    };
    OPACITY_LEVELS[next]
}

fn set_opacity(hwnd: HWND, state_pointer: *mut PinState, opacity: u8) {
    // SAFETY: The pin's state lives until WM_NCDESTROY on this thread; the borrow ends with the
    // presentation, which dispatches nothing.
    let state = unsafe { &mut *state_pointer };
    state.opacity = opacity;
    if let Err(error) = present(hwnd, state, false) {
        log::warn!("could not change a pinned capture's opacity: {error}");
    }
}

fn show_pin_menu(hwnd: HWND, state_pointer: *mut PinState) {
    // SAFETY: The pin's state lives until WM_NCDESTROY on this thread; copied out so no borrow is
    // held across the modal menu.
    let (opacity, click_through) =
        unsafe { ((*state_pointer).opacity, (*state_pointer).click_through) };
    let result = (|| -> windows::core::Result<()> {
        // SAFETY: Creates menus owned by this function and destroyed below.
        let menu = unsafe { CreatePopupMenu() }?;
        // SAFETY: As above.
        let opacity_menu = unsafe { CreatePopupMenu() }?;
        let built = (|| -> windows::core::Result<()> {
            let labels = [w!("100%"), w!("80%"), w!("60%"), w!("40%"), w!("20%")];
            for (index, label) in labels.into_iter().enumerate() {
                let flags = if OPACITY_LEVELS[index] == opacity {
                    MF_STRING | MF_CHECKED
                } else {
                    MF_STRING
                };
                // SAFETY: opacity_menu is live and the label is a static wide string.
                unsafe { AppendMenuW(opacity_menu, flags, MENU_OPACITY_BASE + index, label) }?;
            }
            // SAFETY: Ownership of opacity_menu passes to menu, which destroys it with itself.
            unsafe {
                AppendMenuW(
                    menu,
                    MF_STRING | MF_POPUP,
                    opacity_menu.0 as usize,
                    w!("Opacity"),
                )
            }?;
            let click_through_flags = if click_through {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            };
            // SAFETY: menu is live and every label is a static wide string.
            unsafe {
                AppendMenuW(
                    menu,
                    click_through_flags,
                    MENU_CLICK_THROUGH,
                    w!("Click Through"),
                )
            }?;
            // SAFETY: A separator ignores its command and text.
            unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null()) }?;
            // SAFETY: As above.
            unsafe { AppendMenuW(menu, MF_STRING, MENU_CLOSE, w!("Close")) }?;
            // SAFETY: As above.
            unsafe { AppendMenuW(menu, MF_STRING, MENU_CLOSE_ALL, w!("Close All Pins")) }?;
            let mut point = POINT::default();
            // SAFETY: point is writable storage.
            unsafe { GetCursorPos(&mut point) }?;
            // SAFETY: Lets the menu dismiss when the user clicks elsewhere.
            unsafe { SetForegroundWindow(hwnd) };
            // SAFETY: menu and hwnd stay live through this modal call; the chosen item arrives as
            // WM_COMMAND.
            unsafe { TrackPopupMenu(menu, TPM_RIGHTBUTTON, point.x, point.y, 0, hwnd, None) }?;
            // SAFETY: The documented follow-up that makes the next menu dismiss correctly.
            unsafe { PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0)) }?;
            Ok(())
        })();
        // SAFETY: Destroys menu and, as its submenu, opacity_menu; neither is used again. A submenu
        // that never got attached is destroyed on its own.
        unsafe {
            let _ = DestroyMenu(menu);
            if built.is_err() {
                let _ = DestroyMenu(opacity_menu);
            }
        }
        built
    })();
    if let Err(error) = result {
        log::warn!("could not show a pinned capture's menu: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opacity_steps_stop_at_either_end() {
        assert_eq!(step_opacity(255, true), 255);
        assert_eq!(step_opacity(255, false), 204);
        assert_eq!(step_opacity(51, false), 51);
        assert_eq!(step_opacity(51, true), 102);
        // A value between levels steps from the level at or below it.
        assert_eq!(step_opacity(180, true), 204);
    }

    #[test]
    fn a_pin_is_its_capture_inside_an_opaque_border() {
        // Two pixels wide and one high with a padded stride, inside a one-pixel border.
        let source = [10, 20, 30, 0, 40, 50, 60, 0, 99, 99, 99, 99];
        let mut target = vec![0_u8; 4 * 3 * 4];
        compose_bordered(&mut target, &source, 12, 2, 1, 1, FrameAlpha::Opaque);
        let pixel = |x: usize, y: usize| target[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4].to_vec();
        assert_eq!(pixel(0, 0), BORDER_BGRA);
        assert_eq!(pixel(3, 2), BORDER_BGRA);
        // A desktop capture's alpha byte is meaningless, so it is forced opaque.
        assert_eq!(pixel(1, 1), [10, 20, 30, 255]);
        assert_eq!(pixel(2, 1), [40, 50, 60, 255]);
    }

    #[test]
    fn a_window_capture_with_alpha_is_premultiplied() {
        let source = [200, 100, 50, 128];
        let mut target = vec![0_u8; 4 * 3 * 3];
        compose_bordered(&mut target, &source, 4, 1, 1, 1, FrameAlpha::Straight);
        assert_eq!(target[(3 + 1) * 4..(3 + 1) * 4 + 4], [100, 50, 25, 128]);
    }
}
