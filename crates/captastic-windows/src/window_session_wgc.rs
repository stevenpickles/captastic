//! A Windows Graphics Capture session kept open for the length of a window recording (ADR 0013).
//!
//! The still path in `window_capture_wgc` builds a device, a capture item, a frame pool and a
//! session for one frame and tears them down again; that costs tens of milliseconds, which is fine
//! once and far too slow thirty times a second. A recording instead keeps one session open on a
//! thread of its own, and that thread keeps the newest frame the window has delivered for the
//! recording to take at its own pace.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use captastic_core::{CaptureError, CaptureErrorKind, Rect};
use windows::core::{factory, IInspectable};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext};
use windows::Win32::System::Com::CoIncrementMTAUsage;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

use crate::window_capture::{crop_bgra, wgc_visible_frame, RenderDeadline};
use crate::window_capture_wgc::{
    capture_error, create_device, readback_frame, wgc_create_free_threaded_frame_pool,
    wgc_session_is_supported, windows_error, WgcCaptureResources, WinRtApartment,
    FIRST_FRAME_TIMEOUT,
};
use crate::NativeWindowHandle;

const PIXEL_FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;
/// Two buffers, as for stills: the session thread drains the pool to its newest frame each time it
/// wakes, so a deeper pool would only hold frames that are thrown away.
const FRAME_BUFFERS: i32 = 2;
/// How long the session thread waits for a frame before looking again for a stop or a close, so
/// dropping the session is prompt even when the window delivers nothing.
const IDLE_POLL: Duration = Duration::from_millis(50);
/// How long `start` waits for the device, the session and the first frame together. The first
/// frame itself is held to the still path's `FIRST_FRAME_TIMEOUT`; this covers the setup before it.
const START_TIMEOUT: Duration = Duration::from_millis(1500);
/// How long one frame's GPU readback may take.
const READBACK_BUDGET: Duration = Duration::from_millis(500);
/// Consecutive frames that may fail to read back before the session gives up. A resize can fail
/// one or two while the pool catches up with the window's new size.
const FAILURES_TOLERATED: u32 = 10;

/// One frame of the recorded window.
#[derive(Clone, Debug)]
pub struct WindowFrame {
    /// Opaque BGRA, top row first, `width * 4` bytes a row: the window's visible frame composited
    /// over black, so a translucent backdrop records as it would look on a dark wallpaper.
    pub pixels: Arc<[u8]>,
    pub width: u32,
    pub height: u32,
    /// Where the window's visible frame was on the desktop when this frame was read, when that
    /// could be measured.
    pub bounds: Option<Rect>,
    /// Counts the frames the window has delivered. The same number twice is the same frame: the
    /// window has not changed since.
    pub sequence: u64,
}

/// A capture session on one window, open until dropped.
pub struct WgcWindowSession {
    shared: Arc<Shared>,
    wake: mpsc::SyncSender<()>,
    worker: Option<thread::JoinHandle<()>>,
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    closed: AtomicBool,
    latest: Mutex<Option<WindowFrame>>,
    /// Why the session ended early, once it has.
    failure: Mutex<Option<CaptureError>>,
}

impl WgcWindowSession {
    /// Opens a session on `window` and waits for its first frame.
    ///
    /// Fails when Windows Graphics Capture is unsupported, refuses the window, or delivers no
    /// first frame in time, which is the caller's cue to record the window's area instead.
    /// `cursor` asks Windows to draw the pointer into the frames.
    pub fn start(window: NativeWindowHandle, cursor: bool) -> Result<Self, CaptureError> {
        hold_multithreaded_apartment();
        let shared = Arc::new(Shared::default());
        let (wake, woken) = mpsc::sync_channel(1);
        let (ready, started) = mpsc::sync_channel(1);
        let worker = {
            let shared = shared.clone();
            let wake = wake.clone();
            let hwnd = window.raw();
            thread::Builder::new()
                .name("captastic-window-session".to_owned())
                .spawn(move || run(HWND(hwnd), cursor, &shared, wake, &woken, ready))
                .map_err(|error| {
                    capture_error(
                        CaptureErrorKind::NativeFailure,
                        "spawn_window_session",
                        error.to_string(),
                        true,
                        None,
                    )
                })?
        };
        let session = Self {
            shared,
            wake,
            worker: Some(worker),
        };
        match started.recv_timeout(START_TIMEOUT) {
            Ok(Ok(())) => Ok(session),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(capture_error(
                CaptureErrorKind::Timeout,
                "start_window_session",
                format!(
                    "the window session delivered no first frame within {} ms",
                    START_TIMEOUT.as_millis()
                ),
                true,
                None,
            )),
        }
    }

