//! egui's hello world (name, age, a button and Ferris) full-screen on the display, no compositor
//! needed.
//!
//! ```sh
//! RUST_LOG=info cargo run --release --example hello
//! # Raspberry Pi Touch Display 2 mounted in landscape:
//! EBOARD_ROTATION=90 RUST_LOG=info cargo run --release --example hello
//! # Vivante GPU on /dev/fb0 (i.MX6 BSPs without KMS/GBM):
//! EBOARD_BACKEND=viv-fb RUST_LOG=info cargo run --release --example hello
//! # CPU rendering to /dev/fb0 (no GPU needed):
//! EBOARD_BACKEND=linuxfb RUST_LOG=info cargo run --release --example hello
//! # Blank the display after 30 s without input; a tap wakes it:
//! EBOARD_BLANK_AFTER=30 RUST_LOG=info cargo run --release --example hello
//! ```
//!
//! Stop it with Ctrl+C or the Quit button; the console comes back. Built with
//! `--features persistence`, name and age survive a restart.

mod common;

use eboard::egui;

struct Hello {
    header: common::Header,
    name: String,
    age: u32,
}

impl eboard::App for Hello {
    fn ui(&mut self, ui: &mut egui::Ui, board: &mut eboard::Board) {
        egui::CentralPanel::default().show(ui, |ui| {
            self.header.show(ui, board);

            ui.heading("My egui Application");
            ui.horizontal(|ui| {
                let name_label = ui.label("Your name: ");
                ui.text_edit_singleline(&mut self.name)
                    .labelled_by(name_label.id);
            });
            ui.add(egui::Slider::new(&mut self.age, 0..=120).text("age"));
            if ui.button("Increment").clicked() {
                self.age += 1;
            }
            ui.label(format!("Hello '{}', age {}", self.name, self.age));
            ui.image(egui::include_image!("assets/ferris.png"));
        });
    }

    #[cfg(feature = "persistence")]
    fn save(&mut self, storage: &mut dyn eboard::Storage) {
        eboard::set_value(storage, eboard::APP_KEY, &(&self.name, self.age));
    }

    fn on_exit(&mut self) {
        log::info!("Exiting after {} frames", self.header.frames());
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let result = eboard::run(eboard::Options::default(), |cc| {
        log::info!("Starting on {}", cc.screen.output_name);
        egui_extras::install_image_loaders(&cc.egui_ctx);
        #[cfg(feature = "persistence")]
        let (name, age) = cc
            .storage
            .and_then(|storage| eboard::get_value(storage, eboard::APP_KEY))
            .unwrap_or_else(|| ("Arthur".to_owned(), 42));
        #[cfg(not(feature = "persistence"))]
        let (name, age) = ("Arthur".to_owned(), 42);
        Ok(Box::new(Hello {
            header: common::Header::new("Hello from eboard"),
            name,
            age,
        }))
    });
    if let Err(e) = result {
        log::error!("{e}");
        std::process::exit(1);
    }
}
