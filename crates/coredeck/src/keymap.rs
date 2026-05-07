//! QMK keycode → terminal byte translation.
//!
//! Daemon-local subset of the GUI app's `coredeck::hid::keycodes` module.
//! No egui or display dependencies — just the raw mapping the daemon needs
//! to forward HID input into a wrapper's PTY.

/// Encode keyboard modifiers as the xterm modifier code (1 + bitmap).
/// Returns `None` when no modifiers are pressed.
fn encode_modifiers(shift: bool, alt: bool, ctrl: bool) -> Option<u8> {
    let mut code = 0u8;
    if shift {
        code |= 1;
    }
    if alt {
        code |= 2;
    }
    if ctrl {
        code |= 4;
    }
    if code == 0 {
        None
    } else {
        Some(code + 1)
    }
}

fn build_arrow_seq(modifiers: Option<u8>, key_char: u8) -> Vec<u8> {
    match modifiers {
        Some(m) => vec![0x1b, b'[', b'1', b';', b'0' + m, key_char],
        None => vec![0x1b, b'[', key_char],
    }
}

fn build_home_end_seq(modifiers: Option<u8>, key_char: u8) -> Vec<u8> {
    match modifiers {
        Some(m) => vec![0x1b, b'[', b'1', b';', b'0' + m, key_char],
        None => vec![0x1b, b'[', key_char],
    }
}

fn build_tilde_seq(modifiers: Option<u8>, code: &[u8]) -> Vec<u8> {
    match modifiers {
        Some(m) => {
            let mut seq = vec![0x1b, b'['];
            seq.extend_from_slice(code);
            seq.push(b';');
            seq.push(b'0' + m);
            seq.push(b'~');
            seq
        }
        None => {
            let mut seq = vec![0x1b, b'['];
            seq.extend_from_slice(code);
            seq.push(b'~');
            seq
        }
    }
}

fn build_f1_f4_seq(modifiers: Option<u8>, key_char: u8) -> Vec<u8> {
    match modifiers {
        Some(m) => vec![0x1b, b'[', b'1', b';', b'0' + m, key_char],
        None => vec![0x1b, b'O', key_char],
    }
}

// QMK modifier bit positions (left modifiers)
const MOD_LCTL: u16 = 0x0100;
const MOD_LSFT: u16 = 0x0200;
const MOD_LALT: u16 = 0x0400;

#[derive(Default, Clone, Copy)]
struct Mods {
    ctrl: bool,
    shift: bool,
    alt: bool,
}

fn decompose(keycode: u16) -> (u8, Mods) {
    let base = (keycode & 0xFF) as u8;
    let mods = Mods {
        ctrl: keycode & MOD_LCTL != 0,
        shift: keycode & MOD_LSFT != 0,
        alt: keycode & MOD_LALT != 0,
    };
    (base, mods)
}

fn shifted_or_plain(shift: bool, shifted: u8, plain: u8, alt: bool) -> Vec<u8> {
    let ch = if shift { shifted } else { plain };
    if alt {
        vec![0x1b, ch]
    } else {
        vec![ch]
    }
}

