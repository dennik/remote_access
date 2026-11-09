//! The interface each platform's screen capture implements.

use anyhow::Result;

pub enum Grab {
    /// `Capturer::frame` holds new pixels.
    Frame,
    /// Nothing changed within the timeout. The macOS capturer always returns a frame.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Unchanged,
}

pub trait Capturer {
    fn grab(&mut self, timeout_ms: u32) -> Result<Grab>;
    /// The latest frame as tightly packed BGRA, top row first.
    fn frame(&self) -> (&[u8], u32, u32);
}