    /// The newest frame the window has delivered.
    ///
    /// Fails once the window has closed or the session has given up; until then a window that has
    /// not changed, or is minimised, keeps returning its last frame.
    pub fn latest(&self) -> Result<WindowFrame, CaptureError> {
        if let Some(error) = lock(&self.shared.failure).clone() {
            return Err(error);
        }
        if self.is_closed() {
            return Err(capture_error(
                CaptureErrorKind::SourceUnavailable,
                "window_session_frame",
                "the recorded window has closed",
                false,
                None,
            ));
        }
        lock(&self.shared.latest).clone().ok_or_else(|| {
            capture_error(
                CaptureErrorKind::SourceUnavailable,
                "window_session_frame",
                "the window session has no frame",
                true,
                None,
            )
        })
    }

    /// Whether the window has closed, which ends a recording of it.
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }
}

impl Drop for WgcWindowSession {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        let _ = self.wake.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Keeps the process's multithreaded apartment alive from the first session on.
///
/// Each session thread joins the apartment and leaves it when the session ends. When the last
/// thread leaves, COM tears the apartment down and may unload the capture DLLs while Windows'
/// own capture threads are still delivering a frame or a close to them, which crashed the process
/// intermittently between sessions. One usage that is never released costs nothing and closes
/// that window for good.
fn hold_multithreaded_apartment() {
    static HELD: OnceLock<()> = OnceLock::new();
    HELD.get_or_init(|| {
        // SAFETY: Takes one usage of the process's multithreaded apartment, deliberately never
        // released; it touches no state of the calling thread.
        if let Err(error) = unsafe { CoIncrementMTAUsage() } {
            log::debug!("could not hold the multithreaded apartment: {error}");
        }
    });
}

type Ready = mpsc::SyncSender<Result<(), CaptureError>>;

/// The session thread: reports the first frame, or why there is none, on `ready`, and a later
/// failure through `shared`.
fn run(
    hwnd: HWND,
    cursor: bool,
    shared: &Arc<Shared>,
    wake: mpsc::SyncSender<()>,
    woken: &mpsc::Receiver<()>,
    ready: Ready,
) {
    // Window geometry must be in physical pixels, and thread DPI awareness is not inherited
    // reliably by new threads; see `capture_window_bounded`.
    // SAFETY: Changes DPI virtualization only for this session's own thread.
    let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let mut ready = Some(ready);
    if let Err(error) = capture(hwnd, cursor, shared, wake, woken, &mut ready) {
        match ready.take() {
            Some(ready) => {
                let _ = ready.send(Err(error));
            }
            None => {
                log::warn!("window session ended early: {error}");
                *lock(&shared.failure) = Some(error);
            }
        }
    }
}

fn capture(
    hwnd: HWND,
    cursor: bool,
    shared: &Arc<Shared>,
    wake: mpsc::SyncSender<()>,
    woken: &mpsc::Receiver<()>,
    ready: &mut Option<Ready>,
) -> Result<(), CaptureError> {
    // Declared first so it is dropped last: every WinRT object below is released inside the
    // apartment that activated it (see the factory note in `window_capture_wgc`).
    let _apartment = WinRtApartment::initialize()?;
    if !wgc_session_is_supported().map_err(|error| windows_error("check_support", error, false))? {
        return Err(capture_error(
            CaptureErrorKind::Unsupported,
            "check_support",
            "Windows Graphics Capture is not supported on this system",
            false,
            None,
        ));
    }
    let (device, context, winrt_device) = create_device()?;
    let item_factory = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .map_err(|error| windows_error("get_item_factory", error, false))?;
    // SAFETY: hwnd names the window the user chose to record. One that has closed since makes
    // CreateForWindow fail rather than misbehave.
    let item: GraphicsCaptureItem = unsafe { item_factory.CreateForWindow(hwnd) }
        .map_err(|error| windows_error("create_item_for_window", error, false))?;
    let closed = {
        let shared = shared.clone();
        let wake = wake.clone();
        TypedEventHandler::<GraphicsCaptureItem, IInspectable>::new(move |_, _| {
            shared.closed.store(true, Ordering::Release);
            let _ = wake.try_send(());
            Ok(())
        })
    };
    let closed_token = item
        .Closed(&closed)
        .map_err(|error| windows_error("subscribe_item_closed", error, true))?;
    let mut size = item
        .Size()
        .map_err(|error| windows_error("get_capture_item_size", error, true))?;
    let pool =
        wgc_create_free_threaded_frame_pool(&winrt_device, PIXEL_FORMAT, FRAME_BUFFERS, size)
            .map_err(|error| windows_error("create_frame_pool", error, true))?;
    let mut resources = WgcCaptureResources {
        pool,
        session: None,
    };
    let session = resources
        .pool
        .CreateCaptureSession(&item)
        .map_err(|error| windows_error("create_capture_session", error, true))?;
    resources.session = Some(session.clone());
    let _ = session.SetIsCursorCaptureEnabled(cursor);
    let _ = session.SetIsBorderRequired(false);
    let arrived =
        TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(move |_, _| {
            let _ = wake.try_send(());
            Ok(())
        });
    let arrived_token = resources
        .pool
        .FrameArrived(&arrived)
        .map_err(|error| windows_error("subscribe_frame_arrived", error, true))?;

    let result = (|| {
        session
            .StartCapture()
            .map_err(|error| windows_error("start_capture", error, true))?;
        let started = Instant::now();
        let mut sequence = 0_u64;
        let mut failures = 0_u32;
        loop {
            if shared.stop.load(Ordering::Acquire) {
                return Ok(());
            }
            // The item's Closed event is not raised for every window that goes away, so the
            // window's existence is checked too, at the same cost as a wake.
            // SAFETY: IsWindow only reads whether the handle still names a window.
            if !unsafe { IsWindow(hwnd) }.as_bool() {
                shared.closed.store(true, Ordering::Release);
            }
            if shared.closed.load(Ordering::Acquire) {
                if ready.is_some() {
                    return Err(capture_error(
                        CaptureErrorKind::SourceUnavailable,
                        "wait_for_first_frame",
                        "the window closed before its first frame",
                        false,
                        None,
                    ));
                }
                return Ok(());
            }
            let _ = woken.recv_timeout(IDLE_POLL);
            let Some(frame) = newest_frame(&resources.pool) else {
                if ready.is_some() && started.elapsed() >= FIRST_FRAME_TIMEOUT {
                    return Err(capture_error(
                        CaptureErrorKind::Timeout,
                        "wait_for_first_frame",
                        format!(
                            "no window frame arrived within {} ms",
                            FIRST_FRAME_TIMEOUT.as_millis()
                        ),
                        true,
                        None,
                    ));
                }
                continue;
            };
            let content = frame
                .ContentSize()
                .map_err(|error| windows_error("get_content_size", error, true))?;
            if content.Width > 0
                && content.Height > 0
                && (content.Width, content.Height) != (size.Width, size.Height)
            {
                // The window was resized: later frames arrive at its new size.
                resources
                    .pool
                    .Recreate(&winrt_device, PIXEL_FORMAT, FRAME_BUFFERS, content)
                    .map_err(|error| windows_error("recreate_frame_pool", error, true))?;
                size = content;
            }
            let read = read_frame(hwnd, &device, &context, &frame);
            let _ = frame.Close();
            match read {
                Ok((pixels, width, height, bounds)) => {
                    failures = 0;
                    sequence += 1;
                    *lock(&shared.latest) = Some(WindowFrame {
                        pixels: pixels.into(),
                        width,
                        height,
                        bounds,
                        sequence,
                    });
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(Ok(()));
                    }
                }
                Err(error) if failures < FAILURES_TOLERATED => {
                    failures += 1;
                    log::debug!("window session skipped a frame: {error}");
                }
                Err(error) => return Err(error),
            }
        }
    })();

