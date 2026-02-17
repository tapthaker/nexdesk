/// Keycode translation between evdev (Linux) and macOS virtual keycodes.
///
/// Evdev keycodes follow linux/input-event-codes.h.
/// macOS keycodes are CGKeyCode / kVK_* virtual key codes.

/// Sentinel value for unmapped keys.
const NONE: u16 = 0xFFFF;

/// Lookup table: index = evdev keycode, value = macOS virtual keycode.
/// 0xFFFF means unmapped.
#[rustfmt::skip]
static EVDEV_TO_MACOS: [u16; 256] = {
    let mut t = [NONE; 256];

    // Row 1: Escape + Function keys
    t[1]   = 0x35; // KEY_ESC        -> kVK_Escape
    t[59]  = 0x7A; // KEY_F1         -> kVK_F1
    t[60]  = 0x78; // KEY_F2         -> kVK_F2
    t[61]  = 0x63; // KEY_F3         -> kVK_F3
    t[62]  = 0x76; // KEY_F4         -> kVK_F4
    t[63]  = 0x60; // KEY_F5         -> kVK_F5
    t[64]  = 0x61; // KEY_F6         -> kVK_F6
    t[65]  = 0x62; // KEY_F7         -> kVK_F7
    t[66]  = 0x64; // KEY_F8         -> kVK_F8
    t[67]  = 0x65; // KEY_F9         -> kVK_F9
    t[68]  = 0x6D; // KEY_F10        -> kVK_F10
    t[87]  = 0x67; // KEY_F11        -> kVK_F11
    t[88]  = 0x6F; // KEY_F12        -> kVK_F12

    // Row 2: Number row
    t[41]  = 0x32; // KEY_GRAVE      -> kVK_ANSI_Grave (`)
    t[2]   = 0x12; // KEY_1          -> kVK_ANSI_1
    t[3]   = 0x13; // KEY_2          -> kVK_ANSI_2
    t[4]   = 0x14; // KEY_3          -> kVK_ANSI_3
    t[5]   = 0x15; // KEY_4          -> kVK_ANSI_4
    t[6]   = 0x17; // KEY_5          -> kVK_ANSI_5
    t[7]   = 0x16; // KEY_6          -> kVK_ANSI_6
    t[8]   = 0x1A; // KEY_7          -> kVK_ANSI_7
    t[9]   = 0x1C; // KEY_8          -> kVK_ANSI_8
    t[10]  = 0x19; // KEY_9          -> kVK_ANSI_9
    t[11]  = 0x1D; // KEY_0          -> kVK_ANSI_0
    t[12]  = 0x1B; // KEY_MINUS      -> kVK_ANSI_Minus
    t[13]  = 0x18; // KEY_EQUAL      -> kVK_ANSI_Equal
    t[14]  = 0x33; // KEY_BACKSPACE  -> kVK_Delete (backspace)

    // Row 3: Tab + QWERTY
    t[15]  = 0x30; // KEY_TAB        -> kVK_Tab
    t[16]  = 0x0C; // KEY_Q          -> kVK_ANSI_Q
    t[17]  = 0x0D; // KEY_W          -> kVK_ANSI_W
    t[18]  = 0x0E; // KEY_E          -> kVK_ANSI_E
    t[19]  = 0x0F; // KEY_R          -> kVK_ANSI_R
    t[20]  = 0x11; // KEY_T          -> kVK_ANSI_T
    t[21]  = 0x10; // KEY_Y          -> kVK_ANSI_Y
    t[22]  = 0x20; // KEY_U          -> kVK_ANSI_U
    t[23]  = 0x22; // KEY_I          -> kVK_ANSI_I
    t[24]  = 0x1F; // KEY_O          -> kVK_ANSI_O
    t[25]  = 0x23; // KEY_P          -> kVK_ANSI_P
    t[26]  = 0x21; // KEY_LEFTBRACE  -> kVK_ANSI_LeftBracket
    t[27]  = 0x1E; // KEY_RIGHTBRACE -> kVK_ANSI_RightBracket
    t[43]  = 0x2A; // KEY_BACKSLASH  -> kVK_ANSI_Backslash

    // Row 4: Caps + ASDF
    t[58]  = 0x39; // KEY_CAPSLOCK   -> kVK_CapsLock
    t[30]  = 0x00; // KEY_A          -> kVK_ANSI_A
    t[31]  = 0x01; // KEY_S          -> kVK_ANSI_S
    t[32]  = 0x02; // KEY_D          -> kVK_ANSI_D
    t[33]  = 0x03; // KEY_F          -> kVK_ANSI_F
    t[34]  = 0x05; // KEY_G          -> kVK_ANSI_G
    t[35]  = 0x04; // KEY_H          -> kVK_ANSI_H
    t[36]  = 0x26; // KEY_J          -> kVK_ANSI_J
    t[37]  = 0x28; // KEY_K          -> kVK_ANSI_K
    t[38]  = 0x25; // KEY_L          -> kVK_ANSI_L
    t[39]  = 0x29; // KEY_SEMICOLON  -> kVK_ANSI_Semicolon
    t[40]  = 0x27; // KEY_APOSTROPHE -> kVK_ANSI_Quote
    t[28]  = 0x24; // KEY_ENTER      -> kVK_Return

    // Row 5: Shift + ZXCV
    t[42]  = 0x38; // KEY_LEFTSHIFT  -> kVK_Shift
    t[44]  = 0x06; // KEY_Z          -> kVK_ANSI_Z
    t[45]  = 0x07; // KEY_X          -> kVK_ANSI_X
    t[46]  = 0x08; // KEY_C          -> kVK_ANSI_C
    t[47]  = 0x09; // KEY_V          -> kVK_ANSI_V
    t[48]  = 0x0B; // KEY_B          -> kVK_ANSI_B
    t[49]  = 0x2D; // KEY_N          -> kVK_ANSI_N
    t[50]  = 0x2E; // KEY_M          -> kVK_ANSI_M
    t[51]  = 0x2B; // KEY_COMMA      -> kVK_ANSI_Comma
    t[52]  = 0x2F; // KEY_DOT        -> kVK_ANSI_Period
    t[53]  = 0x2C; // KEY_SLASH      -> kVK_ANSI_Slash
    t[54]  = 0x3C; // KEY_RIGHTSHIFT -> kVK_RightShift

    // Row 6: Bottom row
    t[29]  = 0x3B; // KEY_LEFTCTRL   -> kVK_Control
    t[125] = 0x37; // KEY_LEFTMETA   -> kVK_Command (Super/Win key)
    t[56]  = 0x3A; // KEY_LEFTALT    -> kVK_Option
    t[57]  = 0x31; // KEY_SPACE      -> kVK_Space
    t[100] = 0x3D; // KEY_RIGHTALT   -> kVK_RightOption
    t[126] = 0x36; // KEY_RIGHTMETA  -> kVK_RightCommand
    t[97]  = 0x3E; // KEY_RIGHTCTRL  -> kVK_RightControl

    // Arrow keys
    t[103] = 0x7E; // KEY_UP         -> kVK_UpArrow
    t[108] = 0x7D; // KEY_DOWN       -> kVK_DownArrow
    t[105] = 0x7B; // KEY_LEFT       -> kVK_LeftArrow
    t[106] = 0x7C; // KEY_RIGHT      -> kVK_RightArrow

    // Navigation cluster
    t[110] = 0x72; // KEY_INSERT     -> kVK_Help (macOS equivalent)
    t[111] = 0x75; // KEY_DELETE     -> kVK_ForwardDelete
    t[102] = 0x73; // KEY_HOME       -> kVK_Home
    t[107] = 0x77; // KEY_END        -> kVK_End
    t[104] = 0x74; // KEY_PAGEUP     -> kVK_PageUp
    t[109] = 0x79; // KEY_PAGEDOWN   -> kVK_PageDown

    // Numpad
    t[69]  = 0x47; // KEY_NUMLOCK    -> kVK_ANSI_KeypadClear
    t[98]  = 0x4B; // KEY_KPSLASH    -> kVK_ANSI_KeypadDivide
    t[55]  = 0x43; // KEY_KPASTERISK -> kVK_ANSI_KeypadMultiply
    t[74]  = 0x4E; // KEY_KPMINUS    -> kVK_ANSI_KeypadMinus
    t[78]  = 0x45; // KEY_KPPLUS     -> kVK_ANSI_KeypadPlus
    t[96]  = 0x4C; // KEY_KPENTER    -> kVK_ANSI_KeypadEnter
    t[83]  = 0x41; // KEY_KPDOT      -> kVK_ANSI_KeypadDecimal
    t[82]  = 0x52; // KEY_KP0        -> kVK_ANSI_Keypad0
    t[79]  = 0x53; // KEY_KP1        -> kVK_ANSI_Keypad1
    t[80]  = 0x54; // KEY_KP2        -> kVK_ANSI_Keypad2
    t[81]  = 0x55; // KEY_KP3        -> kVK_ANSI_Keypad3
    t[75]  = 0x56; // KEY_KP4        -> kVK_ANSI_Keypad4
    t[76]  = 0x57; // KEY_KP5        -> kVK_ANSI_Keypad5
    t[77]  = 0x58; // KEY_KP6        -> kVK_ANSI_Keypad6
    t[71]  = 0x59; // KEY_KP7        -> kVK_ANSI_Keypad7
    t[72]  = 0x5B; // KEY_KP8        -> kVK_ANSI_Keypad8
    t[73]  = 0x5C; // KEY_KP9        -> kVK_ANSI_Keypad9

    t
};

