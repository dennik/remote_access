//! Follows the input desktop, which changes when Windows switches between the
//! normal desktop and the secure one used for the lock screen and UAC prompts.

use windows::Win32::Foundation::GENERIC_ALL;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS,
    DESKTOP_CONTROL_FLAGS, HDESK, UOI_NAME,
};

/// Keeps the calling thread attached to the input desktop. Each thread that
/// captures or injects input needs its own tracker.
#[derive(Default)]
pub struct DesktopTracker {
    handle: Option<HDESK>,
    name: String,
}

impl DesktopTracker {
    /// Returns true if the thread moved to a different desktop. Without SYSTEM
    /// rights the secure desktop can't be opened, so the thread just stays put.
    pub fn sync(&mut self) -> bool {
        unsafe {
            let access = DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0);
            let Ok(desk) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, access) else {
                return false;
            };
            let name = desktop_name(desk);
            if name == self.name || SetThreadDesktop(desk).is_err() {
                let _ = CloseDesktop(desk);
                return false;
            }
            if let Some(old) = self.handle.replace(desk) {
                let _ = CloseDesktop(old);
            }
            self.name = name;
            true
        }
    }
}

impl Drop for DesktopTracker {
    fn drop(&mut self) {
        // Windows closes the attached desktop handle when the thread exits.
        self.handle.take();
    }
}

unsafe fn desktop_name(desk: HDESK) -> String {
    let mut buf = [0u16; 128];
    let mut needed = 0u32;
    let result = GetUserObjectInformationW(
        windows::Win32::Foundation::HANDLE(desk.0),
        UOI_NAME,
        Some(buf.as_mut_ptr().cast()),
        std::mem::size_of_val(&buf) as u32,
        Some(&mut needed),
    );
    if result.is_err() {
        return String::new();
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}
