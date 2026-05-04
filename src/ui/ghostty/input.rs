use anyhow::Result;
use arboard::Clipboard;
use tracing::warn;
use winit::keyboard::KeyCode;

use super::layout::{SplitLayout, pane_layout_shortcut, pane_navigation_shortcut};
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

/// Keyboard scroll of the active pane's viewport (Shift+PgUp / Shift+PgDn). Does not send escape
/// sequences to the PTY—same behavior as the mouse wheel. Ghostty: scroll up (into scrollback) is a
/// negative delta.
pub(super) fn forward_keyboard_viewport_scroll(
    pressed_keys: &[Key],
    modifiers: Modifiers,
    active_pane: usize,
    panes: &mut [TerminalPane],
    suppress_input: bool,
) -> bool {
    if suppress_input {
        return false;
    }

    if !modifiers.shift {
        return false;
    }

    let Some(pane) = panes.get_mut(active_pane) else {
        return false;
    };

    let step = pane.viewport_rows().max(1) as isize;

    for key in pressed_keys.iter().copied() {
        match key {
            Key::PageUp => {
                pane.scroll_viewport(-step);
                return true;
            }
            Key::PageDown => {
                pane.scroll_viewport(step);
                return true;
            }
            _ => {}
        }
    }

    false
}

/// Scroll the terminal viewport under the mouse wheel using libghostty scrollback.
pub(super) fn forward_mouse_scroll(
    scroll_delta: (f32, f32),
    cursor_position: Option<(usize, usize)>,
    layout: &SplitLayout,
    panes: &mut [TerminalPane],
    suppress_input: bool,
) -> bool {
    if suppress_input {
        return false;
    }

    let (sx, sy) = scroll_delta;
    // Combine axes so shift+horizontal wheel / sideways trackpad still scrolls the terminal.
    let sy = sy + sx;

    if sy.abs() <= 1e-4 {
        return false;
    }

    let Some((x, y)) = cursor_position else {
        return false;
    };

    let Some((pane_index, _rect)) = layout.pane_at(x, y) else {
        return false;
    };

    // Ghostty: scroll up (into scrollback) is negative. Match low-amplitude axis events: rounding
    // `-sy/40` often yields 0 for a single notch on Wayland/X11.
    let scaled = -sy / 40.0;
    let mut delta_y = scaled.round().clamp(-64.0, 64.0) as isize;
    if delta_y == 0 {
        delta_y = -sy.signum() as isize;
    }

    panes[pane_index].scroll_viewport(delta_y);
    true
}

pub(super) fn forward_special_keys(
    pressed_keys: &[Key],
    modifiers: Modifiers,
    pane: &mut TerminalPane,
    skip_layout_shortcut: bool,
) -> Result<bool> {
    let mut wrote = false;

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

        // Viewport scroll handled in `forward_keyboard_viewport_scroll`; do not send CSI Page keys.
        if modifiers.shift && matches!(key, Key::PageUp | Key::PageDown) {
            continue;
        }

        if modifiers.ctrl {
            if let Some(bytes) = ctrl_sequence(key) {
                pane.write_all(&bytes)?;
                wrote = true;
                continue;
            }
        }

        if let Some(bytes) = key_sequence(key) {
            pane.write_all(bytes)?;
            wrote = true;
        }
    }

    Ok(wrote)
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
