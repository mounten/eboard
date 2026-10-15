//! Keyboards with a built-in US layout. Produces `Key` events (for shortcuts and navigation) and
//! `Text` events (for text input), plus `Copy`/`Cut`/`Paste` for the usual shortcuts.
//!
//! Other layouts need xkbcommon (planned behind an `xkb` feature).

use egui::{Event, Key, Modifiers};

use super::Shared;
use super::codes::*;

#[derive(Default)]
pub(crate) struct KeyboardDecoder {
    shift: [bool; 2],
    ctrl: [bool; 2],
    alt: [bool; 2],
    meta: [bool; 2],
    caps_lock: bool,
}

impl KeyboardDecoder {
    pub(crate) fn event(&mut self, ty: u16, code: u16, value: i32, shared: &mut Shared) {
        if ty == EV_SYN && code == SYN_DROPPED {
            // Key releases may be lost; don't keep modifiers stuck.
            *self = Self {
                caps_lock: self.caps_lock,
                ..Self::default()
            };
            shared.modifiers = self.modifiers();
            return;
        }
        if ty != EV_KEY {
            return;
        }
        // 0 = release, 1 = press, 2 = autorepeat
        let pressed = value != 0;
        let repeat = value == 2;

        if self.update_modifiers(code, pressed, repeat) {
            shared.modifiers = self.modifiers();
            shared
                .events
                .push(Event::ModifiersChanged(shared.modifiers));
        }
        let modifiers = shared.modifiers;
        let (key, text) = lookup(code);

        if pressed && let Some(key) = key {
            // Shortcuts egui expects as dedicated events (egui-winit does the same).
            let shortcut = match (key, modifiers.ctrl, modifiers.shift) {
                (Key::X, true, false) | (Key::Delete, false, true) => Some(Event::Cut),
                (Key::C, true, false) | (Key::Insert, true, false) => Some(Event::Copy),
                (Key::V, true, false) | (Key::Insert, false, true) => {
                    (!shared.clipboard.is_empty()).then(|| Event::Paste(shared.clipboard.clone()))
                }
                _ => None,
            };
            shared.events.extend(shortcut);
        }
        if let Some(key) = key {
            shared.events.push(Event::Key {
                key,
                physical_key: Some(key),
                pressed,
                repeat,
                modifiers,
            });
        }
        if pressed
            && !modifiers.ctrl
            && !modifiers.alt
            && !self.meta.contains(&true)
            && let Some((lower, upper)) = text
        {
            let letter = lower.is_ascii_lowercase();
            let shifted = modifiers.shift ^ (letter && self.caps_lock);
            let c = if shifted { upper } else { lower };
            shared.events.push(Event::Text(c.to_string()));
        }
    }

    /// Returns true if the modifier state changed.
    fn update_modifiers(&mut self, code: u16, pressed: bool, repeat: bool) -> bool {
        let (state, side) = match code {
            KEY_LEFTSHIFT => (&mut self.shift, 0),
            KEY_RIGHTSHIFT => (&mut self.shift, 1),
            KEY_LEFTCTRL => (&mut self.ctrl, 0),
            KEY_RIGHTCTRL => (&mut self.ctrl, 1),
            KEY_LEFTALT => (&mut self.alt, 0),
            KEY_RIGHTALT => (&mut self.alt, 1),
            KEY_LEFTMETA => (&mut self.meta, 0),
            KEY_RIGHTMETA => (&mut self.meta, 1),
            KEY_CAPSLOCK => {
                if pressed && !repeat {
                    self.caps_lock = !self.caps_lock;
                }
                return false;
            }
            _ => return false,
        };
        let changed = state[side] != pressed;
        state[side] = pressed;
        changed
    }

    fn modifiers(&self) -> Modifiers {
        let ctrl = self.ctrl.contains(&true);
        Modifiers {
            alt: self.alt.contains(&true),
            ctrl,
            shift: self.shift.contains(&true),
            mac_cmd: false,
            command: ctrl,
        }
    }
}

