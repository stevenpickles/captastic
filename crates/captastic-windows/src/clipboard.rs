use std::marker::PhantomData;
use std::mem::size_of;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use captastic_core::{
    encode_frame, CaptureError, CaptureErrorKind, CpuFrame, FrameAlpha, FrameOrigin, PixelEncoding,
    PngEffort,
};
use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::Graphics::Gdi::{BITMAPV5HEADER, BI_RGB, LCS_GM_IMAGES};
#[cfg(test)]
use windows::Win32::System::DataExchange::GetClipboardData;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
#[cfg(test)]
use windows::Win32::System::Memory::GlobalSize;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE,
};

const CF_DIBV5_FORMAT: u32 = 17;
const LCS_SRGB: u32 = 0x7352_4742;
/// The value both retention formats carry to decline. Windows reads a `DWORD`, and zero is the
/// documented "no": omitting the format entirely is how a publisher consents.
const RETENTION_DENIED: u32 = 0;
const OPEN_TIMEOUT: Duration = Duration::from_millis(50);
const MAX_RETRY_DELAY: Duration = Duration::from_millis(5);
const HRESULT_ACCESS_DENIED: i32 = 0x8007_0005_u32 as i32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardPublishReport {
    pub payload_bytes: usize,
    pub png_payload_bytes: usize,
    pub png_encode_ns: u64,
    pub allocation_copy_ns: u64,
    pub open_wait_ns: u64,
    pub open_retries: u32,
    pub publish_ns: u64,
    /// Whether this publish told Windows to keep the capture out of the Win+V history.
    pub history_excluded: bool,
    /// Whether this publish told Windows not to sync the capture to the signed-in account.
    pub cloud_sync_excluded: bool,
}

/// What Windows may keep of a capture beyond the paste it was taken for.
///
/// Both fields default to `false`, which is to say both retention paths are declined, and that is
/// the point of having a type here rather than two bare arguments: the value a caller gets by
/// making no decision is the conservative one. The two mistakes are not symmetrical. A user who
/// wanted Win+V history and does not get it loses a convenience they can switch back on; a user
/// whose screenshot reaches a Microsoft account cannot un-send it, and nothing told them it went.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClipboardRetention {
    /// Whether the capture may be kept in the Win+V clipboard history.
    pub history: bool,
    /// Whether the capture may be synced to the signed-in Microsoft account, and from there to
    /// that account's other machines.
    pub cloud_sync: bool,
}

impl ClipboardRetention {
    /// Declines both retention paths: the capture is for this paste and nothing else.
    pub const PRIVATE: Self = Self {
        history: false,
        cloud_sync: false,
    };
}

pub struct ClipboardPublisher {
    window: ClipboardWindow,
    png_format: u32,
    history_exclusion_format: u32,
    cloud_exclusion_format: u32,
    retention: ClipboardRetention,
    _thread_affine: PhantomData<Rc<()>>,
}

impl ClipboardPublisher {
    pub fn new(retention: ClipboardRetention) -> Result<Self, CaptureError> {
        Ok(Self {
            window: ClipboardWindow::create()?,
            png_format: register_format(w!("PNG"), "register_png_clipboard_format")?,
            // Both names are registered whether or not this publisher will use them. Registration
            // is one session-wide atom per name, returned again on every later call, so there is
            // nothing to save by deciding here — and the decision then lives in exactly one place.
            history_exclusion_format: register_format(
                w!("CanIncludeInClipboardHistory"),
                "register_clipboard_history_format",
            )?,
            cloud_exclusion_format: register_format(
                w!("CanUploadToCloudClipboard"),
                "register_cloud_clipboard_format",
            )?,
            retention,
            _thread_affine: PhantomData,
        })
    }

    /// Allocates a `DWORD` of zero for each retention path this publisher declines.
    ///
    /// Allocated with the payload, before the clipboard is opened, for the same reason everything
    /// else here is: an allocation that fails must not be able to cost the user their clipboard.
    fn retention_exclusions(&self) -> Result<Vec<(u32, GlobalMemory)>, CaptureError> {
        let denied = RETENTION_DENIED.to_ne_bytes();
        [
            (
                self.history_exclusion_format,
                self.retention.history,
                "allocate_history_exclusion",
            ),
            (
                self.cloud_exclusion_format,
                self.retention.cloud_sync,
                "allocate_cloud_exclusion",
            ),
        ]
        .into_iter()
        .filter(|(_, permitted, _)| !permitted)
        .map(|(format, _, operation)| {
            GlobalMemory::from_bytes(&denied, operation).map(|memory| (format, memory))
        })
        .collect()
    }

