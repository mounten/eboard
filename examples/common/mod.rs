//! The header every example shows at the top: which backend and output it runs on, the frame
//! rate and the CPU time per frame, and a Quit button (boards often have no keyboard for Ctrl+C).

use std::collections::VecDeque;
use std::time::Instant;

use eboard::egui;

/// Frame intervals averaged for the frame rate.
const HISTORY_LEN: usize = 120;

pub struct Header {
    title: &'static str,
    frames: u64,
    last_frame: Option<Instant>,
    intervals_ms: VecDeque<f32>,
}

impl Header {
    pub fn new(title: &'static str) -> Self {
        Self {
            title,
            frames: 0,
            last_frame: None,
            intervals_ms: VecDeque::with_capacity(HISTORY_LEN),
        }
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Call once per frame, at the top of the UI.
    pub fn show(&mut self, ui: &mut egui::Ui, frame: &mut eboard::Frame) {
        self.tick();
        let screen = frame.screen().clone();

        ui.horizontal(|ui| {
            ui.heading(self.title);
            if ui.button("Quit").clicked() {
                frame.close();
            }
        });
        ui.label(format!(
            "Output: {} ({})",
            screen.output_name, screen.backend
        ));
        ui.label(format!(
            "Size: {}x{} px (panel {}x{}), rotation {}°{}",
            screen.size_px[0],
            screen.size_px[1],
            screen.panel_size_px[0],
            screen.panel_size_px[1],
            screen.rotation.degrees(),
            if screen.hardware_rotation {
                " (hardware)"
            } else {
                ""
            },
        ));
        ui.label(format!(
            "{:.2} Hz, pixels_per_point {}",
            screen.refresh_hz, screen.native_pixels_per_point
        ));
        ui.label(format!("Renderer: {}", screen.renderer_info));

        let mut stats = format!("Frame {}", self.frames);
        if !self.intervals_ms.is_empty() {
            let avg = self.intervals_ms.iter().sum::<f32>() / self.intervals_ms.len() as f32;
            let worst = self.intervals_ms.iter().copied().fold(0.0, f32::max);
            stats += &format!(
                ", {:.1} fps (avg {avg:.1} ms, worst {worst:.1} ms)",
                1000.0 / avg.max(0.001)
            );
        }
        if let Some(cpu) = frame.cpu_usage() {
            stats += &format!(", CPU per frame {:.2} ms", cpu * 1000.0);
        }
        ui.label(stats);
        ui.separator();
    }

    fn tick(&mut self) {
        self.frames += 1;
        let now = Instant::now();
        if let Some(last) = self.last_frame.replace(now) {
            if self.intervals_ms.len() == HISTORY_LEN {
                self.intervals_ms.pop_front();
            }
            self.intervals_ms
                .push_back((now - last).as_secs_f32() * 1000.0);
        }
    }
}
