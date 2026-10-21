//! Bouncing particles: simple physics (velocity integration, bouncing off the edges) painted with
//! `egui::Painter`, repainted every frame. Like `stress_test`, raise the count until the frame
//! rate in the header drops, but this one also spends CPU time per particle, not just on filling
//! pixels.
//!
//! ```sh
//! RUST_LOG=info cargo run --release --example particles
//! ```

mod common;

use std::time::Instant;

use eboard::egui;
use egui::ecolor::Hsva;

/// Minimal xorshift32 PRNG, so the example doesn't need the `rand` crate for scattered start
/// positions and velocities.
struct Rng(u32);

impl Rng {
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        let unit = (self.next_u32() >> 8) as f32 / (1u32 << 24) as f32;
        lo + unit * (hi - lo)
    }
}

struct Particle {
    pos: egui::Pos2,
    vel: egui::Vec2,
    color: egui::Color32,
}

struct Particles {
    header: common::Header,
    count: usize,
    particles: Vec<Particle>,
    rng: Rng,
    last_update: Instant,
}

impl Particles {
    /// Adds or removes particles until there are `self.count`; new ones start anywhere in `rect`.
    fn resize(&mut self, rect: egui::Rect) {
        while self.particles.len() < self.count {
            let rng = &mut self.rng;
            self.particles.push(Particle {
                pos: egui::pos2(
                    rng.range(rect.min.x, rect.max.x),
                    rng.range(rect.min.y, rect.max.y),
                ),
                vel: egui::vec2(rng.range(-250.0, 250.0), rng.range(-250.0, 250.0)),
                color: Hsva::new(rng.range(0.0, 1.0), 0.75, 0.95, 1.0).into(),
            });
        }
        self.particles.truncate(self.count);
    }
}

impl eboard::App for Particles {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eboard::Frame) {
        // Repaint as fast as the display allows: a benchmark, not an idle demo.
        ui.ctx().request_repaint();

        let now = Instant::now();
        // Clamped, so a stall (e.g. the first frame) doesn't fling every particle off the screen
        // in one big step.
        let dt = (now - self.last_update).as_secs_f32().min(0.05);
        self.last_update = now;

        egui::CentralPanel::default().show(ui, |ui| {
            self.header.show(ui, frame);

            ui.horizontal(|ui| {
                if ui.button("-100").clicked() {
                    self.count = self.count.saturating_sub(100);
                }
                ui.label(format!("{} particles", self.count));
                if ui.button("+100").clicked() {
                    self.count += 100;
                }
            });

            let rect = ui.available_rect_before_wrap();
            self.resize(rect);
            for p in &mut self.particles {
                p.pos += p.vel * dt;
                if p.pos.x < rect.min.x || p.pos.x > rect.max.x {
                    p.vel.x = -p.vel.x;
                    p.pos.x = p.pos.x.clamp(rect.min.x, rect.max.x);
                }
                if p.pos.y < rect.min.y || p.pos.y > rect.max.y {
                    p.vel.y = -p.vel.y;
                    p.pos.y = p.pos.y.clamp(rect.min.y, rect.max.y);
                }
            }

            let painter = ui.painter_at(rect);
            for p in &self.particles {
                painter.circle_filled(p.pos, 3.0, p.color);
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
        Ok(Box::new(Particles {
            header: common::Header::new("Particles"),
            count: 300,
            particles: Vec::new(),
            rng: Rng(0x9E37_79B9),
            last_update: Instant::now(),
        }))
    });
    if let Err(e) = result {
        log::error!("{e}");
        std::process::exit(1);
    }
}
