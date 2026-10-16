//! On-screen keyboard for touch-only devices (feature `osk`).
//!
//! It is not built from egui widgets: tapping a widget would take the focus away from the
//! `TextEdit` being typed into. Instead [`Osk::filter`] sits between the input devices and egui.
//! Presses on the keyboard never reach egui; releasing on the key that was pressed sends
//! `Event::Text` or `Event::Key` instead, as a hardware keyboard would. Everything runs in egui
//! points, so rotation and scale need no special handling.
//!
//! The keyboard is shown while egui wants text input (`PlatformOutput::ime`), and in
//! [`OskMode::Auto`] only while no hardware keyboard is attached. While it is shown, the bottom
//! safe-area inset makes room for it, so panels lay out above it.

use std::collections::BTreeSet;

use egui::{
    Event, Key, Modifiers, PointerButton, Pos2, Rect, TouchId, TouchPhase, Vec2, pos2, vec2,
};

use crate::OskMode;

/// Largest key cell in points; on big screens the keyboard doesn't stretch further.
const MAX_KEY_WIDTH: f32 = 96.0;
const MAX_KEY_HEIGHT: f32 = 60.0;
/// Space around the key grid, in points.
const PADDING: f32 = 4.0;
/// Width of a row in key units.
const ROW_UNITS: f32 = 10.0;
/// A second tap on shift within this time (seconds) locks capitals.
const DOUBLE_TAP: f64 = 0.4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    /// Types the character (letters follow shift).
    Char(char),
    Space,
    Backspace,
    Enter,
    Shift,
    /// Switches between letters and symbols.
    Page,
    /// Hides the keyboard by dropping the text focus (Escape).
    Hide,
}

#[derive(Clone, Copy, Debug)]
struct KeyDef {
    action: Action,
    /// Width in key units.
    width: f32,
}

const fn key(action: Action, width: f32) -> KeyDef {
    KeyDef { action, width }
}

const fn chars<const N: usize>(text: [char; N]) -> [KeyDef; N] {
    let mut keys = [key(Action::Space, 1.0); N];
    let mut i = 0;
    while i < N {
        keys[i] = key(Action::Char(text[i]), 1.0);
        i += 1;
    }
    keys
}

