//! Input: Linux evdev events → egui events.
//!
//! The decoders (`touch`, `pointer`, `keyboard`) are pure: they take raw `(type, code, value)`
//! triples and produce [`egui::Event`]s, so they are unit tested on any host. `devices` (Linux
//! only) finds, opens and reads `/dev/input/event*`.

mod codes;
#[cfg(target_os = "linux")]
pub(crate) mod devices;
mod keyboard;
mod pointer;
mod touch;

pub(crate) use keyboard::KeyboardDecoder;
pub(crate) use pointer::PointerDecoder;
#[cfg_attr(not(target_os = "linux"), expect(unused_imports))]
pub(crate) use touch::{AxisRange, TouchDecoder};

use egui::{Pos2, Vec2, pos2};

use crate::Rotation;

/// State shared by all input devices, and the queue of events for the next egui pass.
pub(crate) struct Shared {
    /// Panel size in pixels, native orientation (touch coordinates are relative to it).
    pub panel_size: [f32; 2],
    pub rotation: Rotation,
    /// egui's current points-to-pixels factor (native scale × zoom).
    pub pixels_per_point: f32,
    pub modifiers: egui::Modifiers,
    /// Mouse cursor in points. `None` until a mouse is used.
    pub cursor: Option<Pos2>,
    /// The last pointer input came from a mouse (draw a cursor), not from touch.
    pub cursor_visible: bool,
    /// Text from egui's copy/cut commands, for Ctrl+V (there is no system clipboard).
    pub clipboard: String,
    pub events: Vec<egui::Event>,
}

impl Shared {
    pub(crate) fn new(panel_size: [u32; 2], rotation: Rotation) -> Self {
        Self {
            panel_size: panel_size.map(|v| v as f32),
            rotation,
            pixels_per_point: 1.0,
            modifiers: egui::Modifiers::default(),
            cursor: None,
            cursor_visible: false,
            clipboard: String::new(),
            events: Vec::new(),
        }
    }

    /// The screen as egui sees it, in points.
    pub(crate) fn screen_size_points(&self) -> Vec2 {
        let [w, h] = self.rotation.logical_size(self.panel_size);
        Vec2::new(w, h) / self.pixels_per_point
    }

    /// A position on the panel (pixels, native orientation) in egui points.
    pub(crate) fn panel_to_points(&self, panel_px: [f32; 2]) -> Pos2 {
        let [x, y] = self.rotation.panel_to_logical(panel_px, self.panel_size);
        pos2(x, y) / self.pixels_per_point
    }
}

/// Turns the events of one device into egui events. A device can have several (e.g. a keyboard
/// with a built-in pointing stick).
pub(crate) enum Decoder {
    Touch(TouchDecoder),
    Pointer(PointerDecoder),
    Keyboard(KeyboardDecoder),
}

impl Decoder {
    pub(crate) fn event(&mut self, ty: u16, code: u16, value: i32, shared: &mut Shared) {
        match self {
            Self::Touch(d) => d.event(ty, code, value, shared),
            Self::Pointer(d) => d.event(ty, code, value, shared),
            Self::Keyboard(d) => d.event(ty, code, value, shared),
        }
    }

    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Touch(_) => "touchscreen",
            Self::Pointer(_) => "mouse",
            Self::Keyboard(_) => "keyboard",
        }
    }
}

/// All input devices plus the shared state, as used by the run loop.
#[cfg(target_os = "linux")]
pub(crate) struct Input {
    devices: devices::InputDevices,
    shared: Shared,
    show_cursor: bool,
}

#[cfg(target_os = "linux")]
impl Input {
    /// `None` if input is disabled in the options.
    pub(crate) fn new(options: &crate::Options, panel_size: [u32; 2]) -> Option<Self> {
        if !options.input.enabled {
            log::info!("Input disabled");
            return None;
        }
        Some(Self {
            devices: devices::InputDevices::open(&options.input),
            shared: Shared::new(panel_size, options.output.rotation),
            show_cursor: options.input.show_cursor,
        })
    }

    pub(crate) fn fds(&self) -> impl Iterator<Item = std::os::fd::BorrowedFd<'_>> {
        self.devices.fds()
    }

    /// Reads pending device events. Returns true if egui has new events to process.
    pub(crate) fn dispatch(&mut self, pixels_per_point: f32) -> bool {
        self.shared.pixels_per_point = pixels_per_point;
        let before = self.shared.events.len();
        self.devices.dispatch(&mut self.shared);
        self.shared.events.len() > before
    }

    pub(crate) fn take_events(&mut self) -> Vec<egui::Event> {
        std::mem::take(&mut self.shared.events)
    }

    /// Picks up text egui copied, for later pastes.
    pub(crate) fn handle_output(&mut self, output: &egui::PlatformOutput) {
        for command in &output.commands {
            if let egui::OutputCommand::CopyText(text) = command {
                self.shared.clipboard.clone_from(text);
            }
        }
    }

    /// Draws an arrow cursor on top of everything while a mouse is in use.
    pub(crate) fn paint_cursor(&self, ctx: &egui::Context) {
        if !self.show_cursor || !self.shared.cursor_visible {
            return;
        }
        let Some(tip) = self.shared.cursor else {
            return;
        };
        let at = |x: f32, y: f32| tip + Vec2::new(x, y);
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Debug,
            egui::Id::unique("eboard_cursor"),
        ));
        let fill = egui::Color32::WHITE;
        let outline = egui::Stroke::new(1.0, egui::Color32::BLACK);
        // The arrow is not convex: fill it as head + tail, outline it as one path.
        painter.add(egui::Shape::convex_polygon(
            vec![at(0.0, 0.0), at(0.0, 16.0), at(11.0, 11.0)],
            fill,
            egui::Stroke::NONE,
        ));
        painter.add(egui::Shape::convex_polygon(
            vec![at(4.0, 12.0), at(7.0, 19.0), at(9.5, 18.0), at(6.5, 11.0)],
            fill,
            egui::Stroke::NONE,
        ));
        painter.add(egui::Shape::closed_line(
            vec![
                at(0.0, 0.0),
                at(0.0, 16.0),
                at(4.0, 12.0),
                at(7.0, 19.0),
                at(9.5, 18.0),
                at(6.5, 11.0),
                at(11.0, 11.0),
            ],
            outline,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_to_points_follows_rotation_and_scale() {
        let mut shared = Shared::new([720, 1280], Rotation::Deg90);
        shared.pixels_per_point = 2.0;
        assert_eq!(shared.screen_size_points(), Vec2::new(640.0, 360.0));
        // The panel's top-right corner is the logical top-left.
        assert_eq!(shared.panel_to_points([720.0, 0.0]), pos2(0.0, 0.0));
        assert_eq!(shared.panel_to_points([0.0, 1280.0]), pos2(640.0, 360.0));
    }
}
