//! Idle screen blanking: when to blank, and which input to swallow when waking up.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use egui::{Event, Key, TouchId, TouchPhase};

/// Blanks the screen after a period without input.
#[derive(Debug)]
pub(crate) struct IdleTimer {
    blank_after: Option<Duration>,
    last_activity: Instant,
    blanked: bool,
}

impl IdleTimer {
    pub(crate) fn new(blank_after: Option<Duration>, now: Instant) -> Self {
        Self {
            blank_after: blank_after.filter(|d| !d.is_zero()),
            last_activity: now,
            blanked: false,
        }
    }

    /// User input happened.
    pub(crate) fn activity(&mut self, now: Instant) {
        self.last_activity = now;
    }

    /// When the screen should go dark; `None` if blanking is off or it is dark already.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        if self.blanked {
            return None;
        }
        self.last_activity.checked_add(self.blank_after?)
    }

    pub(crate) fn should_blank(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|t| t <= now)
    }

    pub(crate) fn is_blanked(&self) -> bool {
        self.blanked
    }

    pub(crate) fn set_blanked(&mut self, blanked: bool) {
        self.blanked = blanked;
    }

    /// Changes the timeout (`None` or zero: never blank). Counts from the last input.
    pub(crate) fn set_blank_after(&mut self, blank_after: Option<Duration>) {
        self.blank_after = blank_after.filter(|d| !d.is_zero());
    }
}

/// Swallows the input that wakes the screen, so a tap on a dark screen can't press a button the
/// user couldn't see.
///
/// Everything is dropped until the waking gesture ends: until every touch, button and key that
/// went down after waking is released. Input without any press (mouse motion, wheel) ends it at
/// the end of the batch. Events that only keep state in sync pass through: modifier changes,
/// `PointerGone`, and releases of whatever was held down before the screen went dark.
#[derive(Debug, Default)]
#[cfg_attr(not(feature = "input-evdev"), allow(dead_code))]
pub(crate) struct WakeFilter {
    active: bool,
    pointer_down: bool,
    touches: BTreeSet<u64>,
    keys: BTreeSet<Key>,
}

#[cfg_attr(not(feature = "input-evdev"), allow(dead_code))]
impl WakeFilter {
    /// The screen just woke up: swallow the gesture that woke it.
    pub(crate) fn start(&mut self) {
        *self = Self {
            active: true,
            ..Self::default()
        };
    }

    pub(crate) fn filter(&mut self, events: Vec<Event>) -> Vec<Event> {
        if !self.active {
            return events;
        }
        let mut kept = Vec::new();
        for event in events {
            if self.active && self.swallow(&event) {
                continue;
            }
            kept.push(event);
        }
        if self.active && self.is_idle() {
            // Motion only, or nothing was pressed: the screen is awake, input works again.
            self.active = false;
        }
        kept
    }

    /// Updates the gesture state; returns true if `event` is dropped.
    fn swallow(&mut self, event: &Event) -> bool {
        // `Some(true)`: released something pressed after waking. `Some(false)`: released
        // something pressed before the screen went dark, which egui has to see.
        let released = match event {
            Event::ModifiersChanged(_) | Event::PointerGone => return false,
            Event::PointerButton { pressed: true, .. } => {
                self.pointer_down = true;
                None
            }
            Event::PointerButton { pressed: false, .. } => {
                Some(std::mem::take(&mut self.pointer_down))
            }
            Event::Touch {
                id: TouchId(id),
                phase,
                ..
            } => match phase {
                TouchPhase::Start => {
                    self.touches.insert(*id);
                    None
                }
                TouchPhase::Move => None,
                TouchPhase::End | TouchPhase::Cancel => Some(self.touches.remove(id)),
            },
            Event::Key {
                key,
                pressed: true,
                repeat,
                ..
            } => {
                if !repeat {
                    self.keys.insert(*key);
                }
                None
            }
            Event::Key {
                key,
                pressed: false,
                ..
            } => Some(self.keys.remove(key)),
            _ => None,
        };
        if released == Some(false) {
            return false;
        }
        if released == Some(true) && self.is_idle() {
            // The waking gesture is over; whatever follows is new input.
            self.active = false;
        }
        true
    }

    fn is_idle(&self) -> bool {
        !self.pointer_down && self.touches.is_empty() && self.keys.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Modifiers, PointerButton, pos2};

    #[test]
    fn idle_timer_blanks_after_the_timeout() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut idle = IdleTimer::new(Some(s(10)), t0);
        assert_eq!(idle.deadline(), Some(t0 + s(10)));
        assert!(!idle.should_blank(t0 + s(9)));
        idle.activity(t0 + s(5));
        assert!(!idle.should_blank(t0 + s(10)));
        assert!(idle.should_blank(t0 + s(15)));

        idle.set_blanked(true);
        assert!(idle.is_blanked());
        assert_eq!(idle.deadline(), None);
        assert!(!idle.should_blank(t0 + s(100)));

        idle.set_blanked(false);
        idle.activity(t0 + s(100));
        assert_eq!(idle.deadline(), Some(t0 + s(110)));

