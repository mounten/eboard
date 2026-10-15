//! Full-screen egui on the display, no compositor needed.
//!
//! ```sh
//! RUST_LOG=info cargo run --release --example hello
//! # Raspberry Pi Touch Display 2 mounted in landscape:
//! EGUI_EMBED_ROTATION=90 RUST_LOG=info cargo run --release --example hello
//! # CPU rendering to /dev/fb0 (no GPU needed):
//! EGUI_EMBED_BACKEND=linuxfb RUST_LOG=info cargo run --release --example hello
//! ```
//!
//! Stop it with Ctrl+C; the console comes back.

use std::time::Instant;

use egui_embed::egui;

struct Hello {
    started: Instant,
    frames: u64,
    animate: bool,
}

impl egui_embed::App for Hello {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut egui_embed::Frame) {
        self.frames += 1;
        let screen = frame.screen().clone();

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Hello from egui-embed");
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
            ui.separator();

            ui.label(format!(
                "Uptime: {:.1} s",
                self.started.elapsed().as_secs_f32()
            ));
            ui.label(format!("Frames painted: {}", self.frames));
            if let Some(cpu) = frame.cpu_usage() {
                ui.label(format!("CPU per frame: {:.2} ms", cpu * 1000.0));
            }
            ui.checkbox(&mut self.animate, "Animate (repaint every vsync)");

            if self.animate {
                let t = self.started.elapsed().as_secs_f32();
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 60.0),
                    egui::Sense::hover(),
                );
                let x = rect.left() + (t.sin() * 0.5 + 0.5) * rect.width();
                ui.painter().circle_filled(
                    egui::pos2(x, rect.center().y),
                    20.0,
                    ui.visuals().selection.bg_fill,
                );
                ui.add(egui::Spinner::new().size(40.0));
                ui.ctx().request_repaint();
            } else {
                // Idle: only repaint once a second for the uptime label. CPU stays near 0%.
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_secs(1));
            }
        });
    }

    fn on_exit(&mut self) {
        log::info!("Exiting after {} frames", self.frames);
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let animate = std::env::args().all(|arg| arg != "--idle");
    let result = egui_embed::run(egui_embed::Options::default(), |cc| {
        log::info!("Starting on {}", cc.screen.output_name);
        Ok(Box::new(Hello {
            started: Instant::now(),
            frames: 0,
            animate,
        }))
    });
    if let Err(e) = result {
        log::error!("{e}");
        std::process::exit(1);
    }
}
