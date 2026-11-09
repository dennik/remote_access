//! Maps keys, as egui reports them, to the PC set 1 scan codes the agent injects.
//!
//! Keys are mapped by position (egui's physical key), so the remote computer's own
//! keyboard layout decides which character comes out, as it would for a local keyboard.

use eframe::egui::Key;

/// Left Ctrl. On a Mac both Command and Control send this, so Command+C copies on Windows.
pub const CTRL: (u16, bool) = (0x1D, false);
pub const ALT: (u16, bool) = (0x38, false);
/// Left Windows key, which a Mac keyboard doesn't have. The session menu sends it.
pub const WIN: (u16, bool) = (0x5B, true);

/// Returns the scan code and whether it's an extended (E0-prefixed) key.
pub fn scan_code(key: Key) -> Option<(u16, bool)> {
    use Key::*;
    let normal = |scan: u16| Some((scan, false));
    let extended = |scan: u16| Some((scan, true));
    match key {
        Escape => normal(0x01),
        Num1 => normal(0x02),
        Num2 => normal(0x03),
        Num3 => normal(0x04),
        Num4 => normal(0x05),
        Num5 => normal(0x06),
        Num6 => normal(0x07),
        Num7 => normal(0x08),
        Num8 => normal(0x09),
        Num9 => normal(0x0A),
        Num0 => normal(0x0B),
        Minus => normal(0x0C),
        Equals => normal(0x0D),
        Backspace => normal(0x0E),
        Tab => normal(0x0F),
        Q => normal(0x10),
        W => normal(0x11),
        E => normal(0x12),
        R => normal(0x13),
        T => normal(0x14),
        Y => normal(0x15),
        U => normal(0x16),
        I => normal(0x17),
        O => normal(0x18),
        P => normal(0x19),
        OpenBracket => normal(0x1A),
        CloseBracket => normal(0x1B),
        Enter => normal(0x1C),
        ControlLeft | SuperLeft | SuperRight => Some(CTRL),
        A => normal(0x1E),
        S => normal(0x1F),
        D => normal(0x20),
        F => normal(0x21),
        G => normal(0x22),
        H => normal(0x23),
        J => normal(0x24),
        K => normal(0x25),
        L => normal(0x26),
        Semicolon => normal(0x27),
        Quote => normal(0x28),
        Backtick => normal(0x29),
        ShiftLeft => normal(0x2A),
        Backslash => normal(0x2B),
        Z => normal(0x2C),
        X => normal(0x2D),
        C => normal(0x2E),
        V => normal(0x2F),
        B => normal(0x30),
        N => normal(0x31),
        M => normal(0x32),
        Comma => normal(0x33),
        Period => normal(0x34),
        Slash => normal(0x35),
        ShiftRight => normal(0x36),
        AltLeft => Some(ALT),
        Space => normal(0x39),
        CapsLock => normal(0x3A),
        F1 => normal(0x3B),
        F2 => normal(0x3C),
        F3 => normal(0x3D),
        F4 => normal(0x3E),
        F5 => normal(0x3F),
        F6 => normal(0x40),
        F7 => normal(0x41),
        F8 => normal(0x42),
        F9 => normal(0x43),
        F10 => normal(0x44),
        ScrollLock => normal(0x46),
        IntlBackslash => normal(0x56),
        F11 => normal(0x57),
        F12 => normal(0x58),
        F13 => normal(0x64),
        F14 => normal(0x65),
        F15 => normal(0x66),
        F16 => normal(0x67),
        F17 => normal(0x68),
        F18 => normal(0x69),
        F19 => normal(0x6A),
        MediaTrackPrevious => extended(0x10),
        MediaTrackNext => extended(0x19),
        ControlRight => extended(0x1D),
        AudioVolumeMute => extended(0x20),
        MediaPlayPause => extended(0x22),
        MediaStop => extended(0x24),
        AudioVolumeDown => extended(0x2E),
        AudioVolumeUp => extended(0x30),
        PrintScreen => extended(0x37),
        AltRight => extended(0x38),
        Home => extended(0x47),
        ArrowUp => extended(0x48),
        PageUp => extended(0x49),
        ArrowLeft => extended(0x4B),
        ArrowRight => extended(0x4D),
        End => extended(0x4F),
        ArrowDown => extended(0x50),
        PageDown => extended(0x51),
        Insert => extended(0x52),
        Delete => extended(0x53),
        Menu => extended(0x5D),
        _ => None,
    }
}

/// Keys that only modify others. macOS doesn't report key releases for other keys
/// while Command is held, so those are sent as a press and an immediate release.
pub fn is_modifier(key: Key) -> bool {
    use Key::*;
    matches!(key, ShiftLeft | ShiftRight | ControlLeft | ControlRight | AltLeft | AltRight | SuperLeft | SuperRight)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_keys_match_set_1() {
        assert_eq!(scan_code(Key::A), Some((0x1E, false)));
        assert_eq!(scan_code(Key::Enter), Some((0x1C, false)));
        assert_eq!(scan_code(Key::ArrowLeft), Some((0x4B, true)));
        assert_eq!(scan_code(Key::Delete), Some((0x53, true)));
    }

    #[test]
    fn command_acts_as_ctrl() {
        assert_eq!(scan_code(Key::SuperLeft), Some(CTRL));
        assert_eq!(scan_code(Key::SuperRight), Some(CTRL));
        assert!(is_modifier(Key::SuperLeft));
        assert!(!is_modifier(Key::C));
    }
}
