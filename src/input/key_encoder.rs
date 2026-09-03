//! Key encoding using termwiz
//!
//! Converts winit keyboard events to terminal escape sequences using termwiz's
//! KeyCode::encode() method. This provides comprehensive key handling without
//! maintaining manual escape sequence mappings.

use crt_core::TermMode;
use termwiz::input::{
    KeyCode, KeyCodeEncodeModes, KeyboardEncoding, Modifiers as TermwizModifiers,
};
use winit::keyboard::{Key, NamedKey};

/// Terminal modes that change how keys are encoded.
///
/// Derived from the active terminal's [`TermMode`] so that, e.g., arrow keys
/// produce SS3 sequences under DECCKM (application cursor keys) and Enter
/// produces CR LF under LNM (newline mode).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EncodeModes {
    /// DECCKM: cursor keys send `ESC O x` instead of `ESC [ x`
    pub application_cursor_keys: bool,
    /// LNM: Enter sends `\r\n` instead of `\r`
    pub newline_mode: bool,
}

impl EncodeModes {
    /// Derive encoding modes from a terminal's current mode flags.
    pub fn from_term_mode(mode: TermMode) -> Self {
        Self {
            application_cursor_keys: mode.contains(TermMode::APP_CURSOR),
            newline_mode: mode.contains(TermMode::LINE_FEED_NEW_LINE),
        }
    }
}

/// Convert a winit Key to a termwiz KeyCode
fn winit_to_termwiz_keycode(key: &Key) -> Option<KeyCode> {
    match key {
        Key::Character(c) => {
            // Get the first character
            c.chars().next().map(KeyCode::Char)
        }
        Key::Named(named) => {
            match named {
                // Navigation
                NamedKey::ArrowUp => Some(KeyCode::UpArrow),
                NamedKey::ArrowDown => Some(KeyCode::DownArrow),
                NamedKey::ArrowLeft => Some(KeyCode::LeftArrow),
                NamedKey::ArrowRight => Some(KeyCode::RightArrow),
                NamedKey::Home => Some(KeyCode::Home),
                NamedKey::End => Some(KeyCode::End),
                NamedKey::PageUp => Some(KeyCode::PageUp),
                NamedKey::PageDown => Some(KeyCode::PageDown),

                // Editing
                NamedKey::Backspace => Some(KeyCode::Backspace),
                NamedKey::Delete => Some(KeyCode::Delete),
                NamedKey::Insert => Some(KeyCode::Insert),
                NamedKey::Enter => Some(KeyCode::Enter),
                NamedKey::Tab => Some(KeyCode::Tab),
                NamedKey::Escape => Some(KeyCode::Escape),
                NamedKey::Space => Some(KeyCode::Char(' ')),

                // Function keys
                NamedKey::F1 => Some(KeyCode::Function(1)),
                NamedKey::F2 => Some(KeyCode::Function(2)),
                NamedKey::F3 => Some(KeyCode::Function(3)),
                NamedKey::F4 => Some(KeyCode::Function(4)),
                NamedKey::F5 => Some(KeyCode::Function(5)),
                NamedKey::F6 => Some(KeyCode::Function(6)),
                NamedKey::F7 => Some(KeyCode::Function(7)),
                NamedKey::F8 => Some(KeyCode::Function(8)),
                NamedKey::F9 => Some(KeyCode::Function(9)),
                NamedKey::F10 => Some(KeyCode::Function(10)),
                NamedKey::F11 => Some(KeyCode::Function(11)),
                NamedKey::F12 => Some(KeyCode::Function(12)),
                NamedKey::F13 => Some(KeyCode::Function(13)),
                NamedKey::F14 => Some(KeyCode::Function(14)),
                NamedKey::F15 => Some(KeyCode::Function(15)),
                NamedKey::F16 => Some(KeyCode::Function(16)),
                NamedKey::F17 => Some(KeyCode::Function(17)),
                NamedKey::F18 => Some(KeyCode::Function(18)),
                NamedKey::F19 => Some(KeyCode::Function(19)),
                NamedKey::F20 => Some(KeyCode::Function(20)),

                // Other keys that don't produce escape sequences
                _ => None,
            }
        }
        _ => None,
    }
}

