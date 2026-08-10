#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
mod console;
#[cfg(windows)]
mod daemon_control;
#[cfg(windows)]
mod dxgi;
#[cfg(windows)]
mod hotkey;
#[cfg(windows)]
mod overlay;
#[cfg(windows)]
mod window_capture;

#[cfg(windows)]
pub use clipboard::{ClipboardPublishReport, ClipboardPublisher};
#[cfg(windows)]
pub use console::ConsoleShutdown;
#[cfg(windows)]
pub use daemon_control::DaemonControl;
#[cfg(windows)]
pub use dxgi::{materialize_native_region, DxgiBackend, GpuMaterialization};
#[cfg(windows)]
pub use hotkey::{HotkeyListener, HotkeySpec};
#[cfg(windows)]
pub use overlay::{
    clear_overlay_resource_cache, select_from_frozen_frame,
    select_from_frozen_frame_with_controller, NativeWindowHandle, OverlayController,
    OverlaySelection, SelectionKind,
};
#[cfg(windows)]
pub use window_capture::materialize_selection;
