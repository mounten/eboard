//! A HUD for a video overlay: every pixel that is not UI gets a key color (a "green screen"),
//! which external hardware (an FPGA, a video mixer) replaces with live video.
//!
//! ```sh
//! RUST_LOG=info cargo run --release --example hud
//! # Another key color (RRGGBB), egui's anti-aliasing kept, repaint every frame:
//! RUST_LOG=info cargo run --release --example hud -- --key ff00ff --aa --animate
//! ```
//!
//! What makes it work:
//! - [`eboard::App::clear_color`] returns the key color. The screen is cleared to it before every
//!   frame, so whatever egui does not paint stays exactly the key.
//! - The central panel has no frame: by default it fills the screen with `panel_fill`.
//! - Feathering (egui's anti-aliasing of shape edges) is off, so edges are not blended with the
//!   key into colors the keyer does not recognize. Text is anti-aliased regardless, so it sits on
//!   opaque plates and its edges blend with the plate instead of the key.
//! - No shadows: they are translucent and would tint the key.

mod common;

use std::time::Instant;

use eboard::egui;

/// Background of text and buttons, opaque.
const PLATE: egui::Color32 = egui::Color32::from_rgb(20, 24, 28);

struct Hud {
    header: common::Header,
    key: egui::Color32,
    started: Instant,
    animate: bool,
    recording: bool,
}

/// Opaque background for HUD text and widgets.
fn plate<R>(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::NONE
        .fill(PLATE)
        .inner_margin(8.0)
        .show(ui, |ui| {
            // Areas start narrow; grow with the text instead of wrapping it.
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            add_contents(ui)
        })
        .inner
}

impl eboard::App for Hud {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.key.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, board: &mut eboard::Board) {
        let t = self.started.elapsed().as_secs_f32();
        let ctx = ui.ctx().clone();

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                // Crosshair in the middle of the screen.
                let center = ui.max_rect().center();
                let stroke = egui::Stroke::new(3.0, egui::Color32::WHITE);
                for (from, to) in [
                    (egui::vec2(-40.0, 0.0), egui::vec2(-10.0, 0.0)),
                    (egui::vec2(10.0, 0.0), egui::vec2(40.0, 0.0)),
                    (egui::vec2(0.0, -40.0), egui::vec2(0.0, -10.0)),
                    (egui::vec2(0.0, 10.0), egui::vec2(0.0, 40.0)),
                ] {
                    ui.painter()
                        .line_segment([center + from, center + to], stroke);
                }

                if self.animate {
                    // A marker sweeping across the screen.
                    let rect = ui.max_rect();
                    let x = rect.left() + (t.sin() * 0.5 + 0.5) * rect.width();
                    ui.painter().circle_filled(
                        egui::pos2(x, rect.bottom() - 120.0),
                        12.0,
                        egui::Color32::from_rgb(255, 160, 0),
                    );
                }
            });

        egui::Area::new(egui::Id::unique("status"))
            .anchor(egui::Align2::LEFT_TOP, egui::vec2(16.0, 16.0))
            .show(&ctx, |ui| {
                plate(ui, |ui| {
                    self.header.show(ui, board);
                    ui.label(format!("T+{t:.0} s"));
                    if self.recording {
                        ui.colored_label(egui::Color32::from_rgb(255, 60, 60), "● REC");
                    }
                });
            });

        egui::Area::new(egui::Id::unique("controls"))
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -16.0))
            .show(&ctx, |ui| {
                plate(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.toggle_value(&mut self.recording, "Record");
                        ui.checkbox(&mut self.animate, "Animate");
                    });
                });
            });

        if self.animate {
            ctx.request_repaint();
        } else {
            // Idle: only the clock changes, once a second.
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }
    }

    fn on_exit(&mut self) {
        log::info!("Exiting after {} frames", self.header.frames());
    }
}

/// Parses `RRGGBB` (with or without `#`).
fn parse_rgb(s: &str) -> Option<egui::Color32> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 {
        return None;
    }
    let rgb = u32::from_str_radix(hex, 16).ok()?;
    let [_, r, g, b] = rgb.to_be_bytes();
    Some(egui::Color32::from_rgb(r, g, b))
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let key = match args.iter().position(|a| a == "--key") {
        Some(i) => match args.get(i + 1).and_then(|s| parse_rgb(s)) {
            Some(key) => key,
            None => {
                eprintln!("--key expects a color as RRGGBB, e.g. --key 00ff00");
                std::process::exit(2);
            }
        },
        // Pure green: exact in every framebuffer format, RGB565 included.
        None => egui::Color32::from_rgb(0, 255, 0),
    };
    let anti_aliasing = args.iter().any(|a| a == "--aa");
    let animate = args.iter().any(|a| a == "--animate");

    let result = eboard::run(eboard::Options::default(), |cc| {
        log::info!(
            "HUD on {}, key color #{:02x}{:02x}{:02x}, anti-aliasing {}",
            cc.screen.output_name,
            key.r(),
            key.g(),
            key.b(),
            if anti_aliasing { "on" } else { "off" },
        );
        cc.egui_ctx
            .tessellation_options_mut(|o| o.feathering = anti_aliasing);
        cc.egui_ctx.all_styles_mut(|style| {
            style.visuals.window_shadow = egui::Shadow::NONE;
            style.visuals.popup_shadow = egui::Shadow::NONE;
        });
        Ok(Box::new(Hud {
            header: common::Header::new("HUD"),
            key,
            started: Instant::now(),
            animate,
            recording: false,
        }))
    });
    if let Err(e) = result {
        log::error!("{e}");
        std::process::exit(1);
    }
}