    let _ = resources.pool.RemoveFrameArrived(arrived_token);
    let _ = item.RemoveClosed(closed_token);
    result
}

/// The newest frame waiting in the pool, releasing any older ones back to it.
fn newest_frame(pool: &Direct3D11CaptureFramePool) -> Option<Direct3D11CaptureFrame> {
    let mut newest: Option<Direct3D11CaptureFrame> = None;
    // The pool holds at most FRAME_BUFFERS frames; the bound only guards against a pool that
    // keeps producing while it is drained.
    for _ in 0..=FRAME_BUFFERS {
        let Ok(frame) = pool.TryGetNextFrame() else {
            break;
        };
        if let Some(older) = newest.replace(frame) {
            let _ = older.Close();
        }
    }
    newest
}

/// Reads one frame back and crops it to the window's visible frame.
fn read_frame(
    hwnd: HWND,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    frame: &Direct3D11CaptureFrame,
) -> Result<(Vec<u8>, u32, u32, Option<Rect>), CaptureError> {
    let raw = readback_frame(
        device,
        context,
        frame,
        RenderDeadline::starting_now(READBACK_BUDGET),
    )?;
    let surface = Rect {
        x: 0,
        y: 0,
        width: raw.width,
        height: raw.height,
    };
    let (crop, bounds) = wgc_visible_frame(hwnd, raw.width, raw.height);
    let mut pixels = if crop == surface {
        raw.pixels
    } else {
        crop_bgra(&raw.pixels, surface, crop)?
    };
    // WGC's BGRA is premultiplied, so its colour channels already are the window composited over
    // black; making it opaque is all that is left.
    for pixel in pixels.chunks_exact_mut(4) {
        pixel[3] = u8::MAX;
    }
    Ok((pixels, crop.width, crop.height, bounds))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{COLORREF, LPARAM, LRESULT, WPARAM};
    use windows::Win32::Graphics::Gdi::{CreateSolidBrush, HBRUSH};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostMessageW,
        PostQuitMessage, RegisterClassW, SetWindowPos, TranslateMessage, MSG, SWP_NOMOVE,
        SWP_NOZORDER, WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY, WNDCLASSW, WS_OVERLAPPEDWINDOW,
        WS_VISIBLE,
    };

    #[test]
    fn a_window_that_does_not_exist_cannot_start_a_session() {
        // Whatever this machine supports, there is no window to capture, so the caller is told to
        // fall back rather than handed a session that never delivers.
        let started = Instant::now();
        assert!(WgcWindowSession::start(NativeWindowHandle::from_raw(0x7ff0_0001), false).is_err());
        assert!(started.elapsed() < START_TIMEOUT + Duration::from_secs(1));
    }

    /// A window painted one solid colour, run on a thread of its own with its own message loop.
    struct TestWindow {
        hwnd: isize,
        thread: Option<thread::JoinHandle<()>>,
    }

    /// A test window's colour, as GDI takes it (0x00BBGGRR) and as a BGRA frame holds it, and the
    /// window class painted with it.
    struct Paint {
        gdi: u32,
        bgra: [u8; 3],
        class: PCWSTR,
    }

    const RECORDED: Paint = Paint {
        gdi: 0x0028_28C8,
        bgra: [0x28, 0x28, 0xC8],
        class: w!("CaptasticWindowSessionTest"),
    };
    const COVER: Paint = Paint {
        gdi: 0x00C8_7828,
        bgra: [0xC8, 0x78, 0x28],
        class: w!("CaptasticWindowSessionCover"),
    };
    const PAINT_BGRA: [u8; 3] = RECORDED.bgra;

    extern "system" fn test_window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_DESTROY {
            // SAFETY: Ends this test window thread's own message loop.
            unsafe { PostQuitMessage(0) };
            return LRESULT(0);
        }
        // SAFETY: Default handling for a message delivered to this thread's own window.
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    impl TestWindow {
        fn open(width: i32, height: i32) -> Self {
            Self::open_at(&RECORDED, 120, 120, width, height)
        }

        fn open_at(paint: &Paint, x: i32, y: i32, width: i32, height: i32) -> Self {
            let (sender, receiver) = mpsc::channel();
            let (gdi, class) = (paint.gdi, paint.class.0 as usize);
            let thread = thread::spawn(move || {
                let class = PCWSTR(class as *const u16);
                // SAFETY: Registers and creates a plain window owned by this test thread, then
                // pumps its messages until it is destroyed.
                unsafe {
                    let _ =
                        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
                    let instance = GetModuleHandleW(None).expect("module");
                    let window_class = WNDCLASSW {
                        lpfnWndProc: Some(test_window_proc),
                        hInstance: instance.into(),
                        lpszClassName: class,
                        hbrBackground: HBRUSH(CreateSolidBrush(COLORREF(gdi)).0),
                        ..Default::default()
                    };
                    let _ = RegisterClassW(&window_class);
                    let hwnd = CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        class,
                        PCWSTR::null(),
                        WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                        x,
                        y,
                        width,
                        height,
                        None,
                        None,
                        instance,
                        None,
                    );
                    sender.send(hwnd.0).expect("send");
                    let mut message = MSG::default();
                    while GetMessageW(&mut message, None, 0, 0).as_bool() {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
            });
            let hwnd = receiver.recv().expect("window");
            assert_ne!(hwnd, 0, "the test window was not created");
            Self {
                hwnd,
                thread: Some(thread),
            }
        }

        fn handle(&self) -> NativeWindowHandle {
            NativeWindowHandle::from_raw(self.hwnd)
        }

        fn resize(&self, width: i32, height: i32) {
            // SAFETY: Resizes the live test window; the call is marshalled to its own thread.
            unsafe {
                SetWindowPos(
                    HWND(self.hwnd),
                    None,
                    0,
                    0,
                    width,
                    height,
                    SWP_NOMOVE | SWP_NOZORDER,
                )
            }
            .expect("resize");
        }

        fn close(&mut self) {
            // SAFETY: Asks the live test window to close; its own thread destroys it.
            let _ = unsafe { PostMessageW(HWND(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            self.close();
        }
    }

    /// Waits up to two seconds for the session's newest frame to satisfy `ready`.
    fn frame_where(
        session: &WgcWindowSession,
        ready: impl Fn(&WindowFrame) -> bool,
    ) -> Option<WindowFrame> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(frame) = session.latest() {
                if ready(&frame) {
                    return Some(frame);
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        None
    }

    fn centre(frame: &WindowFrame) -> [u8; 4] {
        let at = ((frame.height / 2 * frame.width + frame.width / 2) * 4) as usize;
        frame.pixels[at..at + 4].try_into().expect("pixel")
    }

    #[test]
    #[ignore = "requires an interactive desktop with Windows Graphics Capture"]
    fn a_session_follows_its_window_through_a_resize_until_it_closes() {
        let mut window = TestWindow::open(320, 240);
        thread::sleep(Duration::from_millis(300));
        let session = WgcWindowSession::start(window.handle(), false).expect("session");

        // The window's own pixels, opaque, at about the size it was opened at.
        let first = frame_where(&session, |frame| centre(frame)[..3] == PAINT_BGRA)
            .expect("a frame showing the window's colour");
        assert_eq!(centre(&first)[3], u8::MAX);
        assert!((first.width as i32 - 320).abs() <= 32, "{}", first.width);
        assert!((first.height as i32 - 240).abs() <= 32, "{}", first.height);
        assert!(first.bounds.is_some());

        // Resized, it is recorded at its new size.
        window.resize(480, 360);
        let resized = frame_where(&session, |frame| frame.width > first.width + 100)
            .expect("a frame at the new size");
        assert!(resized.sequence > first.sequence);
        assert_eq!(centre(&resized)[..3], PAINT_BGRA);

        // Closed, the session says so.
        window.close();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !session.is_closed() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(session.is_closed());
        assert!(session.latest().is_err());
    }

    #[test]
    #[ignore = "requires an interactive desktop with Windows Graphics Capture"]
    fn a_covered_window_is_still_recorded_as_itself() {
        let window = TestWindow::open(320, 240);
        thread::sleep(Duration::from_millis(300));
        let session = WgcWindowSession::start(window.handle(), false).expect("session");
        frame_where(&session, |frame| centre(frame)[..3] == PAINT_BGRA)
            .expect("a frame showing the window's colour");
        // Another window, opened later and so in front, over the whole of it.
        let cover = TestWindow::open_at(&COVER, 80, 80, 400, 320);
        thread::sleep(Duration::from_millis(500));
        let covered = session.latest().expect("a frame while covered");
        assert_eq!(centre(&covered)[..3], PAINT_BGRA);
        assert_ne!(centre(&covered)[..3], COVER.bgra);
        drop(cover);
    }

    #[test]
    #[ignore = "requires an interactive desktop with Windows Graphics Capture"]
    fn an_unchanging_window_keeps_its_last_frame() {
        let window = TestWindow::open(240, 180);
        thread::sleep(Duration::from_millis(300));
        let session = WgcWindowSession::start(window.handle(), false).expect("session");
        let first = frame_where(&session, |frame| centre(frame)[..3] == PAINT_BGRA)
            .expect("a frame showing the window's colour");
        thread::sleep(Duration::from_millis(300));
        let later = session.latest().expect("still a frame");
        assert_eq!(later.pixels, first.pixels);
        drop(session);
    }
}
