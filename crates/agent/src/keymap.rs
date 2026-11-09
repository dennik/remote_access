//! Maps the PC scan codes the viewer sends to macOS virtual key codes.
//!
//! Keys map by position. The Windows key becomes Command and Alt becomes Option,
//! so Mac shortcuts are typed with the Windows key: Win+C copies, Win+Tab
//! switches apps. Used only by the macOS backend, but kept portable so it's
//! tested on every platform.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

pub const MAC_COMMAND: u16 = 0x37;
pub const MAC_RIGHT_COMMAND: u16 = 0x36;
pub const MAC_SHIFT: u16 = 0x38;
pub const MAC_RIGHT_SHIFT: u16 = 0x3C;
pub const MAC_OPTION: u16 = 0x3A;
pub const MAC_RIGHT_OPTION: u16 = 0x3D;
pub const MAC_CONTROL: u16 = 0x3B;
pub const MAC_RIGHT_CONTROL: u16 = 0x3E;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacKey {
    pub code: u16,
}

impl MacKey {
    pub fn is_modifier(self) -> bool {
        matches!(
            self.code,
            MAC_COMMAND
                | MAC_RIGHT_COMMAND
                | MAC_SHIFT
                | MAC_RIGHT_SHIFT
                | MAC_OPTION
                | MAC_RIGHT_OPTION
                | MAC_CONTROL
                | MAC_RIGHT_CONTROL
        )
    }
}