/// Convert a 16-bit QMK keycode into the byte sequence that should be written
/// into a terminal/PTY. Returns `None` for keycodes with no terminal meaning.
pub fn qmk_keycode_to_terminal_bytes(keycode: u16) -> Option<Vec<u8>> {
    let (base, mods) = decompose(keycode);

    // Ctrl + letter → ASCII control code (^A=0x01 .. ^Z=0x1A).
    if mods.ctrl && !mods.shift && !mods.alt && (0x04..=0x1D).contains(&base) {
        return Some(vec![base - 0x04 + 1]);
    }

    let xterm_mod = encode_modifiers(mods.shift, mods.alt, mods.ctrl);

    // Letters
    if (0x04..=0x1D).contains(&base) {
        let ch = if mods.shift {
            b'A' + (base - 0x04)
        } else {
            b'a' + (base - 0x04)
        };
        return Some(if mods.alt { vec![0x1b, ch] } else { vec![ch] });
    }

    // Numbers (0x1E .. 0x27 = 1..0)
    let number_pairs: [(u8, u8, u8); 10] = [
        (0x1E, b'!', b'1'),
        (0x1F, b'@', b'2'),
        (0x20, b'#', b'3'),
        (0x21, b'$', b'4'),
        (0x22, b'%', b'5'),
        (0x23, b'^', b'6'),
        (0x24, b'&', b'7'),
        (0x25, b'*', b'8'),
        (0x26, b'(', b'9'),
        (0x27, b')', b'0'),
    ];
    if let Some(&(_, shifted, plain)) = number_pairs.iter().find(|p| p.0 == base) {
        return Some(shifted_or_plain(mods.shift, shifted, plain, mods.alt));
    }

    match base {
        // Special keys
        0x28 => Some(vec![b'\r']),       // Enter
        0x29 => Some(vec![0x1b]),        // Escape
        0x2A => Some(vec![0x7f]),        // Backspace
        0x2B => {
            // Tab / Shift-Tab
            if mods.shift {
                Some(vec![0x1b, b'[', b'Z'])
            } else {
                Some(vec![b'\t'])
            }
        }
        0x2C => Some(vec![b' ']),        // Space

        // Punctuation
        0x2D => Some(shifted_or_plain(mods.shift, b'_', b'-', mods.alt)),
        0x2E => Some(shifted_or_plain(mods.shift, b'+', b'=', mods.alt)),
        0x2F => Some(shifted_or_plain(mods.shift, b'{', b'[', mods.alt)),
        0x30 => Some(shifted_or_plain(mods.shift, b'}', b']', mods.alt)),
        0x31 => Some(shifted_or_plain(mods.shift, b'|', b'\\', mods.alt)),
        0x33 => Some(shifted_or_plain(mods.shift, b':', b';', mods.alt)),
        0x34 => Some(shifted_or_plain(mods.shift, b'"', b'\'', mods.alt)),
        0x35 => Some(shifted_or_plain(mods.shift, b'~', b'`', mods.alt)),
        0x36 => Some(shifted_or_plain(mods.shift, b'<', b',', mods.alt)),
        0x37 => Some(shifted_or_plain(mods.shift, b'>', b'.', mods.alt)),
        0x38 => Some(shifted_or_plain(mods.shift, b'?', b'/', mods.alt)),

        // F1-F4 (SS3 sequences when unmodified)
        0x3A => Some(build_f1_f4_seq(xterm_mod, b'P')),
        0x3B => Some(build_f1_f4_seq(xterm_mod, b'Q')),
        0x3C => Some(build_f1_f4_seq(xterm_mod, b'R')),
        0x3D => Some(build_f1_f4_seq(xterm_mod, b'S')),

        // F5-F12 (tilde sequences)
        0x3E => Some(build_tilde_seq(xterm_mod, b"15")),
        0x3F => Some(build_tilde_seq(xterm_mod, b"17")),
        0x40 => Some(build_tilde_seq(xterm_mod, b"18")),
        0x41 => Some(build_tilde_seq(xterm_mod, b"19")),
        0x42 => Some(build_tilde_seq(xterm_mod, b"20")),
        0x43 => Some(build_tilde_seq(xterm_mod, b"21")),
        0x44 => Some(build_tilde_seq(xterm_mod, b"23")),
        0x45 => Some(build_tilde_seq(xterm_mod, b"24")),

        // Insert/Delete/PageUp/PageDown
        0x49 => Some(build_tilde_seq(xterm_mod, b"2")),
        0x4C => Some(build_tilde_seq(xterm_mod, b"3")),
        0x4B => Some(build_tilde_seq(xterm_mod, b"5")),
        0x4E => Some(build_tilde_seq(xterm_mod, b"6")),

        // Home/End
        0x4A => Some(build_home_end_seq(xterm_mod, b'H')),
        0x4D => Some(build_home_end_seq(xterm_mod, b'F')),

        // Arrow keys
        0x4F => Some(build_arrow_seq(xterm_mod, b'C')), // Right
        0x50 => Some(build_arrow_seq(xterm_mod, b'D')), // Left
        0x51 => Some(build_arrow_seq(xterm_mod, b'B')), // Down
        0x52 => Some(build_arrow_seq(xterm_mod, b'A')), // Up

        // F13-F24 (extended tilde sequences)
        0x68 => Some(build_tilde_seq(xterm_mod, b"25")),
        0x69 => Some(build_tilde_seq(xterm_mod, b"26")),
        0x6A => Some(build_tilde_seq(xterm_mod, b"28")),
        0x6B => Some(build_tilde_seq(xterm_mod, b"29")),
        0x6C => Some(build_tilde_seq(xterm_mod, b"31")),
        0x6D => Some(build_tilde_seq(xterm_mod, b"32")),
        0x6E => Some(build_tilde_seq(xterm_mod, b"33")),
        0x6F => Some(build_tilde_seq(xterm_mod, b"34")), // F20 — caller may special-case
        0x70 => Some(build_tilde_seq(xterm_mod, b"35")),
        0x71 => Some(build_tilde_seq(xterm_mod, b"36")),
        0x72 => Some(build_tilde_seq(xterm_mod, b"37")),
        0x73 => Some(build_tilde_seq(xterm_mod, b"38")),

        _ => None,
    }
}