    /// Commits a prepared payload to the clipboard.
    ///
    /// Safe to call again after a retryable failure: every handle it allocates is either
    /// transferred to the clipboard or freed before returning.
    ///
    /// A failure reports whether it left the clipboard empty. Win32 requires `EmptyClipboard`
    /// before `SetClipboardData` — emptying is how a process takes ownership — so there is a
    /// moment where the user's previous contents are gone and the replacement has not landed. It
    /// cannot be closed, only made as small as possible and, when it does bite, admitted to.
    pub fn publish(
        &mut self,
        payload: &ClipboardPayload<'_>,
    ) -> Result<ClipboardPublishReport, ClipboardPublishError> {
        let publish_started = Instant::now();
        let layout = &payload.layout;
        let copy_started = Instant::now();
        // Every allocation happens before the clipboard is opened, let alone emptied, so the
        // failures most likely to occur cannot cost the user what they had copied.
        let dib_memory = GlobalMemory::from_frame(payload.frame, layout).map_err(intact)?;
        let exclusions = self.retention_exclusions().map_err(intact)?;
        let png_payload_bytes = payload.png.as_ref().map_or(0, Vec::len);
        let png_memory = payload
            .png
            .as_deref()
            .map(|bytes| GlobalMemory::from_bytes(bytes, "allocate_png"))
            .transpose()
            .map_err(intact)?;
        let allocation_copy_ns = duration_ns(copy_started.elapsed());
        let (clipboard, open_retries, open_wait_ns) =
            ClipboardSession::open(self.window.hwnd, OPEN_TIMEOUT).map_err(intact)?;
        // SAFETY: This thread owns the successfully opened clipboard session.
        unsafe { EmptyClipboard() }
            .map_err(|error| intact(clipboard_error("empty_clipboard", error, true)))?;
        // Past this point the user's previous clipboard contents are gone.
        //
        // The retention markers go on before the pixels, so there is no ordering in which the
        // capture is on the clipboard without them. A marker that cannot be set fails the publish
        // rather than falling back: publishing anyway would quietly deliver the retention the user
        // configured against, which is the one outcome this whole mechanism exists to prevent.
        for (format, memory) in exclusions {
            memory.transfer_to_clipboard(format).map_err(cleared)?;
        }
        dib_memory
            .transfer_to_clipboard(CF_DIBV5_FORMAT)
            .map_err(cleared)?;
        let png_payload_bytes = if let Some(png_memory) = png_memory {
            match png_memory.transfer_to_clipboard(self.png_format) {
                Ok(()) => png_payload_bytes,
                Err(error) => {
                    log::warn!(
                        "DIBV5 clipboard publication succeeded but optional PNG publication failed: {error}"
                    );
                    0
                }
            }
        } else {
            0
        };
        drop(clipboard);
        Ok(ClipboardPublishReport {
            payload_bytes: layout.payload_bytes,
            png_payload_bytes,
            // Carried from `prepare`: the one-time encode, not this attempt's share of it.
            png_encode_ns: payload.png_encode_ns,
            allocation_copy_ns,
            open_wait_ns,
            open_retries,
            // This attempt only. A retried publish reports the attempt that succeeded rather than
            // the sum of the ones that did not.
            publish_ns: duration_ns(publish_started.elapsed()),
            history_excluded: !self.retention.history,
            cloud_sync_excluded: !self.retention.cloud_sync,
        })
    }
}

/// A failed publish, and whether it cost the user what was on the clipboard before.
///
/// The distinction is the whole point: a publish that fails before `EmptyClipboard` is a capture
/// the user did not get, while one that fails after it is a capture they did not get *and* a
/// clipboard they no longer have. Only the second is worth interrupting them about.
#[derive(Clone, Debug)]
pub struct ClipboardPublishError {
    pub error: CaptureError,
    /// True when this attempt emptied the clipboard and then failed to replace its contents.
    pub cleared_previous_contents: bool,
}

impl std::fmt::Display for ClipboardPublishError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.error)?;
        if self.cleared_previous_contents {
            formatter.write_str(" (the previous clipboard contents were cleared)")?;
        }
        Ok(())
    }
}

impl std::error::Error for ClipboardPublishError {}

impl From<ClipboardPublishError> for CaptureError {
    fn from(failure: ClipboardPublishError) -> Self {
        failure.error
    }
}

/// A failure that left the clipboard as it found it.
fn intact(error: CaptureError) -> ClipboardPublishError {
    ClipboardPublishError {
        error,
        cleared_previous_contents: false,
    }
}

/// A failure that happened after the clipboard had been emptied.
fn cleared(error: CaptureError) -> ClipboardPublishError {
    ClipboardPublishError {
        error,
        cleared_previous_contents: true,
    }
}

/// A frame encoded and ready to commit, independent of any one publish attempt.
pub struct ClipboardPayload<'a> {
    frame: &'a CpuFrame,
    layout: DibV5Layout,
    png: Option<Vec<u8>>,
    png_encode_ns: u64,
}

impl<'a> ClipboardPayload<'a> {
    /// Encodes a frame once, ahead of any publish attempt.
    ///
    /// Encoding is the expensive half of a clipboard publish and produces identical bytes every
    /// time, so a retry has no reason to repeat it. Only the cheap half — the global allocations
    /// the clipboard takes ownership of, and the session itself — is per-attempt work. Preparing
    /// deliberately needs no publisher: nothing here touches the clipboard, so a caller cannot
    /// accidentally hold a session open across an encode.
    pub fn prepare(frame: &'a CpuFrame) -> Result<Self, CaptureError> {
        let layout = DibV5Layout::new(frame)?;
        let png_started = Instant::now();
        // `Fast` rather than the default: this runs in the hotkey path, where the user feels the
        // milliseconds, and even Fast compresses a screenshot roughly 20-45x. The file-output
        // worker, which is not in that path, uses `Compact`.
        let png = (frame.alpha() == FrameAlpha::Straight)
            .then(|| encode_frame(frame, PngEffort::Fast))
            .transpose()
            .map_err(png_error)?;
        let png_encode_ns = png
            .as_ref()
            .map_or(0, |_| duration_ns(png_started.elapsed()));
        Ok(Self {
            frame,
            layout,
            png,
            png_encode_ns,
        })
    }

