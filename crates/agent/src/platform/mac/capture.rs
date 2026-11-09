//! Screen capture with CGDisplayCreateImage.
//!
//! Apple deprecated this in favour of ScreenCaptureKit. It still works on current
//! macOS, but if a future release breaks it, ScreenCaptureKit is the replacement.

use crate::capture::{Capturer, Grab};
use anyhow::{bail, Result};
use core_graphics::display::{CGDirectDisplayID, CGDisplay};
use rd_common::proto::{CursorShape, MonitorInfo};

const NAME_PREFIX: &str = "display:";

/// Geometry is in points, the unit CGEvent uses for pointer positions.
pub fn list_monitors() -> Vec<MonitorInfo> {
    let Ok(ids) = CGDisplay::active_displays() else { return Vec::new() };
    let mut list: Vec<MonitorInfo> = ids
        .into_iter()
        .map(|id| {
            let display = CGDisplay::new(id);
            let bounds = display.bounds();
            MonitorInfo {
                name: format!("{NAME_PREFIX}{id}"),
                x: bounds.origin.x as i32,
                y: bounds.origin.y as i32,
                width: bounds.size.width as u32,
                height: bounds.size.height as u32,
                primary: display.is_main(),
            }
        })
        .collect();
    list.sort_by_key(|m| (!m.primary, m.x, m.y));
    list
}

pub fn create_capturer(monitor: &MonitorInfo) -> Box<dyn Capturer> {
    let id = monitor
        .name
        .strip_prefix(NAME_PREFIX)
        .and_then(|n| n.parse::<CGDirectDisplayID>().ok())
        .unwrap_or_else(|| CGDisplay::main().id);
    Box::new(DisplayImage { id, buffer: Vec::new(), width: 0, height: 0 })
}

/// Reading the system cursor shape needs AppKit, so the viewer shows an arrow.
pub fn current_cursor() -> CursorShape {
    CursorShape::Arrow
}

struct DisplayImage {
    id: CGDirectDisplayID,
    buffer: Vec<u8>,
    width: u32,
    height: u32,
}

impl Capturer for DisplayImage {
    fn grab(&mut self, timeout_ms: u32) -> Result<Grab> {
        // No change notification here, so the caller's timeout sets the pace.
        if timeout_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(timeout_ms.min(33) as u64));
        }
        let Some(image) = CGDisplay::new(self.id).image() else {
            bail!("couldn't capture display {}", self.id);
        };
        if image.bits_per_pixel() != 32 {
            bail!("unsupported pixel format: {} bits per pixel", image.bits_per_pixel());
        }
        let (width, height) = (image.width() as u32, image.height() as u32);
        let stride = image.bytes_per_row();
        let data = image.data();
        let bytes = data.bytes();
        let row = width as usize * 4;
        if bytes.len() < stride * (height as usize - 1) + row {
            bail!("display image is smaller than its reported size");
        }

        // Display images are 32-bit little-endian with alpha first, which is BGRA in memory.
        self.buffer.resize(row * height as usize, 0);
        for y in 0..height as usize {
            self.buffer[y * row..(y + 1) * row].copy_from_slice(&bytes[y * stride..y * stride + row]);
        }
        self.width = width;
        self.height = height;
        Ok(Grab::Frame)
    }

    fn frame(&self) -> (&[u8], u32, u32) {
        (&self.buffer, self.width, self.height)
    }
}
