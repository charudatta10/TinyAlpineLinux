//! Keyboard encoding: winit key events → VT/xterm byte sequences.

use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Encode a key event. `app_cursor` selects SS3 arrows (application cursor
/// keys mode); returns `None` when the key has no terminal meaning.
pub fn encode_key(ev: &KeyEvent, mods: ModifiersState, app_cursor: bool) -> Option<Vec<u8>> {
    if ev.state != ElementState::Pressed {
        return None;
    }
    let ctrl = mods.control_key();
    let alt = mods.alt_key();

    // Named keys first.
    if let Key::Named(named) = &ev.logical_key {
        let seq: Vec<u8> = match named {
            NamedKey::Enter => b"\r".to_vec(),
            NamedKey::Tab => b"\t".to_vec(),
            NamedKey::Backspace => b"\x7f".to_vec(),
            NamedKey::Escape => b"\x1b".to_vec(),
            NamedKey::Delete => b"\x1b[3~".to_vec(),
            NamedKey::Insert => b"\x1b[2~".to_vec(),
            NamedKey::Home => b"\x1b[H".to_vec(),
            NamedKey::End => b"\x1b[F".to_vec(),
            NamedKey::PageUp => b"\x1b[5~".to_vec(),
            NamedKey::PageDown => b"\x1b[6~".to_vec(),
            NamedKey::ArrowUp => arrow(b'A', app_cursor),
            NamedKey::ArrowDown => arrow(b'B', app_cursor),
            NamedKey::ArrowRight => arrow(b'C', app_cursor),
            NamedKey::ArrowLeft => arrow(b'D', app_cursor),
            NamedKey::F1 => b"\x1bOP".to_vec(),
            NamedKey::F2 => b"\x1bOQ".to_vec(),
            NamedKey::F3 => b"\x1bOR".to_vec(),
            NamedKey::F4 => b"\x1bOS".to_vec(),
            NamedKey::F5 => b"\x1b[15~".to_vec(),
            NamedKey::F6 => b"\x1b[17~".to_vec(),
            NamedKey::F7 => b"\x1b[18~".to_vec(),
            NamedKey::F8 => b"\x1b[19~".to_vec(),
            NamedKey::F9 => b"\x1b[20~".to_vec(),
            NamedKey::F10 => b"\x1b[21~".to_vec(),
            NamedKey::F11 => b"\x1b[23~".to_vec(),
            NamedKey::F12 => b"\x1b[24~".to_vec(),
            NamedKey::Space => b" ".to_vec(),
            _ => return None,
        };
        return Some(seq);
    }

    // Text characters.
    if let Key::Character(s) = &ev.logical_key {
        if ctrl {
            let c = s.chars().next()?;
            let byte = ctrl_char(c)?;
            return Some(vec![byte]);
        }
        let mut out = Vec::new();
        if alt {
            out.push(0x1b);
        }
        out.extend(s.as_bytes());
        return Some(out);
    }
    None
}

fn arrow(final_byte: u8, app_cursor: bool) -> Vec<u8> {
    if app_cursor {
        vec![0x1b, b'O', final_byte]
    } else {
        vec![0x1b, b'[', final_byte]
    }
}

/// Ctrl+<c> → control byte (0x00..=0x1f, 0x7f).
fn ctrl_char(c: char) -> Option<u8> {
    let b = match c {
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        ' ' | '@' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '/' | '7' | '-' => 0x1f,
        '8' => 0x7f,
        _ => return None,
    };
    Some(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_letters() {
        assert_eq!(ctrl_char('c'), Some(3));
        assert_eq!(ctrl_char('z'), Some(26));
        assert_eq!(ctrl_char('['), Some(27));
        assert_eq!(ctrl_char('/'), Some(31));
        assert_eq!(ctrl_char('9'), None);
    }

    #[test]
    fn arrow_modes() {
        assert_eq!(arrow(b'A', false), b"\x1b[A");
        assert_eq!(arrow(b'A', true), b"\x1bOA");
    }
}
