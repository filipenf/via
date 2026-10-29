use anyhow::Result;
use arboard::Clipboard;
use tracing::warn;
use winit::keyboard::KeyCode;

use super::layout::layout_chord;
use super::pane::TerminalPane;

const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Key {
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,
    Key1,
    Key2,
    Key3,
    Key4,
    Key5,
    Key6,
    Key7,
    Key8,
    Key9,
    Enter,
    NumPadEnter,
    Backspace,
    Tab,
    Escape,
    Up,
    Down,
    Right,
    Left,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
}

impl Key {
    pub(super) fn from_key_code(code: KeyCode) -> Option<Self> {
        let key = match code {
            KeyCode::KeyA => Self::A,
            KeyCode::KeyB => Self::B,
            KeyCode::KeyC => Self::C,
            KeyCode::KeyD => Self::D,
            KeyCode::KeyE => Self::E,
            KeyCode::KeyF => Self::F,
            KeyCode::KeyG => Self::G,
            KeyCode::KeyH => Self::H,
            KeyCode::KeyI => Self::I,
            KeyCode::KeyJ => Self::J,
            KeyCode::KeyK => Self::K,
            KeyCode::KeyL => Self::L,
            KeyCode::KeyM => Self::M,
            KeyCode::KeyN => Self::N,
            KeyCode::KeyO => Self::O,
            KeyCode::KeyP => Self::P,
            KeyCode::KeyQ => Self::Q,
            KeyCode::KeyR => Self::R,
            KeyCode::KeyS => Self::S,
            KeyCode::KeyT => Self::T,
            KeyCode::KeyU => Self::U,
            KeyCode::KeyV => Self::V,
            KeyCode::KeyW => Self::W,
            KeyCode::KeyX => Self::X,
            KeyCode::KeyY => Self::Y,
            KeyCode::KeyZ => Self::Z,
            KeyCode::Digit1 => Self::Key1,
            KeyCode::Digit2 => Self::Key2,
            KeyCode::Digit3 => Self::Key3,
            KeyCode::Digit4 => Self::Key4,
            KeyCode::Digit5 => Self::Key5,
            KeyCode::Digit6 => Self::Key6,
            KeyCode::Digit7 => Self::Key7,
            KeyCode::Digit8 => Self::Key8,
            KeyCode::Digit9 => Self::Key9,
            KeyCode::Enter => Self::Enter,
            KeyCode::NumpadEnter => Self::NumPadEnter,
            KeyCode::Backspace => Self::Backspace,
            KeyCode::Tab => Self::Tab,
            KeyCode::Escape => Self::Escape,
            KeyCode::ArrowUp => Self::Up,
            KeyCode::ArrowDown => Self::Down,
            KeyCode::ArrowRight => Self::Right,
            KeyCode::ArrowLeft => Self::Left,
            KeyCode::Home => Self::Home,
            KeyCode::End => Self::End,
            KeyCode::PageUp => Self::PageUp,
            KeyCode::PageDown => Self::PageDown,
            KeyCode::Insert => Self::Insert,
            KeyCode::Delete => Self::Delete,
            _ => return None,
        };

        Some(key)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Modifiers {
    pub(super) ctrl: bool,
    pub(super) shift: bool,
    pub(super) alt: bool,
    pub(super) super_key: bool,
}

pub(super) fn forward_text_input(
    text: &str,
    modifiers: Modifiers,
    pane: &mut TerminalPane,
    suppress_input: bool,
) -> Result<bool> {
    if suppress_input || modifiers.ctrl || modifiers.super_key || text.is_empty() {
        return Ok(false);
    }

    pane.write_all(text.as_bytes())?;
    Ok(true)
}

/// Clipboard paste from OS selection (Ctrl+Shift+V, Super+V, Shift+Insert).
/// Plain Ctrl+V is not intercepted so Neovim/shell receive `^V` (e.g. visual block mode).
pub(super) fn try_clipboard_paste(
    paste_requested: bool,
    pane: &mut TerminalPane,
    suppress_input: bool,
) -> Result<bool> {
    if suppress_input || !paste_requested {
        return Ok(false);
    }

    let text = match Clipboard::new().and_then(|mut c| c.get_text()) {
        Ok(t) => t,
        Err(error) => {
            warn!(%error, "clipboard read failed");
            return Ok(false);
        }
    };

    tracing::info!(len = text.len(), "pasting from clipboard");

    if text.is_empty() {
        return Ok(false);
    }

    let mut payload = Vec::with_capacity(text.len() + BRACKETED_PASTE_START.len() + 8);
    payload.extend_from_slice(BRACKETED_PASTE_START);
    payload.extend_from_slice(text.as_bytes());
    payload.extend_from_slice(BRACKETED_PASTE_END);
    pane.write_all(&payload)?;
    Ok(true)
}

pub(super) fn paste_requested(key: Key, modifiers: Modifiers) -> bool {
    (key == Key::V && (modifiers.super_key || (modifiers.ctrl && modifiers.shift)))
        || (key == Key::Insert && (modifiers.shift || modifiers.super_key))
}

pub(super) fn copy_requested(key: Key, modifiers: Modifiers) -> bool {
    (key == Key::C && modifiers.super_key)
        || (key == Key::Insert && modifiers.ctrl && !modifiers.shift && !modifiers.alt)
}

pub(super) fn forward_special_keys(
    pressed_keys: &[Key],
    modifiers: Modifiers,
    pane: &mut TerminalPane,
    skip_layout_shortcut: bool,
) -> Result<bool> {
    let payload = special_keys_payload(pressed_keys, modifiers, skip_layout_shortcut);
    if payload.is_empty() {
        return Ok(false);
    }

    pane.write_all(&payload)?;
    Ok(true)
}

/// The bytes `forward_special_keys` would send, without touching a pane.
///
/// Split out so the layout-chord filter can be asserted on the real path: a chord
/// that the shortcut handler consumes must produce no payload at all. Without this
/// the only observable effect was a write into an optional PTY.
pub(super) fn special_keys_payload(
    pressed_keys: &[Key],
    modifiers: Modifiers,
    skip_layout_shortcut: bool,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(16);

    for key in pressed_keys.iter().copied() {
        // The same predicate the shortcut handler acts on, so a chord can never be
        // consumed there and forwarded to the PTY here.
        if skip_layout_shortcut && layout_chord(key, modifiers).is_some() {
            continue;
        }

        if modifiers.ctrl && key == Key::V && modifiers.shift {
            continue;
        }

        if key == Key::Insert && modifiers.shift {
            continue;
        }

        // Super+Insert paste (or stray Insert with Super held): never send <Insert> to the shell —
        // in Vim insert mode that toggles replace mode.
        if key == Key::Insert && modifiers.super_key {
            continue;
        }

        // Ctrl+Insert is OS copy (Kitty/Ghostty/Omarchy terminal remap of Super+C).
        if key == Key::Insert && modifiers.ctrl {
            continue;
        }

        if modifiers.super_key && key == Key::V {
            continue;
        }

        if modifiers.ctrl {
            if let Some(bytes) = ctrl_sequence(key) {
                payload.extend_from_slice(&bytes);
                continue;
            }
        }

        if let Some(bytes) = key_sequence(key, modifiers) {
            payload.extend_from_slice(bytes);
        }
    }

    payload
}

fn ctrl_sequence(key: Key) -> Option<[u8; 1]> {
    let byte = match key {
        Key::A => 0x01,
        Key::B => 0x02,
        Key::C => 0x03,
        Key::D => 0x04,
        Key::E => 0x05,
        Key::F => 0x06,
        Key::G => 0x07,
        Key::H => 0x08,
        Key::I => 0x09,
        Key::J => 0x0a,
        Key::K => 0x0b,
        Key::L => 0x0c,
        Key::M => 0x0d,
        Key::N => 0x0e,
        Key::O => 0x0f,
        Key::P => 0x10,
        Key::Q => 0x11,
        Key::R => 0x12,
        Key::S => 0x13,
        Key::T => 0x14,
        Key::U => 0x15,
        Key::V => 0x16,
        Key::W => 0x17,
        Key::X => 0x18,
        Key::Y => 0x19,
        Key::Z => 0x1a,
        _ => return None,
    };

    Some([byte])
}

fn key_sequence(key: Key, modifiers: Modifiers) -> Option<&'static [u8]> {
    match key {
        Key::Enter | Key::NumPadEnter => Some(b"\r"),
        Key::Backspace => Some(b"\x7f"),
        // xterm/CSI back-tab — cursor-agent and other TUIs use this for mode cycling.
        Key::Tab if modifiers.shift => Some(b"\x1b[Z"),
        Key::Tab => Some(b"\t"),
        Key::Escape => Some(b"\x1b"),
        Key::Up => Some(b"\x1b[A"),
        Key::Down => Some(b"\x1b[B"),
        Key::Right => Some(b"\x1b[C"),
        Key::Left => Some(b"\x1b[D"),
        Key::Home => Some(b"\x1b[H"),
        Key::End => Some(b"\x1b[F"),
        Key::PageUp => Some(b"\x1b[5~"),
        Key::PageDown => Some(b"\x1b[6~"),
        Key::Insert => Some(b"\x1b[2~"),
        Key::Delete => Some(b"\x1b[3~"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_encodes_as_horizontal_tab() {
        assert_eq!(
            key_sequence(Key::Tab, Modifiers::default()),
            Some(b"\t".as_slice())
        );
    }

    #[test]
    fn shift_tab_encodes_as_csi_back_tab() {
        let modifiers = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(
            key_sequence(Key::Tab, modifiers),
            Some(b"\x1b[Z".as_slice())
        );
    }

    #[test]
    fn super_v_is_paste() {
        let modifiers = Modifiers {
            super_key: true,
            ..Modifiers::default()
        };
        assert!(paste_requested(Key::V, modifiers));
    }

    #[test]
    fn ctrl_shift_v_is_paste() {
        let modifiers = Modifiers {
            ctrl: true,
            shift: true,
            ..Modifiers::default()
        };
        assert!(paste_requested(Key::V, modifiers));
    }

    #[test]
    fn shift_insert_is_paste() {
        let modifiers = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert!(paste_requested(Key::Insert, modifiers));
    }

    #[test]
    fn plain_ctrl_v_is_not_paste() {
        let modifiers = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        assert!(
            !paste_requested(Key::V, modifiers),
            "Ctrl+V must stay ^V for Neovim visual-block"
        );
    }

    #[test]
    fn ctrl_insert_is_copy() {
        let modifiers = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        assert!(copy_requested(Key::Insert, modifiers));
        assert!(!copy_requested(Key::C, modifiers));
    }

    #[test]
    fn super_c_is_copy() {
        let modifiers = Modifiers {
            super_key: true,
            ..Modifiers::default()
        };
        assert!(copy_requested(Key::C, modifiers));
        assert!(!paste_requested(Key::V, Modifiers::default()));
    }

    fn alt() -> Modifiers {
        Modifiers {
            alt: true,
            ..Modifiers::default()
        }
    }

    fn alt_shift() -> Modifiers {
        Modifiers {
            alt: true,
            shift: true,
            ..Modifiers::default()
        }
    }

    #[test]
    fn new_layout_chords_never_reach_the_pty_on_the_real_path() {
        // Drives the actual filter used by the input path, not just the shared
        // predicate. Every chord the shortcut handler consumes must produce no bytes,
        // or it would be acted on and typed into the agent at the same time.
        //
        // The digits are looped rather than listed: Alt+5..8 and Alt+Shift+3..8 are
        // their own match arms, and listing a subset silently skipped them.
        let mut checked = 0;
        for key in [
            Key::Key1,
            Key::Key2,
            Key::Key3,
            Key::Key4,
            Key::Key5,
            Key::Key6,
            Key::Key7,
            Key::Key8,
            Key::Key9,
        ] {
            for modifiers in [alt(), alt_shift()] {
                assert!(layout_chord(key, modifiers).is_some(), "premise: a chord");
                let payload = special_keys_payload(&[key], modifiers, true);
                assert!(
                    payload.is_empty(),
                    "{key:?} with {modifiers:?} leaked {payload:?} to the PTY"
                );
                checked += 1;
            }
        }
        for (key, modifiers) in [(Key::J, alt()), (Key::T, alt())] {
            let payload = special_keys_payload(&[key], modifiers, true);
            assert!(
                payload.is_empty(),
                "{key:?} with {modifiers:?} leaked {payload:?} to the PTY"
            );
            checked += 1;
        }
        assert_eq!(
            checked, 20,
            "9 digits x 2 modifier states plus Alt+J and Alt+T"
        );
    }

    #[test]
    fn alt_arrows_always_reach_the_pty() {
        // Alt+Left/Right and the Shift variants are not layout chords, so even with the
        // skip flag set they are forwarded to the agent.
        for modifiers in [alt(), alt_shift()] {
            assert!(!special_keys_payload(&[Key::Left], modifiers, true).is_empty());
            assert!(!special_keys_payload(&[Key::Right], modifiers, true).is_empty());
        }
    }

    #[test]
    fn skip_flag_does_not_swallow_ordinary_typing() {
        // A key that is not a layout chord must survive the filter, so the agent keeps
        // receiving normal input while shortcuts are live.
        assert_eq!(
            special_keys_payload(&[Key::Tab], Modifiers::default(), true),
            b"\t".to_vec()
        );
        // Alt+Down has an escape sequence and is not a chord, so the filter must let it
        // through.
        assert_eq!(
            special_keys_payload(&[Key::Down], alt(), true),
            b"\x1b[B".to_vec(),
            "Alt+Down is not a layout chord and must reach the agent"
        );
    }
}
