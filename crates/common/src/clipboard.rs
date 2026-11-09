//! Plain-text clipboard access, used on both ends of a session.

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};

const CF_UNICODETEXT: u32 = 13;

/// Larger clipboards are not synced.
pub const MAX_CHARS: usize = 1024 * 1024;

pub fn sequence_number() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

struct Open;

impl Open {
    fn new() -> Option<Self> {
        unsafe { OpenClipboard(Some(HWND::default())) }.ok().map(|_| Open)
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

pub fn get_text() -> Option<String> {
    let _open = Open::new()?;
    unsafe {
        let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
        let global = HGLOBAL(handle.0);
        let ptr = GlobalLock(global) as *const u16;
        if ptr.is_null() {
            return None;
        }
        let max_units = GlobalSize(global) / 2;
        let units = std::slice::from_raw_parts(ptr, max_units);
        let len = units.iter().position(|&c| c == 0).unwrap_or(max_units);
        let text = (len <= MAX_CHARS).then(|| String::from_utf16_lossy(&units[..len]));
        let _ = GlobalUnlock(global);
        text
    }
}

pub fn set_text(text: &str) -> bool {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let Some(_open) = Open::new() else { return false };
    unsafe {
        if EmptyClipboard().is_err() {
            return false;
        }
        let Ok(global) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) else { return false };
        let ptr = GlobalLock(global) as *mut u16;
        if ptr.is_null() {
            let _ = GlobalFree(Some(global));
            return false;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        let _ = GlobalUnlock(global);
        // On success the system owns the allocation.
        if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(global.0))).is_err() {
            let _ = GlobalFree(Some(global));
            return false;
        }
    }
    true
}