const LETTERS_1: [KeyDef; 10] = chars(['q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p']);
const LETTERS_2: [KeyDef; 9] = chars(['a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l']);
const LETTERS_3: [KeyDef; 9] = {
    let middle = chars(['z', 'x', 'c', 'v', 'b', 'n', 'm']);
    let mut row = [key(Action::Shift, 1.5); 9];
    let mut i = 0;
    while i < 7 {
        row[i + 1] = middle[i];
        i += 1;
    }
    row[8] = key(Action::Backspace, 1.5);
    row
};
const SYMBOLS_1: [KeyDef; 10] = chars(['1', '2', '3', '4', '5', '6', '7', '8', '9', '0']);
const SYMBOLS_2: [KeyDef; 10] = chars(['@', '#', '$', '%', '&', '*', '-', '+', '(', ')']);
const SYMBOLS_3: [KeyDef; 9] = {
    let middle = chars(['=', '!', '"', '\'', ':', ';', '/', '?']);
    let mut row = [key(Action::Backspace, 2.0); 9];
    let mut i = 0;
    while i < 8 {
        row[i] = middle[i];
        i += 1;
    }
    row
};
const BOTTOM: [KeyDef; 6] = [
    key(Action::Page, 1.5),
    key(Action::Char(','), 1.0),
    key(Action::Space, 4.0),
    key(Action::Char('.'), 1.0),
    key(Action::Hide, 1.0),
    key(Action::Enter, 1.5),
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Page {
    #[default]
    Letters,
    Symbols,
}

impl Page {
    fn rows(self) -> [&'static [KeyDef]; 4] {
        match self {
            Self::Letters => [&LETTERS_1, &LETTERS_2, &LETTERS_3, &BOTTOM],
            Self::Symbols => [&SYMBOLS_1, &SYMBOLS_2, &SYMBOLS_3, &BOTTOM],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Shift {
    #[default]
    Off,
    /// The next letter is a capital.
    Once,
    /// Caps lock (double tap).
    Locked,
}

/// Who gets the current pointer gesture.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Owner {
    #[default]
    None,
    /// It started outside the keyboard.
    Egui,
    /// It started on the keyboard.
    Keyboard,
}

/// Where the keyboard is, for a screen size in points.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Layout {
    /// The whole keyboard area, across the bottom of the screen.
    rect: Rect,
    /// Top left of the key grid (centered when the keys hit their maximum size).
    origin: Pos2,
    unit: f32,
    row_height: f32,
}

impl Layout {
    fn new(screen: Vec2) -> Self {
        let fraction = if screen.y > screen.x { 0.33 } else { 0.40 };
        let row_height = ((screen.y * fraction - 2.0 * PADDING) / 4.0).clamp(1.0, MAX_KEY_HEIGHT);
        let unit = ((screen.x - 2.0 * PADDING) / ROW_UNITS).clamp(1.0, MAX_KEY_WIDTH);
        let height = 4.0 * row_height + 2.0 * PADDING;
        let rect = Rect::from_min_max(pos2(0.0, screen.y - height), pos2(screen.x, screen.y));
        let origin = pos2((screen.x - ROW_UNITS * unit) / 2.0, rect.top() + PADDING);
        Self {
            rect,
            origin,
            unit,
            row_height,
        }
    }

    /// The cells of all keys of `page`, row by row. Cells touch each other, so every tap on
    /// the key grid hits a key.
    fn keys(&self, page: Page) -> Vec<(Action, Rect)> {
        let mut keys = Vec::new();
        for (row, defs) in page.rows().iter().enumerate() {
            let units: f32 = defs.iter().map(|d| d.width).sum();
            let mut x = self.origin.x + (ROW_UNITS - units) / 2.0 * self.unit;
            let y = self.origin.y + row as f32 * self.row_height;
            for def in *defs {
                let width = def.width * self.unit;
                keys.push((
                    def.action,
                    Rect::from_min_size(pos2(x, y), vec2(width, self.row_height)),
                ));
                x += width;
            }
        }
        keys
    }
}

/// The on-screen keyboard: visibility, layout, and the input filter.
#[derive(Debug, Default)]
pub(crate) struct Osk {
    mode: OskMode,
    visible: bool,
    page: Page,
    shift: Shift,
    /// Time of the last shift tap, for double taps.
    last_shift_tap: Option<f64>,
    owner: Owner,
    /// Index into [`Layout::keys`] of the key held down.
    pressed: Option<usize>,
    /// The pointer is still on the pressed key (releasing types it).
    on_pressed: bool,
    /// The (not pressed) pointer is over the keyboard; egui was told it is gone.
    pointer_inside: bool,
    /// Touches that started on the keyboard.
    touches: BTreeSet<u64>,
}

impl Osk {
    pub(crate) fn new(mode: OskMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    pub(crate) fn is_visible(&self) -> bool {
        self.visible
    }

    /// Shows or hides the keyboard after an egui pass. Returns true if that changed (repaint).
    pub(crate) fn update(&mut self, wants_text: bool, has_keyboard: bool) -> bool {
        let visible = match self.mode {
            OskMode::Off => false,
            OskMode::Auto => wants_text && !has_keyboard,
            OskMode::Always => wants_text,
        };
        if visible == self.visible {
            return false;
        }
        *self = Self {
            mode: self.mode,
            visible,
            ..Self::default()
        };
        true
    }

    /// Height of the keyboard in points (0 while hidden), for the bottom safe-area inset.
    pub(crate) fn inset(&self, screen: Vec2) -> f32 {
        if self.visible {
            Layout::new(screen).rect.height()
        } else {
            0.0
        }
    }

    /// Takes the pointer and touch input meant for the keyboard out of `events`, and adds the
    /// text and keys typed on it. `screen` is the screen size in points, `time` egui's input time
    /// in seconds.
    pub(crate) fn filter(&mut self, events: Vec<Event>, screen: Vec2, time: f64) -> Vec<Event> {
        if !self.visible {
            return events;
        }
        let layout = Layout::new(screen);
        let keys = layout.keys(self.page);
        let hit = |pos: Pos2| keys.iter().position(|(_, rect)| rect.contains(pos));
        let inside = |pos: Pos2| layout.rect.contains(pos);

        let mut out = Vec::with_capacity(events.len());
        for event in events {
            match &event {
                Event::PointerMoved(pos) => match self.owner {
                    Owner::Keyboard => self.on_pressed = hit(*pos) == self.pressed,
                    Owner::Egui => out.push(event),
                    Owner::None if inside(*pos) => {
                        if !std::mem::replace(&mut self.pointer_inside, true) {
                            out.push(Event::PointerGone);
                        }
                    }
                    Owner::None => {
                        self.pointer_inside = false;
                        out.push(event);
                    }
                },
                Event::PointerButton {
                    pos,
                    button: PointerButton::Primary,
                    pressed: true,
                    ..
                } => {
                    if self.owner == Owner::None && inside(*pos) {
                        self.owner = Owner::Keyboard;
                        self.pressed = hit(*pos);
                        self.on_pressed = true;
                    } else {
                        if self.owner == Owner::None {
                            self.owner = Owner::Egui;
                        }
                        out.push(event);
                    }
                }
                Event::PointerButton {
                    button: PointerButton::Primary,
                    pressed: false,
                    ..
                } => {
                    if self.owner == Owner::Keyboard {
                        if self.on_pressed
                            && let Some(index) = self.pressed
                        {
                            self.press(keys[index].0, time, &mut out);
                        }
                        self.pressed = None;
                    } else {
                        out.push(event);
                    }
                    self.owner = Owner::None;
                }
                Event::PointerButton { pos, .. } => {
                    // Other buttons: dropped on the keyboard unless egui owns the gesture.
                    if self.owner == Owner::Egui || !inside(*pos) {
                        out.push(event);
                    }
                }
                Event::PointerGone => {
                    self.owner = Owner::None;
                    self.pressed = None;
                    self.pointer_inside = false;
                    out.push(event);
                }
                Event::Touch {
                    id: TouchId(id),
                    phase,
                    pos,
                    ..
                } => {
                    let captured = match phase {
                        TouchPhase::Start => inside(*pos) && self.touches.insert(*id),
                        TouchPhase::Move => self.touches.contains(id),
                        TouchPhase::End | TouchPhase::Cancel => self.touches.remove(id),
                    };
                    if !captured {
                        out.push(event);
                    }
                }
                _ => out.push(event),
            }
        }
        out
    }

    /// A key was tapped.
    fn press(&mut self, action: Action, time: f64, out: &mut Vec<Event>) {
        let key = |out: &mut Vec<Event>, key: Key| {
            for pressed in [true, false] {
                out.push(Event::Key {
                    key,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers: Modifiers::default(),
                });
            }
        };
        match action {
            Action::Char(c) => {
                let text = if self.shift == Shift::Off {
                    c.to_string()
                } else {
                    c.to_uppercase().collect()
                };
                out.push(Event::Text(text));
                if self.shift == Shift::Once {
                    self.shift = Shift::Off;
                }
            }
            Action::Space => out.push(Event::Text(" ".to_owned())),
            Action::Backspace => key(out, Key::Backspace),
            Action::Enter => key(out, Key::Enter),
            Action::Hide => key(out, Key::Escape),
            Action::Page => {
                self.page = match self.page {
                    Page::Letters => Page::Symbols,
                    Page::Symbols => Page::Letters,
                };
            }
            Action::Shift => {
                let double_tap = self
                    .last_shift_tap
                    .is_some_and(|last| time - last < DOUBLE_TAP);
                self.shift = match self.shift {
                    Shift::Off => Shift::Once,
                    Shift::Once if double_tap => Shift::Locked,
                    Shift::Once | Shift::Locked => Shift::Off,
                };
                self.last_shift_tap = Some(time);
            }
        }
    }

    /// Draws the keyboard over everything except the mouse cursor.
    pub(crate) fn paint(&self, ctx: &egui::Context) {
        if !self.visible {
            return;
        }
        let screen = ctx.viewport_rect();
        let layout = Layout::new(screen.size());
        // `Context::layer_painter` would clip to the content rect, which ends above us.
        let painter = egui::Painter::new(
            ctx.clone(),
            egui::LayerId::new(egui::Order::Tooltip, egui::Id::unique("eboard_osk")),
            screen,
        );
        let visuals = &ctx.global_style().visuals;
        painter.rect_filled(layout.rect, 0.0, visuals.extreme_bg_color);
        painter.hline(
            layout.rect.x_range(),
            layout.rect.top(),
            visuals.window_stroke,
        );

        let font = egui::FontId::proportional((layout.row_height * 0.4).clamp(8.0, 24.0));
        let gap = (layout.unit.min(layout.row_height) * 0.06).clamp(1.0, 4.0);
        let color = visuals.text_color();
        for (index, (action, cell)) in layout.keys(self.page).into_iter().enumerate() {
            let held = self.pressed == Some(index) && self.on_pressed;
            let fill = if held {
                visuals.widgets.active.weak_bg_fill
            } else if action == Action::Shift && self.shift != Shift::Off {
                visuals.selection.bg_fill
            } else if matches!(action, Action::Char(_) | Action::Space) {
                visuals.widgets.inactive.weak_bg_fill
            } else {
                visuals.widgets.noninteractive.weak_bg_fill
            };
            let rect = cell.shrink(gap);
            painter.rect_filled(rect, 4.0, fill);
            let text = match action {
                Action::Char(c) if self.shift != Shift::Off => c.to_uppercase().collect(),
                Action::Char(c) => c.to_string(),
                Action::Page if self.page == Page::Letters => "?123".to_owned(),
                Action::Page => "ABC".to_owned(),
                _ => {
                    // Drawn, not text: the arrows aren't in egui's default fonts.
                    let size = (rect.height() * 0.4).min(rect.width() * 0.6);
                    paint_icon(&painter, action, self.shift, rect.center(), size, color);
                    continue;
                }
            };
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                text,
                font.clone(),
                color,
            );
        }
    }
}

/// Draws the symbol of a special key, `size` points tall, centered on `c`.
fn paint_icon(
    painter: &egui::Painter,
    action: Action,
    shift: Shift,
    c: Pos2,
    size: f32,
    color: egui::Color32,
) {
    let stroke = egui::Stroke::new((size / 10.0).max(1.5), color);
    let p = |x: f32, y: f32| c + vec2(x, y) * size;
    match action {
        Action::Shift => {
            let head = vec![p(0.0, -0.5), p(0.5, 0.0), p(-0.5, 0.0)];
            let stem = vec![p(-0.22, 0.0), p(0.22, 0.0), p(0.22, 0.4), p(-0.22, 0.4)];
            if shift == Shift::Off {
                let mut outline = vec![p(0.0, -0.5), p(0.5, 0.0), p(0.22, 0.0), p(0.22, 0.4)];
                outline.extend([p(-0.22, 0.4), p(-0.22, 0.0), p(-0.5, 0.0)]);
                painter.add(egui::Shape::closed_line(outline, stroke));
            } else {
                // The arrow isn't convex: fill it as head + stem.
                painter.add(egui::Shape::convex_polygon(head, color, egui::Stroke::NONE));
                painter.add(egui::Shape::convex_polygon(stem, color, egui::Stroke::NONE));
            }
            if shift == Shift::Locked {
                painter.line_segment([p(-0.3, 0.6), p(0.3, 0.6)], stroke);
            }
        }
        Action::Backspace => {
            let outline = vec![
                p(-0.6, 0.0),
                p(-0.3, -0.35),
                p(0.5, -0.35),
                p(0.5, 0.35),
                p(-0.3, 0.35),
            ];
            painter.add(egui::Shape::closed_line(outline, stroke));
            painter.line_segment([p(-0.05, -0.15), p(0.25, 0.15)], stroke);
            painter.line_segment([p(-0.05, 0.15), p(0.25, -0.15)], stroke);
        }
        Action::Enter => {
            painter.add(egui::Shape::line(
                vec![p(0.5, -0.4), p(0.5, 0.15), p(-0.45, 0.15)],
                stroke,
            ));
            painter.add(egui::Shape::line(
                vec![p(-0.2, -0.1), p(-0.5, 0.15), p(-0.2, 0.4)],
                stroke,
            ));
        }
        Action::Hide => {
            painter.add(egui::Shape::line(
                vec![p(-0.4, -0.2), p(0.0, 0.2), p(0.4, -0.2)],
                stroke,
            ));
        }
        Action::Char(_) | Action::Space | Action::Page => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LANDSCAPE: Vec2 = vec2(1024.0, 576.0);
    const PORTRAIT: Vec2 = vec2(576.0, 1024.0);

    fn shown() -> Osk {
        let mut osk = Osk::new(OskMode::Auto);
        assert!(osk.update(true, false));
        osk
    }

    /// Center of the key with this action on the current page.
    fn center(osk: &Osk, screen: Vec2, action: Action) -> Pos2 {
        Layout::new(screen)
            .keys(osk.page)
            .into_iter()
            .find(|(a, _)| *a == action)
            .unwrap_or_else(|| panic!("no {action:?} key"))
            .1
            .center()
    }

    fn button(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::default(),
        }
    }

    fn touch(id: u64, phase: TouchPhase, pos: Pos2) -> Event {
        Event::Touch {
            device_id: egui::TouchDeviceId(0),
            id: TouchId(id),
            phase,
            pos,
            force: None,
        }
    }

    /// What the touch decoder emits for a tap of the first finger.
    fn tap_events(pos: Pos2) -> Vec<Event> {
        vec![
            touch(0, TouchPhase::Start, pos),
            Event::PointerMoved(pos),
            button(pos, true),
            touch(0, TouchPhase::End, pos),
            button(pos, false),
            Event::PointerGone,
        ]
    }

    fn tap(osk: &mut Osk, action: Action, time: f64) -> Vec<Event> {
        let pos = center(osk, LANDSCAPE, action);
        osk.filter(tap_events(pos), LANDSCAPE, time)
    }

    fn typed(events: &[Event]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    fn key_events(key: Key) -> Vec<Event> {
        [true, false]
            .map(|pressed| Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: Modifiers::default(),
            })
            .to_vec()
    }

    #[test]
    fn layout_fits_the_screen() {
        for screen in [
            LANDSCAPE,
            PORTRAIT,
            vec2(320.0, 240.0),
            vec2(1920.0, 1080.0),
        ] {
            let layout = Layout::new(screen);
            let fraction = layout.rect.height() / screen.y;
            assert!(fraction <= 0.41, "{screen:?}: {fraction}");
            assert_eq!(layout.rect.bottom(), screen.y);
            assert!(layout.unit <= MAX_KEY_WIDTH && layout.row_height <= MAX_KEY_HEIGHT);
            for page in [Page::Letters, Page::Symbols] {
                for (action, rect) in layout.keys(page) {
                    assert!(
                        layout.rect.contains_rect(rect.shrink(0.01)),
                        "{screen:?} {action:?} {rect:?}"
                    );
                }
            }
        }
        // Portrait: a third of the height, full-width keys.
        let portrait = Layout::new(PORTRAIT);
        assert!(portrait.rect.height() <= PORTRAIT.y * 0.33 + 0.01);
        assert_eq!(portrait.origin.x, PADDING);
    }

    #[test]
    fn hit_testing_finds_every_key() {
        let osk = shown();
        let layout = Layout::new(LANDSCAPE);
        let keys = layout.keys(Page::Letters);
        assert_eq!(keys.len(), 10 + 9 + 9 + 6);
        for (index, (_, rect)) in keys.iter().enumerate() {
            let found = keys.iter().position(|(_, r)| r.contains(rect.center()));
            assert_eq!(found, Some(index));
        }
        assert_eq!(keys[0].0, Action::Char('q'));
        // 'a' is half a key to the right of 'q'.
        assert_eq!(keys[10].1.left() - keys[0].1.left(), layout.unit / 2.0);
        let _ = osk;
    }

    #[test]
    fn typing_letters_space_and_keys() {
        let mut osk = shown();
        let mut events = tap(&mut osk, Action::Char('h'), 0.0);
        events.extend(tap(&mut osk, Action::Char('i'), 1.0));
        events.extend(tap(&mut osk, Action::Space, 2.0));
        events.extend(tap(&mut osk, Action::Char('.'), 3.0));
        assert_eq!(typed(&events), "hi .");
        // Only the emulated PointerGone of each tap reaches egui besides the text.
        assert!(
            events
                .iter()
                .all(|e| matches!(e, Event::Text(_) | Event::PointerGone))
        );

        let keys = |events: Vec<Event>| -> Vec<Event> {
            events
                .into_iter()
                .filter(|e| matches!(e, Event::Key { .. }))
                .collect()
        };
        assert_eq!(
            keys(tap(&mut osk, Action::Backspace, 4.0)),
            key_events(Key::Backspace)
        );
        assert_eq!(
            keys(tap(&mut osk, Action::Enter, 5.0)),
            key_events(Key::Enter)
        );
        assert_eq!(
            keys(tap(&mut osk, Action::Hide, 6.0)),
            key_events(Key::Escape)
        );
    }

    #[test]
    fn shift_once_and_caps_lock() {
        let mut osk = shown();
        tap(&mut osk, Action::Shift, 0.0);
        assert_eq!(osk.shift, Shift::Once);
        assert_eq!(typed(&tap(&mut osk, Action::Char('a'), 1.0)), "A");
        assert_eq!(typed(&tap(&mut osk, Action::Char('a'), 2.0)), "a");

        // Slow second tap: off again.
        tap(&mut osk, Action::Shift, 3.0);
        tap(&mut osk, Action::Shift, 4.0);
        assert_eq!(osk.shift, Shift::Off);

        // Double tap: caps lock until shift is tapped again.
        tap(&mut osk, Action::Shift, 5.0);
        tap(&mut osk, Action::Shift, 5.2);
        assert_eq!(osk.shift, Shift::Locked);
        assert_eq!(typed(&tap(&mut osk, Action::Char('b'), 6.0)), "B");
        assert_eq!(typed(&tap(&mut osk, Action::Char('c'), 7.0)), "C");
        tap(&mut osk, Action::Shift, 8.0);
        assert_eq!(typed(&tap(&mut osk, Action::Char('d'), 9.0)), "d");
    }

    #[test]
    fn symbols_page() {
        let mut osk = shown();
        tap(&mut osk, Action::Page, 0.0);
        assert_eq!(osk.page, Page::Symbols);
        assert_eq!(typed(&tap(&mut osk, Action::Char('7'), 1.0)), "7");
        assert_eq!(typed(&tap(&mut osk, Action::Char('@'), 2.0)), "@");
        assert_eq!(typed(&tap(&mut osk, Action::Char('"'), 3.0)), "\"");
        tap(&mut osk, Action::Page, 4.0);
        assert_eq!(typed(&tap(&mut osk, Action::Char('q'), 5.0)), "q");
    }

    #[test]
    fn sliding_off_the_key_cancels_it() {
        let mut osk = shown();
        let a = center(&osk, LANDSCAPE, Action::Char('a'));
        let s = center(&osk, LANDSCAPE, Action::Char('s'));
        let events = osk.filter(
            vec![button(a, true), Event::PointerMoved(s), button(s, false)],
            LANDSCAPE,
            0.0,
        );
        assert_eq!(events, []);
        // Back onto the key before releasing: typed.
        let events = osk.filter(
            vec![
                button(a, true),
                Event::PointerMoved(s),
                Event::PointerMoved(a),
                button(a, false),
            ],
            LANDSCAPE,
            1.0,
        );
        assert_eq!(typed(&events), "a");
    }

    #[test]
    fn gesture_from_the_keyboard_to_the_ui_is_consumed() {
        let mut osk = shown();
        let a = center(&osk, LANDSCAPE, Action::Char('a'));
        let above = pos2(500.0, 100.0);
        let events = osk.filter(
            vec![
                touch(0, TouchPhase::Start, a),
                Event::PointerMoved(a),
                button(a, true),
                touch(0, TouchPhase::Move, above),
                Event::PointerMoved(above),
                touch(0, TouchPhase::End, above),
                button(above, false),
            ],
            LANDSCAPE,
            0.0,
        );
        // The pointer entering the keyboard tells egui it's gone; nothing else gets through.
        assert_eq!(events, [Event::PointerGone]);
    }

    #[test]
    fn gesture_from_the_ui_onto_the_keyboard_passes() {
        let mut osk = shown();
        let a = center(&osk, LANDSCAPE, Action::Char('a'));
        let above = pos2(500.0, 100.0);
        let events = vec![
            touch(0, TouchPhase::Start, above),
            Event::PointerMoved(above),
            button(above, true),
            touch(0, TouchPhase::Move, a),
            Event::PointerMoved(a),
            touch(0, TouchPhase::End, a),
            button(a, false),
            Event::PointerGone,
        ];
        assert_eq!(osk.filter(events.clone(), LANDSCAPE, 0.0), events);
        // And taps above the keyboard work as usual.
        assert_eq!(
            osk.filter(tap_events(above), LANDSCAPE, 1.0),
            tap_events(above)
        );
    }

    #[test]
    fn visibility_and_inset() {
        let mut osk = Osk::new(OskMode::Auto);
        assert_eq!(osk.inset(LANDSCAPE), 0.0);
        let events = tap_events(pos2(500.0, 550.0));
        assert_eq!(osk.filter(events.clone(), LANDSCAPE, 0.0), events);

        assert!(osk.update(true, false));
        assert!(!osk.update(true, false));
        assert!(osk.is_visible());
        let inset = osk.inset(LANDSCAPE);
        assert!(inset > 0.0 && inset <= LANDSCAPE.y * 0.4 + 0.01);
        assert_eq!(inset, Layout::new(LANDSCAPE).rect.height());

        // A hardware keyboard hides it in `Auto`, not in `Always`.
        assert!(osk.update(true, true));
        assert_eq!(osk.inset(LANDSCAPE), 0.0);
        let mut always = Osk::new(OskMode::Always);
        assert!(always.update(true, true));
        let mut off = Osk::new(OskMode::Off);
        assert!(!off.update(true, false));

        // No text field focused: hidden, and state is reset for next time.
        let mut osk = shown();
        tap(&mut osk, Action::Page, 0.0);
        assert!(osk.update(false, false));
        assert!(osk.update(true, false));
        assert_eq!(osk.page, Page::Letters);
    }

    /// Renders a text field with the keyboard to `target/osk_snapshot.png` for a visual check:
    /// `cargo test osk_snapshot -- --ignored`.
    #[cfg(all(feature = "soft", feature = "default_fonts"))]
    #[test]
    #[ignore = "writes a file for manual inspection"]
    fn osk_snapshot() {
        use crate::render::Renderer as _;
        use crate::render::soft::{SoftRenderer, png};

        let size_px = [1280u32, 720];
        let ppp = 1.25;
        let screen = vec2(size_px[0] as f32, size_px[1] as f32) / ppp;
        let mut osk = shown();
        tap(&mut osk, Action::Shift, 0.0);
        let ctx = egui::Context::default();
        let mut renderer = SoftRenderer::new();
        let mut text = "Hello".to_owned();
        for time in [0.0, 0.1] {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, screen)),
                time: Some(time),
                safe_area_insets: Some(egui::SafeAreaInsets(egui::epaint::MarginF32 {
                    bottom: osk.inset(screen),
                    ..Default::default()
                })),
                viewports: std::iter::once((
                    egui::ViewportId::ROOT,
                    egui::ViewportInfo {
                        native_pixels_per_point: Some(ppp),
                        ..Default::default()
                    },
                ))
                .collect(),
                ..Default::default()
            };
            let output = ctx.run_pass(input, |ctx| {
                ctx.root_ui(|ui| {
                    let content = ctx.content_rect();
                    ui.scope_builder(egui::UiBuilder::new().max_rect(content), |ui| {
                        egui::CentralPanel::default().show(ui, |ui| {
                            ui.heading("On-screen keyboard");
                            ui.text_edit_singleline(&mut text);
                        });
                    });
                });
                osk.paint(ctx);
            });
            let primitives = ctx.tessellate(output.shapes, output.pixels_per_point);
            let mut textures = output.textures_delta;
            let clear = ctx
                .global_style()
                .visuals
                .panel_fill
                .to_normalized_gamma_f32();
            renderer.paint(size_px, ppp, clear, &primitives, &mut textures);
        }
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/osk_snapshot.png");
        std::fs::write(&path, png::encode(renderer.canvas())).unwrap();
        println!("wrote {}", path.display());
    }
}
