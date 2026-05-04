use anyhow::Result;
use arboard::Clipboard;
use crossbeam_channel::{Receiver, Sender};
use minifb::{InputCallback, Key, KeyRepeat, MouseMode, Window};
use tracing::warn;

use super::layout::{SplitLayout, pane_layout_shortcut, pane_navigation_shortcut};
use super::pane::TerminalPane;

const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

pub(super) struct TextInput {
    input: Sender<char>,
    paste_signal: Sender<()>,
}

impl TextInput {
    pub(super) fn new(input: Sender<char>, paste_signal: Sender<()>) -> Self {
        Self { input, paste_signal }
    }
}

impl InputCallback for TextInput {
    fn add_char(&mut self, uni_char: u32) {
        // Ctrl+Shift+V arrives as uppercase 'V' (86) through the text callback
        // while minifb never reports V via is_key_pressed when Ctrl+Shift is held.
        // Ctrl+V (without shift) arrives as 0x16 (SYN).
        // Super+V typically arrives as 'v' / 'V' (118 / 86) with no Ctrl modifier — the main loop
        // pairs paste_signal with super_key (see try_clipboard_paste).
        // Signal the main loop to check for a paste in any of these cases.
        if matches!(uni_char, 0x16 | 86 | 118) {
            let _ = self.paste_signal.send(());
        }

        if let Some(ch) = char::from_u32(uni_char) {
            let _ = self.input.send(ch);
        }
    }
}

pub(super) fn forward_text_input(
    input: &Receiver<char>,
    window: &Window,
    pane: &mut TerminalPane,
    suppress_input: bool,
) -> Result<bool> {
    let ctrl = window.is_key_down(Key::LeftCtrl) || window.is_key_down(Key::RightCtrl);
    let super_down = window.is_key_down(Key::LeftSuper) || window.is_key_down(Key::RightSuper);
    let mut wrote = false;

    for ch in input.try_iter() {
        if suppress_input || ctrl || super_down {
            continue;
        }

        let mut bytes = [0; 4];
        pane.write_all(ch.encode_utf8(&mut bytes).as_bytes())?;
        wrote = true;
    }

    Ok(wrote)
}

/// Clipboard paste from OS selection (Ctrl+V, Ctrl+Shift+V, Super+V, Shift+Insert).
pub(super) fn try_clipboard_paste(
    window: &Window,
    paste_signal: &Receiver<()>,
    pane: &mut TerminalPane,
    suppress_input: bool,
) -> Result<bool> {
    if suppress_input {
        // Drain any pending signals.
        for _ in paste_signal.try_iter() {}
        return Ok(false);
    }

    let shift = window.is_key_down(Key::LeftShift) || window.is_key_down(Key::RightShift);
    let ctrl = window.is_key_down(Key::LeftCtrl) || window.is_key_down(Key::RightCtrl);
    let super_key = window.is_key_down(Key::LeftSuper) || window.is_key_down(Key::RightSuper);
    let insert_pressed = window.is_key_pressed(Key::Insert, KeyRepeat::No);
    let super_v = super_key && window.is_key_pressed(Key::V, KeyRepeat::No);
    // Super+Insert: some stacks report this chord instead of Super+V; same paste intent.
    let super_insert = super_key && insert_pressed;

    // paste_signal fires when add_char sees Ctrl+V (0x16), 'V' (86), or 'v' (118). That path
    // must accept Ctrl *or* Super — Super+V does not set the Ctrl modifier.
    let got_paste_signal = paste_signal.try_iter().next().is_some();
    let paste_from_text_callback = got_paste_signal && (ctrl || super_key);

    let paste_requested = paste_from_text_callback
        || (insert_pressed && shift)
        || super_v
        || super_insert;

    if !paste_requested {
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

/// Scroll the terminal viewport under the mouse wheel using libghostty scrollback.
pub(super) fn forward_mouse_scroll(
    window: &Window,
    layout: &SplitLayout,
    panes: &mut [TerminalPane],
    suppress_input: bool,
) -> bool {
    if suppress_input {
        return false;
    }

    let Some((_sx, sy)) = window.get_scroll_wheel() else {
        return false;
    };

    if sy.abs() < f32::EPSILON {
        return false;
    }

    let Some((x, y)) = window.get_unscaled_mouse_pos(MouseMode::Clamp) else {
        return false;
    };

    let x = x as usize;
    let y = y as usize;

    let Some((pane_index, _rect)) = layout.pane_at(x, y) else {
        return false;
    };

    let delta_y = (-sy / 40.0).round().clamp(-64.0, 64.0) as isize;

    if delta_y != 0 {
        panes[pane_index].scroll_viewport(delta_y);
        return true;
    }

    false
}

pub(super) fn forward_special_keys(
    pressed_keys: &[Key],
    window: &Window,
    pane: &mut TerminalPane,
    skip_layout_shortcut: bool,
) -> Result<bool> {
    let ctrl = window.is_key_down(Key::LeftCtrl) || window.is_key_down(Key::RightCtrl);
    let shift = window.is_key_down(Key::LeftShift) || window.is_key_down(Key::RightShift);
    let super_key = window.is_key_down(Key::LeftSuper) || window.is_key_down(Key::RightSuper);
    let mut wrote = false;

    for key in pressed_keys.iter().copied() {
        if skip_layout_shortcut
            && (pane_layout_shortcut(key).is_some() || pane_navigation_shortcut(key).is_some())
        {
            continue;
        }

        if ctrl && key == Key::V && shift {
            continue;
        }

        if key == Key::Insert && shift {
            continue;
        }

        // Super+Insert paste (or stray Insert with Super held): never send <Insert> to the shell —
        // in Vim insert mode that toggles replace mode.
        if key == Key::Insert && super_key {
            continue;
        }

        if super_key && key == Key::V {
            continue;
        }

        if ctrl {
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