    /// The encoded PNG this payload will publish alongside the DIBV5, if any.
    pub fn png_bytes(&self) -> Option<&[u8]> {
        self.png.as_deref()
    }
}

struct ClipboardWindow {
    hwnd: HWND,
}

impl ClipboardWindow {
    fn create() -> Result<Self, CaptureError> {
        // SAFETY: STATIC is a system window class. The message-only parent keeps the zero-sized
        // owner window hidden, and all optional handles/pointers are intentionally absent.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("Captastic Clipboard"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                None,
                None,
            )
        };
        if hwnd.0 == 0 {
            return Err(last_error("create_clipboard_window", false));
        }
        Ok(Self { hwnd })
    }
}

impl Drop for ClipboardWindow {
    fn drop(&mut self) {
        // SAFETY: hwnd is a live window created and destroyed on this publisher's worker thread.
        let _ = unsafe { DestroyWindow(self.hwnd) };
    }
}

struct ClipboardSession;

impl ClipboardSession {
    fn open(owner: HWND, timeout: Duration) -> Result<(Self, u32, u64), CaptureError> {
        let started = Instant::now();
        let mut retries = 0_u32;
        let mut delay = Duration::from_millis(1);
        loop {
            // SAFETY: owner is the live hidden window owned by this worker thread.
            match unsafe { OpenClipboard(owner) } {
                Ok(()) => return Ok((Self, retries, duration_ns(started.elapsed()))),
                Err(error)
                    if error.code().0 == HRESULT_ACCESS_DENIED && started.elapsed() < timeout =>
                {
                    retries = retries.saturating_add(1);
                    thread::sleep(delay);
                    delay = delay.saturating_mul(2).min(MAX_RETRY_DELAY);
                }
                Err(error) if error.code().0 == HRESULT_ACCESS_DENIED => {
                    return Err(CaptureError {
                        kind: CaptureErrorKind::Timeout,
                        backend: "windows-clipboard",
                        operation: "open_clipboard",
                        message: format!(
                            "clipboard remained busy for {:.3} ms: {error}",
                            started.elapsed().as_secs_f64() * 1_000.0
                        ),
                        retryable: true,
                        native_code: Some(i64::from(error.code().0)),
                    });
                }
                Err(error) => return Err(clipboard_error("open_clipboard", error, false)),
            }
        }
    }
}

impl Drop for ClipboardSession {
    fn drop(&mut self) {
        // SAFETY: This guard exists only for one successful OpenClipboard call on this thread.
        let _ = unsafe { CloseClipboard() };
    }
}

struct GlobalMemory {
    handle: HGLOBAL,
    transferred: bool,
}

impl GlobalMemory {
    fn from_frame(frame: &CpuFrame, layout: &DibV5Layout) -> Result<Self, CaptureError> {
        // SAFETY: payload_bytes is nonzero and validated by DibV5Layout.
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, layout.payload_bytes) }
            .map_err(|error| clipboard_error("allocate_dibv5", error, false))?;
        let memory = Self {
            handle,
            transferred: false,
        };
        // SAFETY: handle is a live movable allocation owned by memory. A successful lock returns
        // payload_bytes writable bytes until the matching GlobalUnlock below.
        let pointer = unsafe { GlobalLock(memory.handle) };
        if pointer.is_null() {
            return Err(last_error("lock_dibv5", false));
        }
        // SAFETY: GlobalAlloc reserved exactly payload_bytes, and the allocation remains locked.
        let destination =
            unsafe { std::slice::from_raw_parts_mut(pointer.cast::<u8>(), layout.payload_bytes) };
        let write_result = layout.write(frame, destination);
        // SAFETY: Balances the successful GlobalLock. A false return is also the documented
        // success result when the lock count reaches zero, so no Result interpretation is used.
        let _ = unsafe { GlobalUnlock(memory.handle) };
        write_result?;
        Ok(memory)
    }

    fn transfer_to_clipboard(mut self, format: u32) -> Result<(), CaptureError> {
        // SAFETY: The clipboard is open and empty on this thread. handle is a GMEM_MOVEABLE block
        // that remains owned by this guard unless SetClipboardData succeeds.
        unsafe { SetClipboardData(format, HANDLE(self.handle.0 as isize)) }
            .map_err(|error| clipboard_error("set_clipboard_data", error, true))?;
        self.transferred = true;
        Ok(())
    }

    fn from_bytes(bytes: &[u8], operation: &'static str) -> Result<Self, CaptureError> {
        if bytes.is_empty() {
            return Err(invalid_frame("clipboard payload must not be empty"));
        }
        // SAFETY: bytes is nonempty and its length is the requested allocation size.
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }
            .map_err(|error| clipboard_error(operation, error, false))?;
        let memory = Self {
            handle,
            transferred: false,
        };
        // SAFETY: handle is a live movable allocation owned by memory.
        let pointer = unsafe { GlobalLock(memory.handle) };
        if pointer.is_null() {
            return Err(last_error("lock_clipboard_payload", false));
        }
        // SAFETY: The allocation contains bytes.len() writable bytes and does not overlap bytes.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast(), bytes.len()) };
        // SAFETY: Balances the successful GlobalLock call above.
        let _ = unsafe { GlobalUnlock(memory.handle) };
        Ok(memory)
    }
}

