//! Throughput benchmark: a grid of animated circles painted straight with `egui::Painter` (no
//! widget or layout work), repainted every frame. Frames are paced by the display (vblank or the
//! framebuffer's refresh rate), so the frame rate in the header stays at the refresh rate until
//! the work per frame exceeds the frame budget. Drag the grid slider up until it drops to find the
//! board's limit; "CPU per frame" shows how close it is.
//!
//! ```sh
//! RUST_LOG=info cargo run --release --example stress_test
//! # The CPU renderer:
//! EBOARD_BACKEND=linuxfb RUST_LOG=info cargo run --release --example stress_test
//! ```

mod common;

use std::time::Instant;

use eboard::egui;
use egui::ecolor::Hsva;

struct StressTest {
    header: common::Header,
    grid_size: u32,
    started: Instant,
}

impl eboard::App for StressTest {
    fn ui(&mut self, ui: &mut egui::Ui, board: &mut eboard::Board) {
        // Repaint as fast as the display allows: a benchmark, not an idle demo.
        ui.ctx().request_repaint();

        egui::CentralPanel::default().show(ui, |ui| {
            self.header.show(ui, board);

            ui.horizontal(|ui| {
                ui.add(egui::Slider::new(&mut self.grid_size, 1..=80).text("grid"));
                ui.label(format!("({} shapes)", self.grid_size * self.grid_size));
            });

            let rect = ui.available_rect_before_wrap();
            let painter = ui.painter_at(rect);
            let t = self.started.elapsed().as_secs_f32();
            let n = self.grid_size.max(1);
            let cell = rect.width().min(rect.height()) / n as f32;
            for y in 0..n {
                for x in 0..n {
                    let phase = (x + y) as f32 * 0.3;
                    let pulse = (t * 2.0 + phase).sin() * 0.5 + 0.5;
                    let center =
                        rect.min + egui::vec2((x as f32 + 0.5) * cell, (y as f32 + 0.5) * cell);
                    let hue = ((x + y) as f32 / (2 * n) as f32 + t * 0.1).fract();
                    let color: egui::Color32 = Hsva::new(hue, 0.8, 0.5 + 0.5 * pulse, 1.0).into();
                    painter.circle_filled(center, cell * 0.5 * (0.4 + 0.6 * pulse), color);
                }
            }
        });
    }

    fn on_exit(&mut self) {
        log::info!("Exiting after {} frames", self.header.frames());
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let result = eboard::run(eboard::Options::default(), |_cc| {
        Ok(Box::new(StressTest {
            header: common::Header::new("Stress test"),
            grid_size: 20,
            started: Instant::now(),
        }))
    });
    if let Err(e) = result {
        log::error!("{e}");
        std::process::exit(1);
    }
}