/// The egui key and the (unshifted, shifted) character for a key code, US layout.
fn lookup(code: u16) -> (Option<Key>, Option<(char, char)>) {
    const LETTERS: [(u16, Key); 26] = [
        (KEY_A, Key::A),
        (KEY_B, Key::B),
        (KEY_C, Key::C),
        (KEY_D, Key::D),
        (KEY_E, Key::E),
        (KEY_F, Key::F),
        (KEY_G, Key::G),
        (KEY_H, Key::H),
        (KEY_I, Key::I),
        (KEY_J, Key::J),
        (KEY_K, Key::K),
        (KEY_L, Key::L),
        (KEY_M, Key::M),
        (KEY_N, Key::N),
        (KEY_O, Key::O),
        (KEY_P, Key::P),
        (KEY_Q, Key::Q),
        (KEY_R, Key::R),
        (KEY_S, Key::S),
        (KEY_T, Key::T),
        (KEY_U, Key::U),
        (KEY_V, Key::V),
        (KEY_W, Key::W),
        (KEY_X, Key::X),
        (KEY_Y, Key::Y),
        (KEY_Z, Key::Z),
    ];
    if let Some(&(_, key)) = LETTERS.iter().find(|(c, _)| *c == code) {
        let lower = key
            .name()
            .to_ascii_lowercase()
            .chars()
            .next()
            .unwrap_or('?');
        return (Some(key), Some((lower, lower.to_ascii_uppercase())));
    }

    // Number row: KEY_1..KEY_9, KEY_0.
    if (KEY_1..=KEY_0).contains(&code) {
        const DIGITS: [(Key, char, char); 10] = [
            (Key::Num1, '1', '!'),
            (Key::Num2, '2', '@'),
            (Key::Num3, '3', '#'),
            (Key::Num4, '4', '$'),
            (Key::Num5, '5', '%'),
            (Key::Num6, '6', '^'),
            (Key::Num7, '7', '&'),
            (Key::Num8, '8', '*'),
            (Key::Num9, '9', '('),
            (Key::Num0, '0', ')'),
        ];
        let (key, a, b) = DIGITS[usize::from(code - KEY_1)];
        return (Some(key), Some((a, b)));
    }

    if (KEY_F1..=KEY_F10).contains(&code) {
        const F: [Key; 10] = [
            Key::F1,
            Key::F2,
            Key::F3,
            Key::F4,
            Key::F5,
            Key::F6,
            Key::F7,
            Key::F8,
            Key::F9,
            Key::F10,
        ];
        return (Some(F[usize::from(code - KEY_F1)]), None);
    }

    let (key, text): (Option<Key>, Option<(char, char)>) = match code {
        KEY_ESC => (Some(Key::Escape), None),
        KEY_MINUS => (Some(Key::Minus), Some(('-', '_'))),
        KEY_EQUAL => (Some(Key::Equals), Some(('=', '+'))),
        KEY_BACKSPACE => (Some(Key::Backspace), None),
        KEY_TAB => (Some(Key::Tab), None),
        KEY_LEFTBRACE => (Some(Key::OpenBracket), Some(('[', '{'))),
        KEY_RIGHTBRACE => (Some(Key::CloseBracket), Some((']', '}'))),
        KEY_ENTER | KEY_KPENTER => (Some(Key::Enter), None),
        KEY_SEMICOLON => (Some(Key::Semicolon), Some((';', ':'))),
        KEY_APOSTROPHE => (Some(Key::Quote), Some(('\'', '"'))),
        KEY_GRAVE => (Some(Key::Backtick), Some(('`', '~'))),
        KEY_BACKSLASH => (Some(Key::Backslash), Some(('\\', '|'))),
        KEY_COMMA => (Some(Key::Comma), Some((',', '<'))),
        KEY_DOT => (Some(Key::Period), Some(('.', '>'))),
        KEY_SLASH => (Some(Key::Slash), Some(('/', '?'))),
        KEY_SPACE => (Some(Key::Space), Some((' ', ' '))),
        KEY_F11 => (Some(Key::F11), None),
        KEY_F12 => (Some(Key::F12), None),
        KEY_HOME => (Some(Key::Home), None),
        KEY_END => (Some(Key::End), None),
        KEY_PAGEUP => (Some(Key::PageUp), None),
        KEY_PAGEDOWN => (Some(Key::PageDown), None),
        KEY_UP => (Some(Key::ArrowUp), None),
        KEY_DOWN => (Some(Key::ArrowDown), None),
        KEY_LEFT => (Some(Key::ArrowLeft), None),
        KEY_RIGHT => (Some(Key::ArrowRight), None),
        KEY_INSERT => (Some(Key::Insert), None),
        KEY_DELETE => (Some(Key::Delete), None),
        // Keypad, as if Num Lock were on.
        KEY_KP0 => (Some(Key::Num0), Some(('0', '0'))),
        KEY_KP1 => (Some(Key::Num1), Some(('1', '1'))),
        KEY_KP2 => (Some(Key::Num2), Some(('2', '2'))),
        KEY_KP3 => (Some(Key::Num3), Some(('3', '3'))),
        KEY_KP4 => (Some(Key::Num4), Some(('4', '4'))),
        KEY_KP5 => (Some(Key::Num5), Some(('5', '5'))),
        KEY_KP6 => (Some(Key::Num6), Some(('6', '6'))),
        KEY_KP7 => (Some(Key::Num7), Some(('7', '7'))),
        KEY_KP8 => (Some(Key::Num8), Some(('8', '8'))),
        KEY_KP9 => (Some(Key::Num9), Some(('9', '9'))),
        KEY_KPDOT => (Some(Key::Period), Some(('.', '.'))),
        KEY_KPPLUS => (Some(Key::Plus), Some(('+', '+'))),
        KEY_KPMINUS => (Some(Key::Minus), Some(('-', '-'))),
        KEY_KPASTERISK => (None, Some(('*', '*'))),
        KEY_KPSLASH => (Some(Key::Slash), Some(('/', '/'))),
        _ => (None, None),
    };
    (key, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rotation;

    fn press(d: &mut KeyboardDecoder, s: &mut Shared, code: u16, value: i32) -> Vec<Event> {
        d.event(EV_KEY, code, value, s);
        d.event(EV_SYN, SYN_REPORT, 0, s);
        std::mem::take(&mut s.events)
    }

    fn texts(events: &[Event]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    fn setup() -> (KeyboardDecoder, Shared) {
        (
            KeyboardDecoder::default(),
            Shared::new([800, 600], Rotation::Deg0),
        )
    }

    #[test]
    fn typing_with_shift_and_caps_lock() {
        let (mut d, mut s) = setup();
        let mut typed = String::new();
        typed += &texts(&press(&mut d, &mut s, KEY_H, 1));
        press(&mut d, &mut s, KEY_LEFTSHIFT, 1);
        typed += &texts(&press(&mut d, &mut s, KEY_I, 1));
        typed += &texts(&press(&mut d, &mut s, KEY_1, 1));
        press(&mut d, &mut s, KEY_LEFTSHIFT, 0);
        press(&mut d, &mut s, KEY_CAPSLOCK, 1);
        typed += &texts(&press(&mut d, &mut s, KEY_A, 1));
        typed += &texts(&press(&mut d, &mut s, KEY_1 + 1, 1)); // "2": caps lock only affects letters
        typed += &texts(&press(&mut d, &mut s, KEY_SPACE, 1));
        assert_eq!(typed, "hI!A2 ");
    }

    #[test]
    fn key_events_carry_press_release_and_repeat() {
        let (mut d, mut s) = setup();
        let events = press(&mut d, &mut s, KEY_BACKSPACE, 2);
        assert!(matches!(
            events[..],
            [Event::Key {
                key: Key::Backspace,
                pressed: true,
                repeat: true,
                ..
            }]
        ));
        let events = press(&mut d, &mut s, KEY_BACKSPACE, 0);
        assert!(matches!(
            events[..],
            [Event::Key {
                pressed: false,
                repeat: false,
                ..
            }]
        ));
    }

    #[test]
    fn ctrl_shortcuts() {
        let (mut d, mut s) = setup();
        let events = press(&mut d, &mut s, KEY_LEFTCTRL, 1);
        assert!(matches!(events[..], [Event::ModifiersChanged(m)] if m.ctrl && m.command));
        let events = press(&mut d, &mut s, KEY_C, 1);
        assert!(matches!(events[0], Event::Copy));
        assert!(texts(&events).is_empty(), "no text while ctrl is held");

        // Paste needs something on our clipboard.
        assert!(
            !press(&mut d, &mut s, KEY_V, 1)
                .iter()
                .any(|e| matches!(e, Event::Paste(_)))
        );
        s.clipboard = "hello".into();
        let events = press(&mut d, &mut s, KEY_V, 1);
        assert!(matches!(&events[0], Event::Paste(t) if t == "hello"));
    }

    #[test]
    fn every_letter_maps_to_itself() {
        let codes = [
            (KEY_Q, 'q'),
            (KEY_W, 'w'),
            (KEY_Z, 'z'),
            (KEY_M, 'm'),
            (KEY_L, 'l'),
        ];
        for (code, c) in codes {
            let (key, text) = lookup(code);
            assert_eq!(text, Some((c, c.to_ascii_uppercase())));
            assert_eq!(
                key.map(|k| k.name().to_ascii_lowercase()),
                Some(c.to_string())
            );
        }
    }
}