impl Drop for GlobalMemory {
    fn drop(&mut self) {
        if !self.transferred {
            // SAFETY: Ownership was not transferred to the clipboard, so this guard still owns
            // the live allocation. The return value is irrelevant after the free attempt.
            let _ = unsafe { GlobalFree(self.handle) };
        }
    }
}

#[derive(Debug)]
struct DibV5Layout {
    header: BITMAPV5HEADER,
    tight_stride: usize,
    payload_bytes: usize,
}

impl DibV5Layout {
    fn new(frame: &CpuFrame) -> Result<Self, CaptureError> {
        // A DIBV5 payload is 8-bit BGRA and nothing else. Matched on the encoding rather than
        // tested for inequality so that a pixel of another width cannot reach the row copy below
        // by simply not being the one format this happened to name.
        match frame.format().encoding() {
            PixelEncoding::EightBitRgba { blue_first: true } => {}
            PixelEncoding::EightBitRgba { blue_first: false } => {
                return Err(unsupported(
                    "DIBV5 publication requires BGRA8 pixels, and this frame is RGBA8",
                ))
            }
            // No BITMAPV5HEADER compression describes half-float samples, so there is nothing to
            // publish this as. Narrowing it to 8 bits first would be tone mapping done by the
            // clipboard, which is the wrong place for it even once Captastic can do it at all.
            PixelEncoding::HalfFloatRgba => {
                return Err(unsupported(
                    "DIBV5 publication requires 8-bit pixels, and this frame is half-float",
                ))
            }
        }
        if frame.origin != FrameOrigin::TopLeft {
            return Err(unsupported("DIBV5 publication requires top-left pixels"));
        }
        let width = i32::try_from(frame.width())
            .map_err(|_| invalid_frame("clipboard width exceeds the DIBV5 limit"))?;
        let height = i32::try_from(frame.height())
            .map_err(|_| invalid_frame("clipboard height exceeds the DIBV5 limit"))?;
        if width == 0 || height == 0 {
            return Err(invalid_frame("clipboard frame dimensions must be nonzero"));
        }
        let tight_stride = usize::try_from(frame.width())
            .ok()
            .and_then(|value| value.checked_mul(4))
            .ok_or_else(|| invalid_frame("clipboard row size overflowed"))?;
        if (frame.stride_bytes() as usize) < tight_stride {
            return Err(invalid_frame("clipboard source stride is too small"));
        }
        let pixel_bytes = tight_stride
            .checked_mul(frame.height() as usize)
            .ok_or_else(|| invalid_frame("clipboard pixel size overflowed"))?;
        if frame.pixels().len()
            < (frame.stride_bytes() as usize).saturating_mul(frame.height() as usize)
        {
            return Err(invalid_frame("clipboard source buffer is truncated"));
        }
        let payload_bytes = size_of::<BITMAPV5HEADER>()
            .checked_add(pixel_bytes)
            .ok_or_else(|| invalid_frame("clipboard payload size overflowed"))?;
        let size_image = u32::try_from(pixel_bytes)
            .map_err(|_| invalid_frame("clipboard image exceeds the DIBV5 size limit"))?;
        let header = BITMAPV5HEADER {
            bV5Size: size_of::<BITMAPV5HEADER>() as u32,
            bV5Width: width,
            bV5Height: -height,
            bV5Planes: 1,
            bV5BitCount: 32,
            // BI_RGB, not BI_BITFIELDS: the channel layout below is exactly the BI_RGB default
            // for 32bpp DIBs (B in the low byte, R in the high byte), so BI_BITFIELDS would add
            // no information while inviting the well-known ambiguity among DIB consumers about
            // whether pixel data starts immediately after the header or after an appended
            // BITFIELDS mask triple. bV5RedMask/bV5GreenMask/bV5BlueMask are therefore
            // informative only; BI_RGB readers ignore them and assume this same default layout.
            // bV5AlphaMask is left set for V5-aware consumers that read alpha out of a
            // BITMAPV5HEADER regardless of bV5Compression (e.g. via bV5AlphaMask being nonzero
            // signals straight alpha is present in the fourth byte of each pixel).
            bV5Compression: BI_RGB,
            bV5SizeImage: size_image,
            bV5RedMask: 0x00ff_0000,
            bV5GreenMask: 0x0000_ff00,
            bV5BlueMask: 0x0000_00ff,
            bV5AlphaMask: if frame.alpha() == FrameAlpha::Straight {
                0xff00_0000
            } else {
                0
            },
            bV5CSType: LCS_SRGB,
            bV5Intent: LCS_GM_IMAGES as u32,
            ..Default::default()
        };
        Ok(Self {
            header,
            tight_stride,
            payload_bytes,
        })
    }

