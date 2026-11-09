//! Plain-text clipboard access, used on both ends of a session.

/// Larger clipboards are not synced.
pub const MAX_CHARS: usize = 1024 * 1024;

#[cfg(windows)]
#[path = "clipboard/windows.rs"]
mod imp;
#[cfg(not(windows))]
#[path = "clipboard/portable.rs"]
mod imp;

pub use imp::{get_text, sequence_number, set_text};