/// Convert an evdev keycode to a macOS virtual keycode.
/// Returns `None` for unmapped keys.
pub fn evdev_to_macos(evdev: u32) -> Option<u16> {
    if (evdev as usize) < EVDEV_TO_MACOS.len() {
        let mac = EVDEV_TO_MACOS[evdev as usize];
        if mac != NONE { Some(mac) } else { None }
    } else {
        None
    }
}

/// Lookup table: index = macOS virtual keycode, value = evdev keycode.
/// 0xFFFF means unmapped.
static MACOS_TO_EVDEV: std::sync::LazyLock<[u32; 256]> = std::sync::LazyLock::new(|| {
    let mut t = [0xFFFFFFFFu32; 256];
    for (evdev, &mac) in EVDEV_TO_MACOS.iter().enumerate() {
        if mac != NONE && (mac as usize) < 256 {
            t[mac as usize] = evdev as u32;
        }
    }
    t
});

/// Convert a macOS virtual keycode to an evdev keycode.
/// Returns `None` for unmapped keys.
pub fn macos_to_evdev(mac: u16) -> Option<u32> {
    if (mac as usize) < MACOS_TO_EVDEV.len() {
        let ev = MACOS_TO_EVDEV[mac as usize];
        if ev != 0xFFFFFFFF { Some(ev) } else { None }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letter_a_maps_correctly() {
        assert_eq!(evdev_to_macos(30), Some(0x00)); // KEY_A -> kVK_ANSI_A
    }

    #[test]
    fn enter_maps_correctly() {
        assert_eq!(evdev_to_macos(28), Some(0x24)); // KEY_ENTER -> kVK_Return
    }

    #[test]
    fn space_maps_correctly() {
        assert_eq!(evdev_to_macos(57), Some(0x31)); // KEY_SPACE -> kVK_Space
    }

    #[test]
    fn left_shift_maps_correctly() {
        assert_eq!(evdev_to_macos(42), Some(0x38)); // KEY_LEFTSHIFT -> kVK_Shift
    }

    #[test]
    fn escape_maps_correctly() {
        assert_eq!(evdev_to_macos(1), Some(0x35)); // KEY_ESC -> kVK_Escape
    }

    #[test]
    fn tab_maps_correctly() {
        assert_eq!(evdev_to_macos(15), Some(0x30)); // KEY_TAB -> kVK_Tab
    }

    #[test]
    fn backspace_maps_correctly() {
        assert_eq!(evdev_to_macos(14), Some(0x33)); // KEY_BACKSPACE -> kVK_Delete
    }

    #[test]
    fn arrow_keys_map_correctly() {
        assert_eq!(evdev_to_macos(103), Some(0x7E)); // UP
        assert_eq!(evdev_to_macos(108), Some(0x7D)); // DOWN
        assert_eq!(evdev_to_macos(105), Some(0x7B)); // LEFT
        assert_eq!(evdev_to_macos(106), Some(0x7C)); // RIGHT
    }

    #[test]
    fn left_meta_maps_to_command() {
        assert_eq!(evdev_to_macos(125), Some(0x37)); // KEY_LEFTMETA -> kVK_Command
    }

    #[test]
    fn unmapped_key_returns_none() {
        assert_eq!(evdev_to_macos(255), None);
    }

    #[test]
    fn out_of_range_returns_none() {
        assert_eq!(evdev_to_macos(1000), None);
    }

    #[test]
    fn roundtrip_letter_a() {
        let mac = evdev_to_macos(30).unwrap();
        assert_eq!(macos_to_evdev(mac), Some(30));
    }

    #[test]
    fn roundtrip_all_mapped_keys() {
        for evdev in 0u32..256 {
            if let Some(mac) = evdev_to_macos(evdev) {
                assert_eq!(
                    macos_to_evdev(mac),
                    Some(evdev),
                    "roundtrip failed for evdev={} mac=0x{:02X}",
                    evdev,
                    mac
                );
            }
        }
    }

    #[test]
    fn number_row_maps_correctly() {
        assert_eq!(evdev_to_macos(2), Some(0x12));  // KEY_1
        assert_eq!(evdev_to_macos(11), Some(0x1D)); // KEY_0
    }

    #[test]
    fn f_keys_map_correctly() {
        assert_eq!(evdev_to_macos(59), Some(0x7A)); // F1
        assert_eq!(evdev_to_macos(88), Some(0x6F)); // F12
    }
}