/// Returns None for keys with no Mac equivalent, such as Print Screen and Scroll Lock.
pub fn to_mac(scan: u16, extended: bool) -> Option<MacKey> {
    let code = if extended {
        match scan {
            0x1C => 0x4C,              // keypad Enter
            0x1D => MAC_RIGHT_CONTROL, // right Ctrl
            0x35 => 0x4B,              // keypad /
            0x38 => MAC_RIGHT_OPTION,  // right Alt / AltGr
            0x47 => 0x73,              // Home
            0x48 => 0x7E,              // Up
            0x49 => 0x74,              // Page Up
            0x4B => 0x7B,              // Left
            0x4D => 0x7C,              // Right
            0x4F => 0x77,              // End
            0x50 => 0x7D,              // Down
            0x51 => 0x79,              // Page Down
            0x52 => 0x72,              // Insert, which is Help on Mac keyboards
            0x53 => 0x75,              // Delete (forward)
            0x5B => MAC_COMMAND,       // left Windows
            0x5C => MAC_RIGHT_COMMAND, // right Windows
            0x5D => return None,       // Menu key
            0x20 => 0x4A,              // Mute
            0x2E => 0x49,              // Volume Down
            0x30 => 0x48,              // Volume Up
            _ => return None,
        }
    } else {
        match scan {
            0x01 => 0x35, // Esc
            0x02 => 0x12, // 1
            0x03 => 0x13, // 2
            0x04 => 0x14, // 3
            0x05 => 0x15, // 4
            0x06 => 0x17, // 5
            0x07 => 0x16, // 6
            0x08 => 0x1A, // 7
            0x09 => 0x1C, // 8
            0x0A => 0x19, // 9
            0x0B => 0x1D, // 0
            0x0C => 0x1B, // -
            0x0D => 0x18, // =
            0x0E => 0x33, // Backspace
            0x0F => 0x30, // Tab
            0x10 => 0x0C, // Q
            0x11 => 0x0D, // W
            0x12 => 0x0E, // E
            0x13 => 0x0F, // R
            0x14 => 0x11, // T
            0x15 => 0x10, // Y
            0x16 => 0x20, // U
            0x17 => 0x22, // I
            0x18 => 0x1F, // O
            0x19 => 0x23, // P
            0x1A => 0x21, // [
            0x1B => 0x1E, // ]
            0x1C => 0x24, // Enter
            0x1D => MAC_CONTROL,
            0x1E => 0x00, // A
            0x1F => 0x01, // S
            0x20 => 0x02, // D
            0x21 => 0x03, // F
            0x22 => 0x05, // G
            0x23 => 0x04, // H
            0x24 => 0x26, // J
            0x25 => 0x28, // K
            0x26 => 0x25, // L
            0x27 => 0x29, // ;
            0x28 => 0x27, // '
            0x29 => 0x32, // `
            0x2A => MAC_SHIFT,
            0x2B => 0x2A, // backslash
            0x2C => 0x06, // Z
            0x2D => 0x07, // X
            0x2E => 0x08, // C
            0x2F => 0x09, // V
            0x30 => 0x0B, // B
            0x31 => 0x2D, // N
            0x32 => 0x2E, // M
            0x33 => 0x2B, // ,
            0x34 => 0x2F, // .
            0x35 => 0x2C, // /
            0x36 => MAC_RIGHT_SHIFT,
            0x37 => 0x43, // keypad *
            0x38 => MAC_OPTION,
            0x39 => 0x31, // Space
            0x3A => 0x39, // Caps Lock
            0x3B => 0x7A, // F1
            0x3C => 0x78, // F2
            0x3D => 0x63, // F3
            0x3E => 0x76, // F4
            0x3F => 0x60, // F5
            0x40 => 0x61, // F6
            0x41 => 0x62, // F7
            0x42 => 0x64, // F8
            0x43 => 0x65, // F9
            0x44 => 0x6D, // F10
            0x45 => 0x47, // Num Lock, which is keypad Clear on Mac keyboards
            0x47 => 0x59, // keypad 7
            0x48 => 0x5B, // keypad 8
            0x49 => 0x5C, // keypad 9
            0x4A => 0x4E, // keypad -
            0x4B => 0x56, // keypad 4
            0x4C => 0x57, // keypad 5
            0x4D => 0x58, // keypad 6
            0x4E => 0x45, // keypad +
            0x4F => 0x53, // keypad 1
            0x50 => 0x54, // keypad 2
            0x51 => 0x55, // keypad 3
            0x52 => 0x52, // keypad 0
            0x53 => 0x41, // keypad .
            0x56 => 0x0A, // the extra key left of Z on ISO keyboards
            0x57 => 0x67, // F11
            0x58 => 0x6F, // F12
            _ => return None,
        }
    };
    Some(MacKey { code })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(scan: u16, extended: bool) -> u16 {
        to_mac(scan, extended).unwrap().code
    }

    #[test]
    fn letters_digits_and_common_keys() {
        assert_eq!(code(0x1E, false), 0x00, "A");
        assert_eq!(code(0x2E, false), 0x08, "C");
        assert_eq!(code(0x2F, false), 0x09, "V");
        assert_eq!(code(0x15, false), 0x10, "Y");
        assert_eq!(code(0x0B, false), 0x1D, "0");
        assert_eq!(code(0x1C, false), 0x24, "Enter");
        assert_eq!(code(0x0E, false), 0x33, "Backspace");
        assert_eq!(code(0x01, false), 0x35, "Esc");
    }

    #[test]
    fn extended_flag_selects_the_navigation_keys() {
        assert_eq!(code(0x48, true), 0x7E, "Up arrow");
        assert_eq!(code(0x48, false), 0x5B, "keypad 8");
        assert_eq!(code(0x53, true), 0x75, "forward Delete");
        assert_eq!(code(0x1C, true), 0x4C, "keypad Enter");
    }

    #[test]
    fn modifiers_map_by_position() {
        assert_eq!(code(0x5B, true), MAC_COMMAND, "Windows key is Command");
        assert_eq!(code(0x38, false), MAC_OPTION, "Alt is Option");
        assert_eq!(code(0x1D, false), MAC_CONTROL);
        assert!(to_mac(0x5B, true).unwrap().is_modifier());
        assert!(!to_mac(0x1E, false).unwrap().is_modifier());
    }

    #[test]
    fn all_letters_are_distinct_and_unmapped_keys_are_none() {
        let letters: Vec<u16> =
            (0x10..=0x19).chain(0x1E..=0x26).chain(0x2C..=0x32).map(|s| code(s, false)).collect();
        assert_eq!(letters.len(), 26);
        let mut unique = letters.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 26, "two letters share a Mac key code");
        assert_eq!(to_mac(0x46, false), None, "Scroll Lock");
        assert_eq!(to_mac(0x37, true), None, "Print Screen");
    }
}
