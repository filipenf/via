use anyhow::Result;
use arboard::Clipboard;
use tracing::warn;
use winit::keyboard::KeyCode;

use super::layout::{pane_layout_shortcut, pane_navigation_shortcut};
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

/// Clipboard paste from OS selection (Ctrl+Shift+V, Super+V, Shift+Insert). Plain Ctrl+V is not
/// intercepted so Neovim/shell receive `^V` (e.g. visual block mode).
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
    key == Key::C && modifiers.super_key
}

pub(super) fn forward_special_keys(
    pressed_keys: &[Key],
    modifiers: Modifiers,
    pane: &mut TerminalPane,
    skip_layout_shortcut: bool,
) -> Result<bool> {
    let mut payload = Vec::with_capacity(16);

    for key in pressed_keys.iter().copied() {
        if skip_layout_shortcut
            && (pane_layout_shortcut(key).is_some() || pane_navigation_shortcut(key).is_some())
        {
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

        if modifiers.super_key && key == Key::V {
            continue;
        }

        if modifiers.ctrl {
            if let Some(bytes) = ctrl_sequence(key) {
                payload.extend_from_slice(&bytes);
                continue;
            }
        }

        if let Some(bytes) = key_sequence(key) {
            payload.extend_from_slice(bytes);
        }
    }

    if payload.is_empty() {
        return Ok(false);
    }

    pane.write_all(&payload)?;
    Ok(true)
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

fn key_sequence(key: Key) -> Option<&'static [u8]> {
    match key {
        Key::Enter | Key::NumPadEnter => Some(b"\r"),
        Key::Backspace => Some(b"\x7f"),
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