    fn write(&self, frame: &CpuFrame, destination: &mut [u8]) -> Result<(), CaptureError> {
        if destination.len() != self.payload_bytes {
            return Err(invalid_frame("DIBV5 destination has the wrong size"));
        }
        let header_bytes = size_of::<BITMAPV5HEADER>();
        // SAFETY: BITMAPV5HEADER is a fully initialized repr(C) Copy type. destination contains
        // header_bytes writable bytes, and the source/destination do not overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(
                (&self.header as *const BITMAPV5HEADER).cast::<u8>(),
                destination.as_mut_ptr(),
                header_bytes,
            )
        };
        let source_stride = frame.stride_bytes() as usize;
        let pixels = &mut destination[header_bytes..];
        for row in 0..frame.height() as usize {
            let source_start = row * source_stride;
            let destination_start = row * self.tight_stride;
            pixels[destination_start..destination_start + self.tight_stride]
                .copy_from_slice(&frame.pixels()[source_start..source_start + self.tight_stride]);
        }
        Ok(())
    }
}

/// Translates an encoder failure into the clipboard backend's error vocabulary.
///
/// A frame the encoder rejects is a frame this publisher built its DIBV5 from moments earlier, so
/// the layout is already known good; anything reaching here is a programming error rather than
/// something a retry would clear.
fn png_error(error: captastic_core::PngError) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::InvalidFrame,
        backend: "windows-clipboard",
        operation: "encode_png",
        message: error.to_string(),
        retryable: false,
        native_code: None,
    }
}

fn clipboard_error(
    operation: &'static str,
    error: windows::core::Error,
    retryable: bool,
) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::NativeFailure,
        backend: "windows-clipboard",
        operation,
        message: error.to_string(),
        retryable,
        native_code: Some(i64::from(error.code().0)),
    }
}

/// Registers one clipboard format name, naming the one that failed rather than "a format".
fn register_format(
    name: windows::core::PCWSTR,
    operation: &'static str,
) -> Result<u32, CaptureError> {
    // SAFETY: name is a static null-terminated wide string.
    let format = unsafe { RegisterClipboardFormatW(name) };
    if format == 0 {
        return Err(last_error(operation, false));
    }
    Ok(format)
}

fn last_error(operation: &'static str, retryable: bool) -> CaptureError {
    clipboard_error(operation, windows::core::Error::from_win32(), retryable)
}

fn unsupported(message: impl Into<String>) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::Unsupported,
        backend: "windows-clipboard",
        operation: "build_dibv5",
        message: message.into(),
        retryable: false,
        native_code: None,
    }
}

fn invalid_frame(message: impl Into<String>) -> CaptureError {
    CaptureError {
        kind: CaptureErrorKind::InvalidFrame,
        backend: "windows-clipboard",
        operation: "build_dibv5",
        message: message.into(),
        retryable: false,
        native_code: None,
    }
}

fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// Reads one clipboard format back, or reports that it is not on the clipboard at all.
///
/// `Ok(None)` is the interesting answer for the retention formats: their absence is what consent
/// looks like, so a test has to be able to tell "not published" from "published as zero".
#[cfg(test)]
fn read_current_format(owner: HWND, format: u32) -> Result<Option<Vec<u8>>, CaptureError> {
    let (clipboard, _, _) = ClipboardSession::open(owner, OPEN_TIMEOUT)?;
    // SAFETY: The clipboard is open on this thread.
    let available =
        unsafe { windows::Win32::System::DataExchange::IsClipboardFormatAvailable(format).is_ok() };
    if !available {
        drop(clipboard);
        return Ok(None);
    }
    // SAFETY: The clipboard is open on this thread. The returned handle remains clipboard-owned.
    let handle = unsafe { GetClipboardData(format) }
        .map_err(|error| clipboard_error("get_clipboard_data", error, true))?;
    let global = HGLOBAL(handle.0 as *mut std::ffi::c_void);
    // SAFETY: global refers to the clipboard-owned DIBV5 allocation while the clipboard is open.
    let size = unsafe { GlobalSize(global) };
    if size == 0 {
        return Err(last_error("get_clipboard_size", true));
    }
    // SAFETY: The clipboard handle names a movable global allocation.
    let pointer = unsafe { GlobalLock(global) };
    if pointer.is_null() {
        return Err(last_error("lock_clipboard_data", true));
    }
    // SAFETY: GlobalSize reported size readable bytes, valid through the matching unlock.
    let payload = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), size) }.to_vec();
    // SAFETY: Balances the successful GlobalLock without interpreting the ambiguous false result.
    let _ = unsafe { GlobalUnlock(global) };
    drop(clipboard);
    Ok(Some(payload))
}

#[cfg(test)]
fn read_current_dibv5(owner: HWND) -> Result<Vec<u8>, CaptureError> {
    read_current_format(owner, CF_DIBV5_FORMAT)?
        .ok_or_else(|| invalid_frame("no DIBV5 payload is on the clipboard"))
}

#[cfg(test)]
mod tests {
    use captastic_core::{
        CaptureId, CaptureMode, ColorSpace, DisplayId, FrameMetadata, PixelFormat, Rect,
        TimingProvenance,
    };
    use std::sync::Arc;