/// QMK keycode for F20 (the device's "Claude" button) — special cased by the
/// daemon so it doesn't inject an F20 escape sequence into the wrapper.
pub const KEYCODE_F20: u16 = 0x006F;

/// QMK keycode for F24. The firmware emits this when the Claude button
/// is held while the Stop button is tapped (instead of the usual
/// LCTL(KC_C)) — the daemon treats it as "open a fresh claude in the
/// currently-focused session's cwd."
pub const KEYCODE_FRESH_SESSION: u16 = 0x0073;

/// Knob press+rotate clockwise → cycle to NEXT wrapper.
/// Firmware encoder Layer 1 (encoder held) CW rotation emits `LCTL(KC_TAB)`.
/// Daemon special-cases this so it doesn't fall through to the wrapper PTY
/// as a literal Ctrl+Tab.
pub const KEYCODE_KNOB_NEXT: u16 = 0x012B;

/// Knob press+rotate counter-clockwise → cycle to PREVIOUS wrapper.
/// Firmware encoder Layer 1 CCW rotation emits `LCTL(LSFT(KC_TAB))`.
pub const KEYCODE_KNOB_PREV: u16 = 0x032B;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_c_is_etx() {
        // C = 0x06, LCTL = 0x0100 → 0x0106
        assert_eq!(qmk_keycode_to_terminal_bytes(0x0106), Some(vec![0x03]));
    }

    #[test]
    fn enter() {
        assert_eq!(qmk_keycode_to_terminal_bytes(0x0028), Some(vec![b'\r']));
    }

    #[test]
    fn arrow_up() {
        assert_eq!(qmk_keycode_to_terminal_bytes(0x0052), Some(vec![0x1b, b'[', b'A']));
    }

    #[test]
    fn lowercase_a() {
        assert_eq!(qmk_keycode_to_terminal_bytes(0x0004), Some(vec![b'a']));
    }

    #[test]
    fn shifted_a() {
        // Shift+A → uppercase
        assert_eq!(qmk_keycode_to_terminal_bytes(0x0204), Some(vec![b'A']));
    }

    #[test]
    fn shift_tab() {
        assert_eq!(qmk_keycode_to_terminal_bytes(0x022B), Some(vec![0x1b, b'[', b'Z']));
    }

    #[test]
    fn unknown_returns_none() {
        assert_eq!(qmk_keycode_to_terminal_bytes(0x0000), None);
    }
}
