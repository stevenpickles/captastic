use std::marker::PhantomData;
use std::mem::size_of;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use captastic_core::{
    CaptureError, CaptureErrorKind, CpuFrame, FrameAlpha, FrameOrigin, PixelFormat,
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
}

pub struct ClipboardPublisher {
    window: ClipboardWindow,
    png_format: u32,
    _thread_affine: PhantomData<Rc<()>>,
}

impl ClipboardPublisher {
    pub fn new() -> Result<Self, CaptureError> {
        // SAFETY: The registered clipboard-format name is a static null-terminated string.
        let png_format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
        if png_format == 0 {
            return Err(last_error("register_png_clipboard_format", false));
        }
        Ok(Self {
            window: ClipboardWindow::create()?,
            png_format,
            _thread_affine: PhantomData,
        })
    }

    pub fn publish(&mut self, frame: &CpuFrame) -> Result<ClipboardPublishReport, CaptureError> {
        let publish_started = Instant::now();
        let layout = DibV5Layout::new(frame)?;
        let copy_started = Instant::now();
        let dib_memory = GlobalMemory::from_frame(frame, &layout)?;
        let png_started = Instant::now();
        let png = (frame.alpha == FrameAlpha::Straight)
            .then(|| encode_png(frame))
            .transpose()?;
        let png_encode_ns = png
            .as_ref()
            .map_or(0, |_| duration_ns(png_started.elapsed()));
        let png_payload_bytes = png.as_ref().map_or(0, Vec::len);
        let png_memory = png
            .as_deref()
            .map(|bytes| GlobalMemory::from_bytes(bytes, "allocate_png"))
            .transpose()?;
        let allocation_copy_ns = duration_ns(copy_started.elapsed());
        let (clipboard, open_retries, open_wait_ns) =
            ClipboardSession::open(self.window.hwnd, OPEN_TIMEOUT)?;
        // SAFETY: This thread owns the successfully opened clipboard session.
        unsafe { EmptyClipboard() }
            .map_err(|error| clipboard_error("empty_clipboard", error, true))?;
        dib_memory.transfer_to_clipboard(CF_DIBV5_FORMAT)?;
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
            png_encode_ns,
            allocation_copy_ns,
            open_wait_ns,
            open_retries,
            publish_ns: duration_ns(publish_started.elapsed()),
        })
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
        if frame.format != PixelFormat::Bgra8Unorm {
            return Err(unsupported("DIBV5 publication requires BGRA8 pixels"));
        }
        if frame.origin != FrameOrigin::TopLeft {
            return Err(unsupported("DIBV5 publication requires top-left pixels"));
        }
        let width = i32::try_from(frame.width)
            .map_err(|_| invalid_frame("clipboard width exceeds the DIBV5 limit"))?;
        let height = i32::try_from(frame.height)
            .map_err(|_| invalid_frame("clipboard height exceeds the DIBV5 limit"))?;
        if width == 0 || height == 0 {
            return Err(invalid_frame("clipboard frame dimensions must be nonzero"));
        }
        let tight_stride = usize::try_from(frame.width)
            .ok()
            .and_then(|value| value.checked_mul(4))
            .ok_or_else(|| invalid_frame("clipboard row size overflowed"))?;
        if (frame.stride_bytes as usize) < tight_stride {
            return Err(invalid_frame("clipboard source stride is too small"));
        }
        let pixel_bytes = tight_stride
            .checked_mul(frame.height as usize)
            .ok_or_else(|| invalid_frame("clipboard pixel size overflowed"))?;
        if frame.pixels.len() < (frame.stride_bytes as usize).saturating_mul(frame.height as usize)
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
            bV5AlphaMask: if frame.alpha == FrameAlpha::Straight {
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
        let source_stride = frame.stride_bytes as usize;
        let pixels = &mut destination[header_bytes..];
        for row in 0..frame.height as usize {
            let source_start = row * source_stride;
            let destination_start = row * self.tight_stride;
            pixels[destination_start..destination_start + self.tight_stride]
                .copy_from_slice(&frame.pixels[source_start..source_start + self.tight_stride]);
        }
        Ok(())
    }
}

