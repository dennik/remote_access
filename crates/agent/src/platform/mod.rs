//! Operating-system specific parts of the agent. Each platform provides:
//! `capture` (list_monitors, create_capturer, current_cursor), `input`
//! (mouse_move, mouse_button, wheel, key), `install` (install, uninstall, info),
//! `DesktopTracker` and `set_dpi_aware`.

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::*;

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::*;

#[cfg(not(any(windows, target_os = "macos")))]
compile_error!("rdagent supports Windows and macOS only");