/// Build termwiz modifiers from individual modifier flags
fn build_modifiers(ctrl: bool, shift: bool, alt: bool) -> TermwizModifiers {
    let mut mods = TermwizModifiers::NONE;
    if ctrl {
        mods |= TermwizModifiers::CTRL;
    }
    if shift {
        mods |= TermwizModifiers::SHIFT;
    }
    if alt {
        mods |= TermwizModifiers::ALT;
    }
    mods
}

/// Encode a winit key event to terminal escape sequence bytes using the
/// default (non-application) terminal modes.
///
/// Returns the bytes to send to the PTY, or None if the key cannot be encoded.
#[allow(dead_code)] // convenience wrapper; the shell path uses `encode_key_with_modes`
pub fn encode_key(
    key: &Key,
    ctrl_pressed: bool,
    shift_pressed: bool,
    alt_pressed: bool,
) -> Option<Vec<u8>> {
    encode_key_with_modes(
        key,
        ctrl_pressed,
        shift_pressed,
        alt_pressed,
        EncodeModes::default(),
    )
}

/// Encode a winit key event to terminal escape sequence bytes, honouring the
/// terminal's cursor-key / newline modes.
///
/// A `Key::Character` without Ctrl or Alt is sent verbatim (the full text,
/// which may be several code points for composed input or some layouts);
/// Shift and Caps Lock are already applied to the logical key by the platform.
/// Everything else goes through termwiz's xterm-compatible encoder.
///
/// Returns the bytes to send to the PTY, or None if the key cannot be encoded.
pub fn encode_key_with_modes(
    key: &Key,
    ctrl_pressed: bool,
    shift_pressed: bool,
    alt_pressed: bool,
    modes: EncodeModes,
) -> Option<Vec<u8>> {
    if let Key::Character(text) = key
        && !ctrl_pressed
        && !alt_pressed
    {
        return if text.is_empty() {
            None
        } else {
            Some(text.as_bytes().to_vec())
        };
    }

    let keycode = winit_to_termwiz_keycode(key)?;
    let modifiers = build_modifiers(ctrl_pressed, shift_pressed, alt_pressed);

    let termwiz_modes = KeyCodeEncodeModes {
        encoding: KeyboardEncoding::Xterm,
        application_cursor_keys: modes.application_cursor_keys,
        newline_mode: modes.newline_mode,
        modify_other_keys: None,
    };

    // Encode the key - returns a String containing the escape sequence
    match keycode.encode(modifiers, termwiz_modes, true) {
        Ok(encoded) => {
            if encoded.is_empty() {
                None
            } else {
                Some(encoded.into_bytes())
            }
        }
        Err(e) => {
            log::debug!("Failed to encode key {:?}: {}", key, e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_tab() {
        let result = encode_key(&Key::Named(NamedKey::Tab), false, false, false);
        assert_eq!(result, Some(b"\t".to_vec()));
    }

    #[test]
    fn test_encode_shift_tab() {
        let result = encode_key(&Key::Named(NamedKey::Tab), false, true, false);
        // Shift+Tab should produce backtab escape sequence
        assert!(result.is_some());
        let bytes = result.unwrap();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn test_encode_arrow_keys() {
        let up = encode_key(&Key::Named(NamedKey::ArrowUp), false, false, false);
        assert_eq!(up, Some(b"\x1b[A".to_vec()));

        let down = encode_key(&Key::Named(NamedKey::ArrowDown), false, false, false);
        assert_eq!(down, Some(b"\x1b[B".to_vec()));
    }

    #[test]
    fn test_encode_function_keys() {
        let f1 = encode_key(&Key::Named(NamedKey::F1), false, false, false);
        assert!(f1.is_some());

        let f12 = encode_key(&Key::Named(NamedKey::F12), false, false, false);
        assert!(f12.is_some());
    }

    #[test]
    fn test_encode_character() {
        let a = encode_key(&Key::Character("a".into()), false, false, false);
        assert_eq!(a, Some(b"a".to_vec()));
    }

    #[test]
    fn test_encode_ctrl_c() {
        let ctrl_c = encode_key(&Key::Character("c".into()), true, false, false);
        // Ctrl+C should produce ETX (0x03)
        assert_eq!(ctrl_c, Some(vec![0x03]));
    }

    #[test]
    fn test_encode_home_end() {
        // Home/End are forwarded as-is to termwiz (no readline ^A/^E override),
        // so shells and TUI apps get the standard CSI H / CSI F sequences.
        let home = encode_key(&Key::Named(NamedKey::Home), false, false, false);
        assert_eq!(
            home,
            Some(b"\x1b[H".to_vec()),
            "Home key encoding (termwiz)"
        );

        let end = encode_key(&Key::Named(NamedKey::End), false, false, false);
        assert_eq!(end, Some(b"\x1b[F".to_vec()), "End key encoding (termwiz)");
    }

    // ── Additional key encoding tests ──────────────────────────────

    #[test]
    fn encode_enter_produces_cr() {
        let result = encode_key(&Key::Named(NamedKey::Enter), false, false, false);
        assert_eq!(result, Some(b"\r".to_vec()));
    }

    #[test]
    fn encode_escape_produces_esc() {
        let result = encode_key(&Key::Named(NamedKey::Escape), false, false, false);
        assert_eq!(result, Some(b"\x1b".to_vec()));
    }

    #[test]
    fn encode_backspace_produces_del() {
        let result = encode_key(&Key::Named(NamedKey::Backspace), false, false, false);
        assert_eq!(result, Some(vec![0x7f])); // DEL character
    }

    #[test]
    fn encode_delete_key() {
        let result = encode_key(&Key::Named(NamedKey::Delete), false, false, false);
        assert!(result.is_some());
        let bytes = result.unwrap();
        // Delete should produce an escape sequence (ESC [ 3 ~)
        assert!(bytes.starts_with(b"\x1b["));
    }

    #[test]
    fn encode_insert_key() {
        let result = encode_key(&Key::Named(NamedKey::Insert), false, false, false);
        assert!(result.is_some());
        let bytes = result.unwrap();
        assert!(bytes.starts_with(b"\x1b["));
    }

    #[test]
    fn encode_page_up_down() {
        let pgup = encode_key(&Key::Named(NamedKey::PageUp), false, false, false);
        assert!(pgup.is_some());
        let pgdn = encode_key(&Key::Named(NamedKey::PageDown), false, false, false);
        assert!(pgdn.is_some());
        assert_ne!(pgup, pgdn);
    }

    #[test]
    fn encode_left_right_arrows() {
        let left = encode_key(&Key::Named(NamedKey::ArrowLeft), false, false, false);
        assert_eq!(left, Some(b"\x1b[D".to_vec()));
        let right = encode_key(&Key::Named(NamedKey::ArrowRight), false, false, false);
        assert_eq!(right, Some(b"\x1b[C".to_vec()));
    }

    #[test]
    fn encode_ctrl_a_through_z() {
        // Ctrl+A = 0x01, Ctrl+Z = 0x1A
        for (i, ch) in ('a'..='z').enumerate() {
            let result = encode_key(&Key::Character(ch.to_string().into()), true, false, false);
            let expected = (i as u8) + 1;
            assert_eq!(
                result,
                Some(vec![expected]),
                "Ctrl+{} should be 0x{:02x}",
                ch,
                expected
            );
        }
    }

    #[test]
    fn encode_f1_through_f12_all_different() {
        let named_keys = [
            NamedKey::F1,
            NamedKey::F2,
            NamedKey::F3,
            NamedKey::F4,
            NamedKey::F5,
            NamedKey::F6,
            NamedKey::F7,
            NamedKey::F8,
            NamedKey::F9,
            NamedKey::F10,
            NamedKey::F11,
            NamedKey::F12,
        ];
        let results: Vec<_> = named_keys
            .iter()
            .map(|k| encode_key(&Key::Named(*k), false, false, false))
            .collect();
        // All should produce some output
        for (i, r) in results.iter().enumerate() {
            assert!(r.is_some(), "F{} should produce output", i + 1);
        }
        // All should be unique
        for i in 0..results.len() {
            for j in (i + 1)..results.len() {
                assert_ne!(
                    results[i],
                    results[j],
                    "F{} and F{} should differ",
                    i + 1,
                    j + 1
                );
            }
        }
    }

    #[test]
    fn encode_space() {
        let result = encode_key(&Key::Named(NamedKey::Space), false, false, false);
        assert_eq!(result, Some(b" ".to_vec()));
    }

    #[test]
    fn encode_alt_character() {
        let result = encode_key(&Key::Character("a".into()), false, false, true);
        assert!(result.is_some());
        let bytes = result.unwrap();
        // Alt+a should produce ESC + a
        assert_eq!(bytes, b"\x1ba".to_vec());
    }

    #[test]
    fn encode_unknown_named_key_returns_none() {
        // CapsLock has no terminal encoding
        let result = encode_key(&Key::Named(NamedKey::CapsLock), false, false, false);
        assert_eq!(result, None);
    }

    #[test]
    fn encode_shift_arrows_produce_modified_sequences() {
        let result = encode_key(&Key::Named(NamedKey::ArrowUp), false, true, false);
        assert!(result.is_some());
        let bytes = result.unwrap();
        // Shift+Up should produce a modified sequence (different from plain Up)
        assert_ne!(bytes, b"\x1b[A".to_vec());
    }

    // ── Encode-mode tests (B16) ────────────────────────────────────

    const APP_CURSOR: EncodeModes = EncodeModes {
        application_cursor_keys: true,
        newline_mode: false,
    };

    #[test]
    fn encode_modes_from_term_mode() {
        let m = EncodeModes::from_term_mode(TermMode::APP_CURSOR | TermMode::LINE_FEED_NEW_LINE);
        assert!(m.application_cursor_keys);
        assert!(m.newline_mode);
        let m = EncodeModes::from_term_mode(TermMode::empty());
        assert_eq!(m, EncodeModes::default());
    }

    #[test]
    fn encode_arrows_use_ss3_under_app_cursor_mode() {
        let up = encode_key_with_modes(
            &Key::Named(NamedKey::ArrowUp),
            false,
            false,
            false,
            APP_CURSOR,
        );
        assert_eq!(up, Some(b"\x1bOA".to_vec()));
        let left = encode_key_with_modes(
            &Key::Named(NamedKey::ArrowLeft),
            false,
            false,
            false,
            APP_CURSOR,
        );
        assert_eq!(left, Some(b"\x1bOD".to_vec()));
    }

    #[test]
    fn encode_home_end_use_ss3_under_app_cursor_mode() {
        let home =
            encode_key_with_modes(&Key::Named(NamedKey::Home), false, false, false, APP_CURSOR);
        assert_eq!(home, Some(b"\x1bOH".to_vec()));
        let end =
            encode_key_with_modes(&Key::Named(NamedKey::End), false, false, false, APP_CURSOR);
        assert_eq!(end, Some(b"\x1bOF".to_vec()));
    }

    #[test]
    fn encode_enter_under_newline_mode_sends_crlf() {
        let modes = EncodeModes {
            application_cursor_keys: false,
            newline_mode: true,
        };
        let enter = encode_key_with_modes(&Key::Named(NamedKey::Enter), false, false, false, modes);
        assert_eq!(enter, Some(b"\r\n".to_vec()));
    }

    #[test]
    fn encode_multi_char_character_sends_full_text() {
        // Composed input / some layouts deliver several code points in one key.
        let result = encode_key(&Key::Character("ạ̈".into()), false, false, false);
        assert_eq!(result, Some("ạ̈".as_bytes().to_vec()));
        let result = encode_key(&Key::Character("ab".into()), false, false, false);
        assert_eq!(result, Some(b"ab".to_vec()));
    }

    #[test]
    fn encode_non_ascii_character_verbatim() {
        let result = encode_key(&Key::Character("é".into()), false, true, false);
        assert_eq!(result, Some("é".as_bytes().to_vec()));
    }

    #[test]
    fn encode_uppercase_with_shift_is_verbatim() {
        // Shift is already applied to the logical key; don't re-derive it.
        let result = encode_key(&Key::Character("A".into()), false, true, false);
        assert_eq!(result, Some(b"A".to_vec()));
    }
}