// The maximum payload of a single DEFLATE stored (uncompressed) block: the block's LEN field
// is a u16, so a block can never hold more than u16::MAX bytes. `push_stored_byte` and
// `flush_stored_block` both key off this constant directly rather than a `Vec`'s `capacity()`,
// since `Vec::with_capacity` is permitted to over-allocate and comparing against the live
// capacity would silently let a block grow past the u16::MAX the LEN field can express.
const STORED_BLOCK_LIMIT: usize = u16::MAX as usize;

fn encode_png(frame: &CpuFrame) -> Result<Vec<u8>, CaptureError> {
    if frame.format != PixelFormat::Bgra8Unorm || frame.origin != FrameOrigin::TopLeft {
        return Err(unsupported(
            "PNG clipboard publication requires top-left BGRA8 pixels",
        ));
    }
    let row_bytes = usize::try_from(frame.width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or_else(|| invalid_frame("PNG row size overflowed"))?;
    if (frame.stride_bytes as usize) < row_bytes {
        return Err(invalid_frame("PNG source stride is too small"));
    }
    let required = (frame.stride_bytes as usize)
        .checked_mul(frame.height as usize)
        .ok_or_else(|| invalid_frame("PNG source size overflowed"))?;
    if frame.pixels.len() < required {
        return Err(invalid_frame("PNG source buffer is truncated"));
    }
    let raw_length = row_bytes
        .checked_add(1)
        .and_then(|row| row.checked_mul(frame.height as usize))
        .ok_or_else(|| invalid_frame("PNG scanline buffer overflowed"))?;
    // A terminal block is always emitted, including when the raw byte count ends on a boundary.
    let block_count = raw_length / STORED_BLOCK_LIMIT + 1;
    let compressed_length = raw_length
        .checked_add(block_count.saturating_mul(5))
        .and_then(|length| length.checked_add(6))
        .ok_or_else(|| invalid_frame("PNG DEFLATE stream size overflowed"))?;
    let idat_length = u32::try_from(compressed_length)
        .map_err(|_| invalid_frame("PNG IDAT exceeds the format size limit"))?;
    let mut png = Vec::with_capacity(compressed_length.saturating_add(57));
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut ihdr = [0_u8; 13];
    ihdr[0..4].copy_from_slice(&frame.width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&frame.height.to_be_bytes());
    ihdr[8] = 8;
    ihdr[9] = 6; // RGBA.
    write_png_chunk(&mut png, b"IHDR", &ihdr)?;
    png.extend_from_slice(&idat_length.to_be_bytes());
    let idat_crc_start = png.len();
    png.extend_from_slice(b"IDAT");
    let idat_payload_start = png.len();
    png.extend_from_slice(&[0x78, 0x01]);
    let mut block = Vec::with_capacity(STORED_BLOCK_LIMIT);
    let mut adler = Adler32::new();
    for row in 0..frame.height as usize {
        push_stored_byte(&mut png, &mut block, 0, &mut adler);
        let start = row * frame.stride_bytes as usize;
        for pixel in frame.pixels[start..start + row_bytes].chunks_exact(4) {
            for byte in [pixel[2], pixel[1], pixel[0], pixel[3]] {
                push_stored_byte(&mut png, &mut block, byte, &mut adler);
            }
        }
    }
    flush_stored_block(&mut png, &mut block, true);
    png.extend_from_slice(&adler.finish().to_be_bytes());
    debug_assert_eq!(png.len() - idat_payload_start, compressed_length);
    let idat_crc = crc32(&png[idat_crc_start..]);
    png.extend_from_slice(&idat_crc.to_be_bytes());
    write_png_chunk(&mut png, b"IEND", &[])?;
    Ok(png)
}

#[inline]
fn push_stored_byte(destination: &mut Vec<u8>, block: &mut Vec<u8>, byte: u8, adler: &mut Adler32) {
    adler.update(byte);
    block.push(byte);
    // Compare against the fixed STORED_BLOCK_LIMIT, not `block.capacity()`: `Vec::with_capacity`
    // is only a lower bound on the allocation, so a capacity-based check could let `block` grow
    // past u16::MAX bytes before flushing, which `flush_stored_block` cannot express in the
    // DEFLATE stored-block LEN field.
    if block.len() == STORED_BLOCK_LIMIT {
        flush_stored_block(destination, block, false);
    }
}

fn flush_stored_block(destination: &mut Vec<u8>, block: &mut Vec<u8>, final_block: bool) {
    assert!(
        block.len() <= STORED_BLOCK_LIMIT,
        "DEFLATE stored block exceeds the u16 LEN field limit"
    );
    destination.push(u8::from(final_block));
    let length = block.len() as u16;
    destination.extend_from_slice(&length.to_le_bytes());
    destination.extend_from_slice(&(!length).to_le_bytes());
    destination.extend_from_slice(block);
    block.clear();
}

fn write_png_chunk(
    destination: &mut Vec<u8>,
    kind: &[u8; 4],
    payload: &[u8],
) -> Result<(), CaptureError> {
    let length = u32::try_from(payload.len())
        .map_err(|_| invalid_frame("PNG chunk exceeds the format size limit"))?;
    destination.extend_from_slice(&length.to_be_bytes());
    destination.extend_from_slice(kind);
    destination.extend_from_slice(payload);
    destination.extend_from_slice(&crc32_parts(kind, payload).to_be_bytes());
    Ok(())
}

struct Adler32 {
    first: u32,
    second: u32,
    pending: u16,
}

impl Adler32 {
    fn new() -> Self {
        Self {
            first: 1,
            second: 0,
            pending: 0,
        }
    }

    #[inline]
    fn update(&mut self, byte: u8) {
        self.first += u32::from(byte);
        self.second += self.first;
        self.pending += 1;
        if self.pending == 5_552 {
            self.reduce();
        }
    }

    fn reduce(&mut self) {
        const MODULUS: u32 = 65_521;
        self.first %= MODULUS;
        self.second %= MODULUS;
        self.pending = 0;
    }

    fn finish(mut self) -> u32 {
        self.reduce();
        (self.second << 16) | self.first
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn crc32_parts(first: &[u8], second: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in first.iter().chain(second) {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
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

#[cfg(test)]
fn read_current_dibv5(owner: HWND) -> Result<Vec<u8>, CaptureError> {
    let (clipboard, _, _) = ClipboardSession::open(owner, OPEN_TIMEOUT)?;
    // SAFETY: The clipboard is open on this thread. The returned handle remains clipboard-owned.
    let handle = unsafe { GetClipboardData(CF_DIBV5_FORMAT) }
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
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use captastic_core::{
        CaptureId, CaptureMode, ColorSpace, DisplayId, FrameMetadata, Rect, TimingProvenance,
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
                frame_generation: Some(1),
                copy_count: 1,
                pool_slot: None,
            },
        )
        .expect("valid fixture")
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

    #[test]
    fn dibv5_rejects_a_truncated_source() {
        let mut frame = frame(1, 1, 4, vec![1, 2, 3, 4]);
        frame.pixels = Arc::from([1_u8, 2, 3]);
        assert_eq!(
            DibV5Layout::new(&frame).expect_err("truncated frame").kind,
            CaptureErrorKind::InvalidFrame
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
        };
        assert_eq!(report.open_retries, 3);
    }

    #[test]
    fn png_clipboard_payload_is_rgba_and_structurally_complete() {
        let frame = frame(1, 1, 4, vec![10, 20, 30, 64]).with_alpha(FrameAlpha::Straight);
        let png = encode_png(&frame).expect("PNG payload");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        assert!(png.windows(4).any(|window| window == b"IDAT"));
        assert!(png.windows(5).any(|window| window == [0, 30, 20, 10, 64]));
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    /// Decodes the stored-block-only zlib stream `encode_png` emits: a 2-byte zlib header,
    /// one or more DEFLATE stored blocks (final-flag byte, LE `LEN`, LE `NLEN`, `LEN` literal
    /// bytes), then a 4-byte big-endian Adler-32 trailer. Returns the reassembled raw bytes
    /// alongside the number of stored blocks consumed, so tests can assert both content and
    /// that a multi-block boundary was actually exercised.
    fn inflate_stored_blocks(zlib: &[u8]) -> (Vec<u8>, usize) {
        assert!(zlib.len() >= 2, "zlib stream missing its 2-byte header");
        let mut cursor = &zlib[2..];
        let mut raw = Vec::new();
        let mut block_count = 0;
        loop {
            assert!(cursor.len() >= 5, "truncated stored-block header");
            let final_block = cursor[0] != 0;
            let len = u16::from_le_bytes([cursor[1], cursor[2]]);
            let nlen = u16::from_le_bytes([cursor[3], cursor[4]]);
            assert_eq!(
                len, !nlen,
                "stored-block LEN/NLEN one's-complement mismatch"
            );
            assert!(
                len as usize <= STORED_BLOCK_LIMIT,
                "stored block exceeds the u16 LEN field limit"
            );
            cursor = &cursor[5..];
            let len = len as usize;
            assert!(cursor.len() >= len, "stored block body truncated");
            raw.extend_from_slice(&cursor[..len]);
            cursor = &cursor[len..];
            block_count += 1;
            if final_block {
                break;
            }
        }
        assert_eq!(
            cursor.len(),
            4,
            "expected exactly the Adler-32 trailer after the final block"
        );
        let adler = u32::from_be_bytes(cursor.try_into().expect("4 trailer bytes"));
        let mut recomputed = Adler32::new();
        for &byte in &raw {
            recomputed.update(byte);
        }
        assert_eq!(
            adler,
            recomputed.finish(),
            "Adler-32 trailer does not match the payload"
        );
        (raw, block_count)
    }

    #[test]
    fn png_stored_block_framing_round_trips_across_a_block_boundary() {
        // width=16384 => row_bytes = 65536, so a single row's filter byte plus pixel bytes
        // (65537 raw bytes) exceeds STORED_BLOCK_LIMIT (65535) and forces `encode_png` to split
        // the row across two DEFLATE stored blocks. This is the exact boundary M41 covers:
        // `Vec::with_capacity(STORED_BLOCK_LIMIT)` is permitted to over-allocate, so a flush
        // decision keyed off `capacity()` instead of the fixed limit could silently grow a block
        // past what the stored-block LEN field (a u16) can represent.
        let width = 16_384_u32;
        let mut pixels = Vec::with_capacity(width as usize * 4);
        for i in 0..width {
            let i = i as u8;
            pixels.extend_from_slice(&[
                i.wrapping_mul(7),
                i.wrapping_mul(13),
                i.wrapping_mul(17),
                255,
            ]);
        }
        let frame = frame(width, 1, width * 4, pixels.clone());

        let png = encode_png(&frame).expect("PNG payload");

        let idat_pos = png
            .windows(4)
            .position(|window| window == b"IDAT")
            .expect("IDAT chunk present");
        let idat_length = u32::from_be_bytes(
            png[idat_pos - 4..idat_pos]
                .try_into()
                .expect("4 length bytes"),
        ) as usize;
        let idat_payload = &png[idat_pos + 4..idat_pos + 4 + idat_length];
        assert_eq!(&idat_payload[..2], &[0x78, 0x01], "zlib header");

        let (raw, block_count) = inflate_stored_blocks(idat_payload);
        assert!(
            block_count >= 2,
            "expected the oversized row to span multiple stored blocks, got {block_count}"
        );

        let mut expected_raw = Vec::with_capacity(1 + pixels.len());
        expected_raw.push(0); // PNG "none" filter byte.
        for pixel in pixels.chunks_exact(4) {
            expected_raw.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
        assert_eq!(raw, expected_raw);
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
        let mut publisher = ClipboardPublisher::new().expect("publisher");
        publisher.publish(&frame).expect("publish");
        let actual = read_current_dibv5(publisher.window.hwnd).expect("clipboard readback");
        assert_eq!(actual, expected);
    }
}
