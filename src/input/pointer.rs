//! Relative pointing devices: mice, trackballs, pointing sticks.
//!
//! The cursor position lives in [`Shared`] (all mice move the same cursor) in egui points and is
//! clamped to the screen. Motion is applied in screen space, so display rotation doesn't matter.

use egui::{Event, MouseWheelSource, MouseWheelUnit, PointerButton, TouchPhase, Vec2, pos2, vec2};

use super::Shared;
use super::codes::*;

/// Kernel units per wheel notch for `REL_WHEEL_HI_RES`.
const HI_RES_PER_NOTCH: f32 = 120.0;

#[derive(Default)]
pub(crate) struct PointerDecoder {
    motion: [i32; 2],
    /// Wheel notches, `[horizontal, vertical]`.
    wheel: [f32; 2],
    /// The device reports high-resolution wheel events; ignore the low-resolution duplicates.
    hi_res: [bool; 2],
    buttons: Vec<(PointerButton, bool)>,
}

impl PointerDecoder {
    pub(crate) fn event(&mut self, ty: u16, code: u16, value: i32, shared: &mut Shared) {
        match (ty, code) {
            (EV_REL, REL_X) => self.motion[0] += value,
            (EV_REL, REL_Y) => self.motion[1] += value,
            (EV_REL, REL_WHEEL_HI_RES) => {
                self.hi_res[1] = true;
                self.wheel[1] += value as f32 / HI_RES_PER_NOTCH;
            }
            (EV_REL, REL_HWHEEL_HI_RES) => {
                self.hi_res[0] = true;
                self.wheel[0] += value as f32 / HI_RES_PER_NOTCH;
            }
            (EV_REL, REL_WHEEL) if !self.hi_res[1] => self.wheel[1] += value as f32,
            (EV_REL, REL_HWHEEL) if !self.hi_res[0] => self.wheel[0] += value as f32,
            (EV_KEY, code) if value != 2 => {
                let button = match code {
                    BTN_LEFT => PointerButton::Primary,
                    BTN_RIGHT => PointerButton::Secondary,
                    BTN_MIDDLE => PointerButton::Middle,
                    BTN_SIDE => PointerButton::Extra1,
                    BTN_EXTRA => PointerButton::Extra2,
                    _ => return,
                };
                self.buttons.push((button, value != 0));
            }
            (EV_SYN, SYN_REPORT) => self.flush(shared),
            (EV_SYN, SYN_DROPPED) => {
                *self = Self {
                    hi_res: self.hi_res,
                    ..Self::default()
                }
            }
            _ => {}
        }
    }

    fn flush(&mut self, shared: &mut Shared) {
        let size = shared.screen_size_points();
        let mut pos = shared.cursor.unwrap_or((size / 2.0).to_pos2());
        let modifiers = shared.modifiers;
        let first_event = shared.events.len();

        if self.motion != [0, 0] {
            // Mouse counts are roughly pixels; move in pixels so the speed doesn't depend on the
            // UI scale.
            let delta =
                vec2(self.motion[0] as f32, self.motion[1] as f32) / shared.pixels_per_point;
            pos = (pos + delta).clamp(pos2(0.0, 0.0), size.to_pos2());
            shared.events.push(Event::MouseMoved(delta));
            shared.events.push(Event::PointerMoved(pos));
            self.motion = [0, 0];
        }

        for (button, pressed) in self.buttons.drain(..) {
            shared.events.push(Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers,
            });
        }

        if self.wheel != [0.0, 0.0] {
            // egui (like winit): positive y scrolls up, positive x scrolls left. The kernel's
            // horizontal wheel is positive to the right.
            shared.events.push(Event::MouseWheel {
                unit: MouseWheelUnit::Line,
                delta: Vec2::new(-self.wheel[0], self.wheel[1]),
                phase: TouchPhase::Move,
                source: MouseWheelSource::Wheel,
                modifiers,
            });
            self.wheel = [0.0, 0.0];
        }

        if shared.events.len() > first_event {
            shared.cursor = Some(pos);
            shared.cursor_visible = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Rotation;

    fn setup() -> (PointerDecoder, Shared) {
        let mut shared = Shared::new([800, 600], Rotation::Deg0);
        shared.pixels_per_point = 2.0;
        (PointerDecoder::default(), shared)
    }

    fn feed(d: &mut PointerDecoder, s: &mut Shared, events: &[(u16, u16, i32)]) -> Vec<Event> {
        for &(ty, code, value) in events {
            d.event(ty, code, value, s);
        }
        d.event(EV_SYN, SYN_REPORT, 0, s);
        std::mem::take(&mut s.events)
    }

    #[test]
    fn motion_starts_at_center_and_is_clamped() {
        let (mut d, mut s) = setup();
        let events = feed(&mut d, &mut s, &[(EV_REL, REL_X, 20), (EV_REL, REL_Y, -10)]);
        // Screen is 400x300 points; 20 px = 10 points.
        assert!(matches!(events[1], Event::PointerMoved(p) if p == pos2(210.0, 145.0)));
        assert!(s.cursor_visible);

        let events = feed(&mut d, &mut s, &[(EV_REL, REL_X, 10_000)]);
        assert!(matches!(events[1], Event::PointerMoved(p) if p == pos2(400.0, 145.0)));
    }

    #[test]
    fn buttons_report_the_cursor_position() {
        let (mut d, mut s) = setup();
        feed(&mut d, &mut s, &[(EV_REL, REL_X, 2)]);
        let events = feed(&mut d, &mut s, &[(EV_KEY, BTN_RIGHT, 1)]);
        assert!(matches!(
            events[0],
            Event::PointerButton { button: PointerButton::Secondary, pressed: true, pos, .. } if pos == pos2(201.0, 150.0)
        ));
    }

    #[test]
    fn hi_res_wheel_replaces_low_res() {
        let (mut d, mut s) = setup();
        let events = feed(
            &mut d,
            &mut s,
            &[(EV_REL, REL_WHEEL_HI_RES, 60), (EV_REL, REL_WHEEL, 1)],
        );
        assert!(matches!(events[0], Event::MouseWheel { delta, .. } if delta == vec2(0.0, 0.5)));
        let events = feed(&mut d, &mut s, &[(EV_REL, REL_WHEEL, 1)]);
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn horizontal_wheel_sign() {
        let (mut d, mut s) = setup();
        let events = feed(&mut d, &mut s, &[(EV_REL, REL_HWHEEL, 1)]);
        assert!(matches!(events[0], Event::MouseWheel { delta, .. } if delta == vec2(-1.0, 0.0)));
    }
}