    use super::*;

    fn frame(width: u32, height: u32, stride: u32, pixels: Vec<u8>) -> CpuFrame {
        CpuFrame::new(
            Arc::from(pixels),
            width,
            height,
            stride,
            PixelFormat::Bgra8Unorm,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            FrameMetadata {
                capture_id: CaptureId(1),
                backend: "test".to_owned(),
                display_id: DisplayId::primary(),
                source_rect: Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
                rotation_degrees: 0,
                capture_mode: CaptureMode::Latest { max_age_ms: None },
                presentation_offset_ns: Some(0),
                timing_provenance: TimingProvenance::Synthetic,
                native_ready_offset_ns: 1,
                cpu_ready_offset_ns: Some(2),
                frame_age_ns: Some(0),
                verified_current_offset_ns: None,
                frame_generation: Some(1),
                copy_count: 1,
                pool_slot: None,
                cursor: None,
            },
        )
        .expect("valid fixture")
    }

    /// The fallible half of `frame`, for the layouts `CpuFrame` refuses to build.
    fn build_frame(
        width: u32,
        height: u32,
        stride_bytes: u32,
        pixels: Vec<u8>,
    ) -> Result<CpuFrame, captastic_core::FrameError> {
        CpuFrame::new(
            Arc::from(pixels),
            width,
            height,
            stride_bytes,
            PixelFormat::Bgra8Unorm,
            FrameOrigin::TopLeft,
            ColorSpace::Srgb,
            frame(1, 1, 4, vec![0; 4]).metadata.clone(),
        )
    }