        idle.set_blank_after(Some(s(60)));
        assert_eq!(idle.deadline(), Some(t0 + s(160)));
        idle.set_blank_after(None);
        assert_eq!(idle.deadline(), None);
        idle.set_blank_after(Some(Duration::ZERO));
        assert_eq!(idle.deadline(), None);
        assert_eq!(IdleTimer::new(None, t0).deadline(), None);
    }

    fn touch(id: u64, phase: TouchPhase) -> Event {
        Event::Touch {
            device_id: egui::TouchDeviceId(0),
            id: TouchId(id),
            phase,
            pos: pos2(10.0, 10.0),
            force: None,
        }
    }

    fn button(pressed: bool) -> Event {
        Event::PointerButton {
            pos: pos2(10.0, 10.0),
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::default(),
        }
    }

    fn key(key: Key, pressed: bool) -> Event {
        Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers: Modifiers::default(),
        }
    }

    /// What the touch decoder emits for the first finger.
    fn finger_down(id: u64) -> Vec<Event> {
        vec![
            touch(id, TouchPhase::Start),
            Event::PointerMoved(pos2(10.0, 10.0)),
            button(true),
        ]
    }

    fn finger_up(id: u64) -> Vec<Event> {
        vec![
            touch(id, TouchPhase::End),
            button(false),
            Event::PointerGone,
        ]
    }

    #[test]
    fn inactive_filter_passes_everything() {
        let mut filter = WakeFilter::default();
        assert_eq!(filter.filter(finger_down(1)), finger_down(1));
    }

    #[test]
    fn waking_tap_is_swallowed() {
        let mut filter = WakeFilter::default();
        filter.start();
        // Down and up in separate batches.
        assert_eq!(filter.filter(finger_down(1)), []);
        assert_eq!(
            filter.filter(vec![
                touch(1, TouchPhase::Move),
                Event::PointerMoved(pos2(11.0, 10.0))
            ]),
            []
        );
        assert_eq!(filter.filter(finger_up(1)), [Event::PointerGone]);
        // The next tap works.
        assert_eq!(filter.filter(finger_down(2)), finger_down(2));
    }

    #[test]
    fn events_after_the_waking_tap_in_the_same_batch_pass() {
        let mut filter = WakeFilter::default();
        filter.start();
        let mut events = finger_down(1);
        events.extend(finger_up(1));
        events.extend(finger_down(2));
        let mut expected = vec![Event::PointerGone];
        expected.extend(finger_down(2));
        assert_eq!(filter.filter(events), expected);
    }

    #[test]
    fn multi_touch_wake_ends_with_the_last_finger() {
        let mut filter = WakeFilter::default();
        filter.start();
        let mut events = finger_down(1);
        events.push(touch(2, TouchPhase::Start));
        assert_eq!(filter.filter(events), []);
        assert_eq!(filter.filter(finger_up(1)), [Event::PointerGone]);
        assert_eq!(filter.filter(vec![touch(2, TouchPhase::Move)]), []);
        assert_eq!(filter.filter(vec![touch(2, TouchPhase::Cancel)]), []);
        assert_eq!(filter.filter(finger_down(3)), finger_down(3));
    }

    #[test]
    fn mouse_motion_wakes_without_a_gesture() {
        let mut filter = WakeFilter::default();
        filter.start();
        let moved = Event::PointerMoved(pos2(5.0, 5.0));
        assert_eq!(filter.filter(vec![moved.clone(), moved.clone()]), []);
        assert_eq!(filter.filter(vec![moved.clone()]), [moved]);
    }

    #[test]
    fn mouse_click_wake_is_swallowed() {
        let mut filter = WakeFilter::default();
        filter.start();
        assert_eq!(
            filter.filter(vec![Event::PointerMoved(pos2(5.0, 5.0)), button(true)]),
            []
        );
        assert_eq!(filter.filter(vec![button(false)]), []);
        assert_eq!(filter.filter(vec![button(true)]), [button(true)]);
    }

    #[test]
    fn waking_key_and_its_text_are_swallowed() {
        let mut filter = WakeFilter::default();
        filter.start();
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        assert_eq!(
            filter.filter(vec![
                Event::ModifiersChanged(shift),
                key(Key::A, true),
                Event::Text("A".into()),
            ]),
            [Event::ModifiersChanged(shift)]
        );
        let repeat = Event::Key {
            key: Key::A,
            physical_key: Some(Key::A),
            pressed: true,
            repeat: true,
            modifiers: shift,
        };
        assert_eq!(filter.filter(vec![repeat, Event::Text("A".into())]), []);
        assert_eq!(filter.filter(vec![key(Key::A, false)]), []);
        assert_eq!(filter.filter(vec![key(Key::B, true)]), [key(Key::B, true)]);
    }

    #[test]
    fn releases_of_things_held_before_blanking_pass() {
        let mut filter = WakeFilter::default();
        filter.start();
        // A finger rested on the screen until it went dark, then lifted.
        assert_eq!(filter.filter(finger_up(7)), finger_up(7));
        assert_eq!(
            filter.filter(vec![key(Key::Enter, false)]),
            [key(Key::Enter, false)]
        );
    }
}
