//! Touchscreens: multi-touch protocol B (slots + tracking ids, e.g. Goodix, FocalTech, Ilitek)
//! and single-touch (`ABS_X`/`ABS_Y` + `BTN_TOUCH`, e.g. resistive panels).
//!
//! Besides [`egui::Event::Touch`], the first finger also drives the egui pointer (press, move,
//! release), like egui-winit does, so ordinary widgets work with touch.

use egui::{Event, PointerButton, TouchDeviceId, TouchId, TouchPhase};

use super::Shared;
use super::codes::*;
use crate::TouchTransform;

/// Value range of a touch axis, from the device's `absinfo`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AxisRange {
    pub min: i32,
    pub max: i32,
}

impl AxisRange {
    /// Maps a value to 0..=1. Value `v` covers `[v, v + 1)`, so a range of `0..=719` on a
    /// 720 px panel lands on pixel centers.
    fn normalize(self, value: i32) -> f32 {
        let span = (self.max - self.min) as f32 + 1.0;
        if span <= 0.0 {
            return 0.0;
        }
        (((value - self.min) as f32 + 0.5) / span).clamp(0.0, 1.0)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Contact {
    /// Tracking id the kernel currently reports for this slot (`None` = no finger).
    current: Option<i32>,
    /// Tracking id egui knows about (sent `Start`, not yet `End`).
    reported: Option<i32>,
    position: [i32; 2],
    /// Position sent with the last event, for `End` after the slot was reused.
    reported_position: [i32; 2],
    moved: bool,
}

pub(crate) struct TouchDecoder {
    device_id: u64,
    x: AxisRange,
    y: AxisRange,
    transform: TouchTransform,
    /// Multi-touch protocol B. Otherwise single-touch, using contact 0.
    multitouch: bool,
    contacts: Vec<Contact>,
    slot: usize,
    /// After `SYN_DROPPED`, events are skipped up to the next `SYN_REPORT`.
    dropping: bool,
    /// The touch that drives the emulated mouse pointer.
    pointer_touch: Option<u64>,
}

impl TouchDecoder {
    /// `slots`: number of multi-touch slots, or `None` for a single-touch device.
    pub(crate) fn new(
        device_id: u64,
        x: AxisRange,
        y: AxisRange,
        slots: Option<usize>,
        transform: TouchTransform,
    ) -> Self {
        Self {
            device_id,
            x,
            y,
            transform,
            multitouch: slots.is_some(),
            contacts: vec![Contact::default(); slots.unwrap_or(1).max(1)],
            slot: 0,
            dropping: false,
            pointer_touch: None,
        }
    }

    pub(crate) fn event(&mut self, ty: u16, code: u16, value: i32, shared: &mut Shared) {
        match (ty, code) {
            (EV_SYN, SYN_DROPPED) => self.dropping = true,
            (EV_SYN, SYN_REPORT) => {
                if self.dropping {
                    // We lost events: end all touches rather than guess.
                    self.dropping = false;
                    self.cancel_all(shared);
                } else {
                    self.sync(shared);
                }
            }
            _ if self.dropping => {}
            (EV_ABS, code) if self.multitouch => self.multitouch_axis(code, value),
            (EV_ABS, ABS_X) => self.set_position(0, value),
            (EV_ABS, ABS_Y) => self.set_position(1, value),
            (EV_KEY, BTN_TOUCH) => {
                self.contacts[0].current = (value != 0).then_some(0);
            }
            _ => {}
        }
    }

    fn multitouch_axis(&mut self, code: u16, value: i32) {
        match code {
            ABS_MT_SLOT => {
                self.slot = value.max(0) as usize;
                if self.slot >= self.contacts.len() {
                    self.contacts.resize(self.slot + 1, Contact::default());
                }
            }
            ABS_MT_TRACKING_ID => self.contacts[self.slot].current = (value >= 0).then_some(value),
            ABS_MT_POSITION_X => self.set_position(0, value),
            ABS_MT_POSITION_Y => self.set_position(1, value),
            _ => {}
        }
    }

    fn set_position(&mut self, axis: usize, value: i32) {
        let slot = if self.multitouch { self.slot } else { 0 };
        let contact = &mut self.contacts[slot];
        contact.position[axis] = value;
        contact.moved = true;
    }

    /// End of a frame of events: report what changed.
    fn sync(&mut self, shared: &mut Shared) {
        for i in 0..self.contacts.len() {
            let c = self.contacts[i];
            if c.reported != c.current {
                if let Some(old) = c.reported {
                    // A lifted finger keeps its last position; a reused slot already holds the
                    // new contact's position.
                    let at = if c.current.is_none() {
                        c.position
                    } else {
                        c.reported_position
                    };
                    self.emit(old, TouchPhase::End, at, shared);
                }
                if let Some(new) = c.current {
                    self.emit(new, TouchPhase::Start, c.position, shared);
                }
            } else if let Some(id) = c.current
                && c.moved
                && c.position != c.reported_position
            {
                self.emit(id, TouchPhase::Move, c.position, shared);
            }
            let c = &mut self.contacts[i];
            c.reported = c.current;
            c.reported_position = c.position;
            c.moved = false;
        }
    }

    fn cancel_all(&mut self, shared: &mut Shared) {
        for i in 0..self.contacts.len() {
            let c = self.contacts[i];
            if let Some(id) = c.reported {
                self.emit(id, TouchPhase::Cancel, c.reported_position, shared);
            }
            self.contacts[i] = Contact::default();
        }
    }

    fn emit(&mut self, tracking_id: i32, phase: TouchPhase, raw: [i32; 2], shared: &mut Shared) {
        let normalized = self
            .transform
            .apply([self.x.normalize(raw[0]), self.y.normalize(raw[1])]);
        let pos = shared.panel_to_points([
            normalized[0] * shared.panel_size[0],
            normalized[1] * shared.panel_size[1],
        ]);
        let id = u64::from(tracking_id as u32);
        shared.events.push(Event::Touch {
            device_id: TouchDeviceId(self.device_id),
            id: TouchId(id),
            phase,
            pos,
            force: None,
        });

        // Pointer emulation for the first finger.
        if self.pointer_touch.is_some_and(|p| p != id) {
            return;
        }
        let modifiers = shared.modifiers;
        let button = |pressed| Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers,
        };
        match phase {
            TouchPhase::Start => {
                self.pointer_touch = Some(id);
                shared.cursor_visible = false;
                shared.events.push(Event::PointerMoved(pos));
                shared.events.push(button(true));
            }
            TouchPhase::Move => shared.events.push(Event::PointerMoved(pos)),
            TouchPhase::End => {
                self.pointer_touch = None;
                shared.events.push(button(false));
                // No hover effects after the finger is gone.
                shared.events.push(Event::PointerGone);
            }
            TouchPhase::Cancel => {
                self.pointer_touch = None;
                shared.events.push(Event::PointerGone);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rotation;
    use egui::pos2;

    const RANGE_X: AxisRange = AxisRange { min: 0, max: 719 };
    const RANGE_Y: AxisRange = AxisRange { min: 0, max: 1279 };

    fn shared(rotation: Rotation) -> Shared {
        Shared::new([720, 1280], rotation)
    }

    fn feed(d: &mut TouchDecoder, s: &mut Shared, events: &[(u16, u16, i32)]) -> Vec<Event> {
        for &(ty, code, value) in events {
            d.event(ty, code, value, s);
        }
        std::mem::take(&mut s.events)
    }

    const SYN: (u16, u16, i32) = (EV_SYN, SYN_REPORT, 0);

    fn touches(events: &[Event]) -> Vec<(u64, TouchPhase, egui::Pos2)> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Touch { id, phase, pos, .. } => Some((id.0, *phase, *pos)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn multitouch_tap_emulates_a_click() {
        let mut s = shared(Rotation::Deg0);
        let mut d = TouchDecoder::new(1, RANGE_X, RANGE_Y, Some(10), TouchTransform::default());
        let down = feed(
            &mut d,
            &mut s,
            &[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, 7),
                (EV_ABS, ABS_MT_POSITION_X, 99),
                (EV_ABS, ABS_MT_POSITION_Y, 199),
                SYN,
            ],
        );
        let p = pos2(99.5, 199.5);
        assert_eq!(touches(&down), vec![(7, TouchPhase::Start, p)]);
        assert!(matches!(down[1], Event::PointerMoved(q) if q == p));
        assert!(matches!(
            down[2],
            Event::PointerButton {
                pressed: true,
                button: PointerButton::Primary,
                ..
            }
        ));

        let moved = feed(&mut d, &mut s, &[(EV_ABS, ABS_MT_POSITION_X, 109), SYN]);
        assert_eq!(
            touches(&moved),
            vec![(7, TouchPhase::Move, pos2(109.5, 199.5))]
        );
        assert!(matches!(moved[1], Event::PointerMoved(_)));

        let up = feed(&mut d, &mut s, &[(EV_ABS, ABS_MT_TRACKING_ID, -1), SYN]);
        assert_eq!(touches(&up), vec![(7, TouchPhase::End, pos2(109.5, 199.5))]);
        assert!(matches!(up[1], Event::PointerButton { pressed: false, .. }));
        assert!(matches!(up[2], Event::PointerGone));
    }

    #[test]
    fn second_finger_does_not_move_the_pointer() {
        let mut s = shared(Rotation::Deg0);
        let mut d = TouchDecoder::new(1, RANGE_X, RANGE_Y, Some(10), TouchTransform::default());
        feed(
            &mut d,
            &mut s,
            &[
                (EV_ABS, ABS_MT_TRACKING_ID, 1),
                (EV_ABS, ABS_MT_POSITION_X, 10),
                SYN,
            ],
        );
        let second = feed(
            &mut d,
            &mut s,
            &[
                (EV_ABS, ABS_MT_SLOT, 1),
                (EV_ABS, ABS_MT_TRACKING_ID, 2),
                (EV_ABS, ABS_MT_POSITION_X, 500),
                SYN,
            ],
        );
        assert_eq!(second.len(), 1, "{second:?}");
        assert!(matches!(
            second[0],
            Event::Touch {
                phase: TouchPhase::Start,
                ..
            }
        ));

        // First finger lifts: pointer released. Second finger keeps going as a pure touch.
        let lift = feed(
            &mut d,
            &mut s,
            &[
                (EV_ABS, ABS_MT_SLOT, 0),
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
                SYN,
            ],
        );
        assert_eq!(touches(&lift).len(), 1);
        assert!(
            lift.iter()
                .any(|e| matches!(e, Event::PointerButton { pressed: false, .. }))
        );
    }

    #[test]
    fn slot_reused_within_one_frame_ends_the_old_touch() {
        let mut s = shared(Rotation::Deg0);
        let mut d = TouchDecoder::new(1, RANGE_X, RANGE_Y, Some(2), TouchTransform::default());
        feed(&mut d, &mut s, &[(EV_ABS, ABS_MT_TRACKING_ID, 1), SYN]);
        let events = feed(&mut d, &mut s, &[(EV_ABS, ABS_MT_TRACKING_ID, 2), SYN]);
        let phases: Vec<_> = touches(&events).iter().map(|t| (t.0, t.1)).collect();
        assert_eq!(phases, vec![(1, TouchPhase::End), (2, TouchPhase::Start)]);
    }

    #[test]
    fn touch_follows_display_rotation() {
        // Portrait panel, landscape UI (90° clockwise): the panel's top-right is the UI's
        // top-left.
        let mut s = shared(Rotation::Deg90);
        let mut d = TouchDecoder::new(1, RANGE_X, RANGE_Y, Some(10), TouchTransform::default());
        let events = feed(
            &mut d,
            &mut s,
            &[
                (EV_ABS, ABS_MT_TRACKING_ID, 1),
                (EV_ABS, ABS_MT_POSITION_X, 719),
                (EV_ABS, ABS_MT_POSITION_Y, 0),
                SYN,
            ],
        );
        assert_eq!(
            touches(&events),
            vec![(1, TouchPhase::Start, pos2(0.5, 0.5))]
        );
    }

    #[test]
    fn single_touch_device() {
        let mut s = shared(Rotation::Deg0);
        let range = AxisRange { min: 0, max: 4095 };
        let mut d = TouchDecoder::new(1, range, range, None, TouchTransform::default());
        let down = feed(
            &mut d,
            &mut s,
            &[
                (EV_ABS, ABS_X, 2048),
                (EV_ABS, ABS_Y, 0),
                (EV_KEY, BTN_TOUCH, 1),
                SYN,
            ],
        );
        let t = touches(&down);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].1, TouchPhase::Start);
        assert!((t[0].2.x - 360.0).abs() < 0.2, "{:?}", t[0].2);
        let up = feed(&mut d, &mut s, &[(EV_KEY, BTN_TOUCH, 0), SYN]);
        assert_eq!(touches(&up)[0].1, TouchPhase::End);
    }

    #[test]
    fn dropped_events_cancel_touches() {
        let mut s = shared(Rotation::Deg0);
        let mut d = TouchDecoder::new(1, RANGE_X, RANGE_Y, Some(10), TouchTransform::default());
        feed(&mut d, &mut s, &[(EV_ABS, ABS_MT_TRACKING_ID, 1), SYN]);
        let events = feed(
            &mut d,
            &mut s,
            &[
                (EV_SYN, SYN_DROPPED, 0),
                (EV_ABS, ABS_MT_POSITION_X, 300),
                SYN,
            ],
        );
        assert_eq!(touches(&events)[0].1, TouchPhase::Cancel);
        assert!(matches!(events.last(), Some(Event::PointerGone)));
    }

    #[test]
    fn touch_transform_is_applied_before_rotation() {
        let mut s = shared(Rotation::Deg0);
        let transform = TouchTransform {
            invert_x: true,
            ..Default::default()
        };
        let mut d = TouchDecoder::new(1, RANGE_X, RANGE_Y, Some(10), transform);
        let events = feed(
            &mut d,
            &mut s,
            &[
                (EV_ABS, ABS_MT_TRACKING_ID, 1),
                (EV_ABS, ABS_MT_POSITION_X, 0),
                (EV_ABS, ABS_MT_POSITION_Y, 0),
                SYN,
            ],
        );
        assert_eq!(touches(&events)[0].2, pos2(719.5, 0.5));
    }
}