    #[test]
    fn dibv5_layout_is_top_down_bgra_and_removes_padding() {
        let frame = frame(
            2,
            2,
            12,
            vec![
                1, 2, 3, 4, 5, 6, 7, 8, 90, 91, 92, 93, 9, 10, 11, 12, 13, 14, 15, 16, 94, 95, 96,
                97,
            ],
        );
        let layout = DibV5Layout::new(&frame).expect("valid layout");
        assert_eq!(size_of::<BITMAPV5HEADER>(), 124);
        assert_eq!(layout.payload_bytes, 140);
        assert_eq!(layout.header.bV5Width, 2);
        assert_eq!(layout.header.bV5Height, -2);
        assert_eq!(layout.header.bV5AlphaMask, 0);
        let mut payload = vec![0_u8; layout.payload_bytes];
        layout.write(&frame, &mut payload).expect("payload");
        assert_eq!(
            &payload[124..],
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
    }

    /// This used to build a valid frame and then swap in a shorter buffer, to reach the layout's
    /// own truncation check. `CpuFrame` no longer permits that, so the rejection is asserted where
    /// a caller now meets it - at construction. The check in `DibV5Layout::new` stays: it guards
    /// an unchecked slice of `stride * height` bytes, and one comparison per capture is a cheap
    /// price for not trusting a future constructor to have validated anything.
    #[test]
    fn a_truncated_source_cannot_become_a_frame() {
        assert!(matches!(
            build_frame(1, 1, 4, vec![1, 2, 3]),
            Err(captastic_core::FrameError::BufferTooShort {
                actual: 3,
                required: 4
            })
        ));
    }

    #[test]
    fn dibv5_refuses_half_float_pixels() {
        let frame = CpuFrame::new(
            Arc::from(vec![0_u8; 8]),
            1,
            1,
            8,
            PixelFormat::Rgba16Float,
            FrameOrigin::TopLeft,
            ColorSpace::ScRgb,
            frame(1, 1, 4, vec![0; 4]).metadata.clone(),
        )
        .expect("a valid half-float frame");

        let error = DibV5Layout::new(&frame).expect_err("half-float frame");
        assert_eq!(error.kind, CaptureErrorKind::Unsupported);
        assert!(
            error.message.contains("half-float"),
            "the refusal should say what was wrong with the frame: {}",
            error.message
        );
    }

    #[test]
    fn dibv5_advertises_deliberate_window_alpha() {
        let frame = frame(1, 1, 4, vec![10, 20, 30, 64]).with_alpha(FrameAlpha::Straight);
        let layout = DibV5Layout::new(&frame).expect("valid alpha layout");
        assert_eq!(layout.header.bV5AlphaMask, 0xff00_0000);
        let mut payload = vec![0_u8; layout.payload_bytes];
        layout.write(&frame, &mut payload).expect("alpha payload");
        assert_eq!(&payload[124..], &[10, 20, 30, 64]);
    }

    #[test]
    fn report_fields_are_stable() {
        let report = ClipboardPublishReport {
            payload_bytes: 124,
            png_payload_bytes: 68,
            png_encode_ns: 1,
            allocation_copy_ns: 1,
            open_wait_ns: 2,
            open_retries: 3,
            publish_ns: 4,
            history_excluded: true,
            cloud_sync_excluded: true,
        };
        assert_eq!(report.open_retries, 3);
    }

    #[test]
    fn retention_declines_both_paths_unless_a_caller_says_otherwise() {
        // The default is the whole safety property: a caller that forgets to decide gets the
        // answer whose consequences can be undone. Asserted rather than assumed, because a later
        // derive or field addition could flip it silently.
        assert_eq!(ClipboardRetention::default(), ClipboardRetention::PRIVATE);
        assert!(!ClipboardRetention::default().history);
        assert!(!ClipboardRetention::default().cloud_sync);
    }

    #[test]
    fn png_clipboard_payload_decodes_to_the_published_pixels() {
        let frame =
            frame(2, 1, 8, vec![10, 20, 30, 64, 40, 50, 60, 255]).with_alpha(FrameAlpha::Straight);
        let png = encode_frame(&frame, PngEffort::Fast).expect("PNG payload");

        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let decoder = png::Decoder::new(std::io::Cursor::new(&png));
        let mut reader = decoder.read_info().expect("valid PNG header");
        let mut buffer = vec![0; reader.output_buffer_size().expect("bounded buffer")];
        let info = reader.next_frame(&mut buffer).expect("valid PNG data");

        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!((info.width, info.height), (2, 1));
        // BGRA in, RGBA out, alpha preserved.
        assert_eq!(
            &buffer[..info.buffer_size()],
            &[30, 20, 10, 64, 60, 50, 40, 255]
        );
    }

    #[test]
    fn preparing_encodes_once_and_yields_reusable_bytes() {
        // The retry loop publishes the same payload repeatedly, so the encode must be complete
        // and self-contained before the first attempt rather than repeated inside each one.
        let frame =
            frame(2, 1, 8, vec![10, 20, 30, 64, 40, 50, 60, 255]).with_alpha(FrameAlpha::Straight);
        let payload = ClipboardPayload::prepare(&frame).expect("prepare");

        let bytes = payload.png_bytes().expect("straight alpha publishes a PNG");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        // Two reads of the same prepared payload see identical bytes at the same address: the
        // payload owns its encode rather than producing one on demand.
        assert_eq!(
            payload.png_bytes().expect("still present").as_ptr(),
            bytes.as_ptr()
        );
        assert!(payload.png_encode_ns > 0);
    }

    #[test]
    fn opaque_frames_prepare_without_a_png_payload() {
        // Opaque captures go to the clipboard as DIBV5 only, so there is nothing to encode and
        // the reported encode cost must stay zero rather than counting an absent payload.
        let frame = frame(2, 1, 8, vec![10, 20, 30, 255, 40, 50, 60, 255]);
        let payload = ClipboardPayload::prepare(&frame).expect("prepare");

        assert!(payload.png_bytes().is_none());
        assert_eq!(payload.png_encode_ns, 0);
    }

    #[test]
    fn png_clipboard_payload_compresses_rather_than_storing() {
        // The regression that retired the hand-rolled encoder: it emitted stored (uncompressed)
        // DEFLATE, so its output was always *larger* than the raw pixels it was given.
        let width = 512_u32;
        let height = 512_u32;
        let frame = frame(
            width,
            height,
            width * 4,
            vec![0x40; (width * height * 4) as usize],
        )
        .with_alpha(FrameAlpha::Straight);

        let png = encode_frame(&frame, PngEffort::Fast).expect("PNG payload");

        let raw_bytes = (width * height * 4) as usize;
        assert!(
            png.len() * 10 < raw_bytes,
            "a uniform {width}x{height} frame encoded to {} bytes against {raw_bytes} raw",
            png.len()
        );
    }

    #[test]
    #[ignore = "mutates the interactive Windows clipboard"]
    fn native_clipboard_round_trip_preserves_dibv5_payload() {
        let frame = frame(
            2,
            2,
            8,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        );
        let layout = DibV5Layout::new(&frame).expect("layout");
        let mut expected = vec![0_u8; layout.payload_bytes];
        layout.write(&frame, &mut expected).expect("expected DIBV5");
        let mut publisher =
            ClipboardPublisher::new(ClipboardRetention::PRIVATE).expect("publisher");
        let payload = ClipboardPayload::prepare(&frame).expect("prepare");
        publisher.publish(&payload).expect("publish");
        let actual = read_current_dibv5(publisher.window.hwnd).expect("clipboard readback");
        assert_eq!(actual, expected);
    }

    /// Confirms the retention markers reach the real clipboard, as the `DWORD` Windows reads.
    ///
    /// The unit tests above can only check what this code decided to do. Whether Windows sees a
    /// four-byte zero under the two registered names is a fact about the clipboard, and the only
    /// place to learn it is the clipboard.
    ///
    /// cargo test --locked -p captastic-windows -- --ignored --nocapture retention
    #[test]
    #[ignore = "mutates the interactive Windows clipboard"]
    fn declined_retention_publishes_a_zero_dword_under_both_names() {
        let frame = frame(2, 2, 8, vec![7; 16]);
        let mut publisher =
            ClipboardPublisher::new(ClipboardRetention::PRIVATE).expect("publisher");
        let payload = ClipboardPayload::prepare(&frame).expect("prepare");
        let report = publisher.publish(&payload).expect("publish");

        assert!(report.history_excluded);
        assert!(report.cloud_sync_excluded);
        for (name, format) in [
            (
                "CanIncludeInClipboardHistory",
                publisher.history_exclusion_format,
            ),
            (
                "CanUploadToCloudClipboard",
                publisher.cloud_exclusion_format,
            ),
        ] {
            let value = read_current_format(publisher.window.hwnd, format)
                .expect("clipboard readback")
                .unwrap_or_else(|| panic!("{name} was not published"));
            assert_eq!(value, RETENTION_DENIED.to_ne_bytes(), "{name} value");
        }
        // The capture itself still published: declining retention must not cost the paste.
        assert!(read_current_dibv5(publisher.window.hwnd).is_ok());
    }

    /// The other half of the contract: consent is the *absence* of the format, not a nonzero value.
    #[test]
    #[ignore = "mutates the interactive Windows clipboard"]
    fn permitted_retention_publishes_neither_name() {
        let frame = frame(2, 2, 8, vec![7; 16]);
        let retention = ClipboardRetention {
            history: true,
            cloud_sync: true,
        };
        let mut publisher = ClipboardPublisher::new(retention).expect("publisher");
        let payload = ClipboardPayload::prepare(&frame).expect("prepare");
        let report = publisher.publish(&payload).expect("publish");

        assert!(!report.history_excluded);
        assert!(!report.cloud_sync_excluded);
        for (name, format) in [
            (
                "CanIncludeInClipboardHistory",
                publisher.history_exclusion_format,
            ),
            (
                "CanUploadToCloudClipboard",
                publisher.cloud_exclusion_format,
            ),
        ] {
            assert!(
                read_current_format(publisher.window.hwnd, format)
                    .expect("clipboard readback")
                    .is_none(),
                "{name} was published even though retention was permitted"
            );
        }
    }

    /// Reports what each encoder effort costs, and buys, on a real captured frame.
    ///
    /// This is the harness that retired the hand-rolled stored-DEFLATE encoder — on this content
    /// it ran at ~19.4 ms/MP and emitted slightly *more* bytes than the raw pixels, against
    /// ~2-3 ms/MP and a 20-45x reduction for `Fast`. It survives that decision as the check on the
    /// one that replaced it: `Fast` is on the hotkey path, so its cost is a latency budget, and
    /// switching the clipboard to `Compact` would be visible here as a 5-6x regression.
    ///
    /// A window is the subject rather than the whole desktop because desktop duplication only
    /// yields on change and an idle machine never produces one; the pixels are the same kind of
    /// screenshot content either way. Rates are per megapixel so a larger capture extrapolates.
    ///
    /// CAPTASTIC_TEST_WINDOW_HANDLE=<hwnd> cargo test --locked -p captastic-windows --release
    ///     -- --ignored --nocapture png_encoder_latency_and_size_on_a_real_capture
    #[test]
    #[ignore = "requires CAPTASTIC_TEST_WINDOW_HANDLE naming a live interactive window"]
    fn png_encoder_latency_and_size_on_a_real_capture() {
        use std::time::Instant;

        let raw = std::env::var("CAPTASTIC_TEST_WINDOW_HANDLE")
            .expect("set CAPTASTIC_TEST_WINDOW_HANDLE")
            .parse::<isize>()
            .expect("numeric window handle");
        let reference = frame(2, 2, 8, vec![0; 16]);
        let captured = crate::window_capture::capture_window_visual(
            crate::NativeWindowHandle::from_raw(raw),
            &reference.metadata,
        )
        .expect("window capture")
        .frame;
        let opaque = captured.clone().with_alpha(FrameAlpha::Opaque);
        let megapixels = f64::from(captured.width()) * f64::from(captured.height()) / 1_000_000.0;
        let raw_bytes = captured.required_bytes();

        // One warm-up plus three timed runs each; the best run is the least noisy estimate on a
        // machine that is also running a desktop.
        let best = |mut run: Box<dyn FnMut() -> usize>| {
            run();
            (0..3)
                .map(|_| {
                    let started = Instant::now();
                    let bytes = run();
                    (started.elapsed(), bytes)
                })
                .min_by_key(|(elapsed, _)| *elapsed)
                .expect("at least one run")
        };
        let measure = |frame: CpuFrame, effort: PngEffort| {
            best(Box::new(move || {
                encode_frame(&frame, effort).expect("png encode").len()
            }))
        };

        let results = [
            (
                "Fast, straight alpha -> RGBA (clipboard)",
                measure(captured.clone(), PngEffort::Fast),
            ),
            (
                "Compact, straight alpha -> RGBA",
                measure(captured.clone(), PngEffort::Compact),
            ),
            (
                "Compact, opaque -> RGB (file output)",
                measure(opaque, PngEffort::Compact),
            ),
        ];

        println!(
            "captured frame {}x{} ({megapixels:.2} MP, {:.1} MiB raw)",
            captured.width(),
            captured.height(),
            raw_bytes as f64 / (1024.0 * 1024.0),
        );
        for (label, (elapsed, bytes)) in results {
            let ms = elapsed.as_secs_f64() * 1000.0;
            println!(
                "  {label:36} {ms:8.1} ms ({:6.1} ms/MP)  {:8.2} MiB  ({:.3}x of raw)",
                ms / megapixels,
                bytes as f64 / (1024.0 * 1024.0),
                bytes as f64 / raw_bytes as f64,
            );
        }
    }
}
