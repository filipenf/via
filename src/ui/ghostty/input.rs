use anyhow::Result;
use crossbeam_channel::{Receiver, Sender};
use minifb::{InputCallback, Key, Window};

use super::layout::{pane_layout_shortcut, pane_navigation_shortcut};
use super::pane::TerminalPane;

pub(super) struct TextInput {
    input: Sender<char>,
}

impl TextInput {
    pub(super) fn new(input: Sender<char>) -> Self {
        Self { input }
    }
}

impl InputCallback for TextInput {
    fn add_char(&mut self, uni_char: u32) {
        if let Some(ch) = char::from_u32(uni_char) {
            let _ = self.input.send(ch);
        }
    }
}

pub(super) fn forward_text_input(
    input: &Receiver<char>,
    pane: &mut TerminalPane,
    suppress_input: bool,
) -> Result<()> {
    for ch in input.try_iter() {
        if suppress_input {
            continue;
        }

        let mut bytes = [0; 4];
        pane.write_all(ch.encode_utf8(&mut bytes).as_bytes())?;
    }

    Ok(())
}

pub(super) fn forward_special_keys(
    pressed_keys: &[Key],
    window: &Window,
    pane: &mut TerminalPane,
    skip_layout_shortcut: bool,
) -> Result<()> {
    let ctrl = window.is_key_down(Key::LeftCtrl) || window.is_key_down(Key::RightCtrl);

    for key in pressed_keys.iter().copied() {
        if skip_layout_shortcut
            && (pane_layout_shortcut(key).is_some() || pane_navigation_shortcut(key).is_some())
        {
            continue;
        }

        if ctrl {
            if let Some(bytes) = ctrl_sequence(key) {
                pane.write_all(&bytes)?;
                continue;
            }
        }

        if let Some(bytes) = key_sequence(key) {
            pane.write_all(bytes)?;
        }
    }

    Ok(())
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
