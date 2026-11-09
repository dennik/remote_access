//! Injects the viewer's mouse and keyboard events.

use rd_common::proto::{MonitorInfo, MouseButton};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP,
    MOUSE_EVENT_FLAGS, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

const XBUTTON1: u32 = 1;
const XBUTTON2: u32 = 2;

fn send_mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    };
    unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
}

/// Maps a 0..=65535 position on one monitor to SendInput's coordinates, which
/// span the whole virtual desktop.
pub fn to_virtual_desktop(
    x: u16,
    y: u16,
    monitor: &MonitorInfo,
    virtual_origin: (i32, i32),
    virtual_size: (i32, i32),
) -> (i32, i32) {
    let scale = |pos: u16, extent: u32, offset: i32, origin: i32, size: i32| {
        let span = (extent as i64 - 1).max(0);
        let pixel = offset as i64 + (pos as i64 * span + 32767) / 65535;
        ((pixel - origin as i64) * 65535 / (size as i64 - 1).max(1)) as i32
    };
    (
        scale(x, monitor.width, monitor.x, virtual_origin.0, virtual_size.0),
        scale(y, monitor.height, monitor.y, virtual_origin.1, virtual_size.1),
    )
}

pub fn mouse_move(x: u16, y: u16, monitor: &MonitorInfo) {
    let (origin, size) = unsafe {
        (
            (GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN)),
            (GetSystemMetrics(SM_CXVIRTUALSCREEN), GetSystemMetrics(SM_CYVIRTUALSCREEN)),
        )
    };
    let (dx, dy) = to_virtual_desktop(x, y, monitor, origin, size);
    send_mouse(dx, dy, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK);
}

pub fn mouse_button(button: MouseButton, down: bool) {
    let (flags, data) = match (button, down) {
        (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
        (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (MouseButton::X1, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
        (MouseButton::X1, false) => (MOUSEEVENTF_XUP, XBUTTON1),
        (MouseButton::X2, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
        (MouseButton::X2, false) => (MOUSEEVENTF_XUP, XBUTTON2),
    };
    send_mouse(0, 0, data, flags);
}

pub fn wheel(horizontal: bool, delta: i16) {
    let flags = if horizontal { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL };
    send_mouse(0, 0, delta as i32 as u32, flags);
}

pub fn key(scan: u16, extended: bool, down: bool) {
    let mut flags: KEYBD_EVENT_FLAGS = KEYEVENTF_SCANCODE;
    if extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if !down {
        flags |= KEYEVENTF_KEYUP;
    }
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    };
    unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(x: i32, y: i32, width: u32, height: u32) -> MonitorInfo {
        MonitorInfo { name: String::new(), x, y, width, height, primary: false }
    }

    #[test]
    fn single_monitor_corners_reach_the_full_range() {
        let m = monitor(0, 0, 1920, 1080);
        assert_eq!(to_virtual_desktop(0, 0, &m, (0, 0), (1920, 1080)), (0, 0));
        assert_eq!(to_virtual_desktop(65535, 65535, &m, (0, 0), (1920, 1080)), (65535, 65535));
    }

    #[test]
    fn second_monitor_to_the_left_uses_negative_origin() {
        // A 1920-wide second display sits left of a 1920-wide primary.
        let second = monitor(-1920, 0, 1920, 1080);
        let (x0, _) = to_virtual_desktop(0, 0, &second, (-1920, 0), (3840, 1080));
        let (x1, _) = to_virtual_desktop(65535, 0, &second, (-1920, 0), (3840, 1080));
        assert_eq!(x0, 0);
        assert!((32700..32768).contains(&x1), "right edge should be near the middle, got {x1}");
    }
}
