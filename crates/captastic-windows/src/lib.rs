#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
mod console;
#[cfg(windows)]
mod daemon_control;
#[cfg(windows)]
mod display_manager;
#[cfg(windows)]
mod dxgi;
#[cfg(windows)]
mod hotkey;
#[cfg(windows)]
mod overlay;
#[cfg(windows)]
mod startup;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod window_capture;
#[cfg(windows)]
mod window_capture_wgc;

#[cfg(windows)]
pub use clipboard::{ClipboardPublishReport, ClipboardPublisher};
#[cfg(windows)]
pub use console::ConsoleShutdown;
#[cfg(windows)]
pub use daemon_control::DaemonControl;
#[cfg(windows)]
pub use display_manager::{display_containing_pointer, DxgiDisplayManager};
#[cfg(windows)]
pub use dxgi::{enumerate_displays, materialize_native_region, DxgiBackend, GpuMaterialization};
#[cfg(windows)]
pub use hotkey::{HotkeyListener, HotkeySpec};
#[cfg(windows)]
pub use overlay::{
    clear_overlay_resource_cache, select_from_frozen_frame,
    select_from_frozen_frame_with_controller, select_from_frozen_frame_with_initial_tool,
    select_from_frozen_frame_with_initial_tool_and_ui, InitialSelectionTool, NativeWindowHandle,
    OverlayController, OverlaySelection, SelectionKind,
};
#[cfg(windows)]
pub use startup::{disable_startup, enable_startup, startup_command};
#[cfg(windows)]
pub use tray::{open_path, show_error_dialog, TrayEvent, TrayIcon};
#[cfg(windows)]
pub use window_capture::materialize_selection;
