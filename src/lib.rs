//! eframe for embedded boards: run [egui](https://github.com/emilk/egui) apps full-screen on
//! embedded Linux without a compositor or winit, like Qt's `eglfs` and `linuxfb` platform plugins.
//!
//! Backends (cargo features, picked at runtime with [`Backend`]):
//! - `kms`: DRM/KMS + GBM + EGL, painted with OpenGL ES (`egui_glow`).
//! - `viv-fb`: Vivante GPU on `/dev/fbN` through NXP's fbdev EGL, painted with OpenGL ES. For
//!   i.MX6 BSPs without KMS/GBM (like Qt's `eglfs_viv`).
//! - `linuxfb`: `/dev/fbN`, painted on the CPU. For boards without KMS/GBM or GPU drivers.
//!
//! The API mirrors eframe:
//!
//! ```no_run
//! struct Hello;
//!
//! impl eboard::App for Hello {
//!     fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eboard::Frame) {
//!         ui.heading("Hello from KMS!");
//!     }
//! }
//!
//! eboard::run(eboard::Options::default(), |_cc| Ok(Box::new(Hello))).unwrap();
//! ```
//!
//! # Fonts
//!
//! The `default_fonts` feature (on by default) bundles egui's fonts. An app that ships its own
//! fonts can turn it off and install them when it is created:
//!
//! ```no_run
//! # struct MyApp;
//! # impl eboard::App for MyApp {
//! #     fn ui(&mut self, _ui: &mut egui::Ui, _frame: &mut eboard::Frame) {}
//! # }
//! use std::sync::Arc;
//!
//! eboard::run(eboard::Options::default(), |cc| {
//!     let mut fonts = egui::FontDefinitions::empty();
//!     let font = std::fs::read("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf")?;
//!     fonts.font_data.insert("DejaVuSans".into(), Arc::new(egui::FontData::from_owned(font)));
//!     for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
//!         fonts.families.insert(family, vec!["DejaVuSans".into()]);
//!     }
//!     cc.egui_ctx.set_fonts(fonts);
//!     Ok(Box::new(MyApp))
//! })
//! .unwrap();
//! ```
//!
//! Without any fonts, egui draws boxes instead of text. Cargo features are additive: if another
//! dependency enables `egui/default_fonts`, the fonts are bundled anyway.
//!
//! # Configuration
//!
//! [`Options`] can be overridden at runtime with environment variables, like Qt's
//! `QT_QPA_EGLFS_*`:
//!
//! | Variable | Example | |
//! |---|---|---|
//! | `EBOARD_BACKEND` | `kms` | backend (`auto`, `kms`, `viv-fb`, `linuxfb`) |
//! | `EBOARD_KMS_DEVICE` | `/dev/dri/card1` | DRM device |
//! | `EBOARD_FB_DEVICE` | `/dev/fb1` | framebuffer device for `viv-fb`/`linuxfb` (default `/dev/fb0`) |
//! | `EBOARD_KMS_CONNECTOR` | `DSI-1`, `HDMI-A-1` | output |
//! | `EBOARD_KMS_MODE` | `1280x720@60` | display mode |
//! | `EBOARD_ROTATION` | `90` | clockwise UI rotation (0, 90, 180, 270) |
//! | `EBOARD_PHYSICAL_SIZE` | `155x88` | panel size in mm, for the scale factor |
//! | `EBOARD_PPP` | `1.5` | fixed `native_pixels_per_point` |
//! | `EBOARD_VSYNC` | `0` | async page flips / no `FBIO_WAITFORVSYNC` |
//! | `EBOARD_PERSISTENCE_PATH` | `/var/lib/app/state.ron` | state file (feature `persistence`) |
//!
//! # Persistence
//!
//! With the `persistence` feature, `App::save` stores state in a `Storage` that is written
//! atomically to a RON file (only when it changed) and handed back through
//! `CreationContext::storage` on the next start, like eframe. See `Options::persistence_path`
//! for where the file goes.
//!
//! Set `RUST_LOG=debug` (with a logger such as `env_logger`) to see the EGL/GL implementation and
//! the chosen device, connector and mode.

// Without a backend for this OS (e.g. on a development host) most internals are unused.
#![cfg_attr(
    any(
        not(target_os = "linux"),
        not(any(feature = "kms", feature = "viv-fb", feature = "linuxfb"))
    ),
    allow(dead_code)
)]

mod app;
mod error;
mod geometry;
#[cfg(feature = "input-evdev")]
mod input;
mod options;
mod platform;
mod render;
#[cfg(target_os = "linux")]
mod runner;
#[cfg(target_os = "linux")]
mod signals;
#[cfg(feature = "persistence")]
mod storage;

pub use egui;
#[cfg(feature = "glow")]
pub use egui_glow;
#[cfg(feature = "glow")]
pub use egui_glow::glow;

pub use app::{App, CreationContext, Frame, ScreenInfo};
pub use error::Error;
pub use geometry::{Rotation, TouchTransform, pixels_per_point_from_size};
pub use options::{Backend, InputGrab, InputOptions, ModeRequest, Options, OutputConfig};
#[cfg(feature = "persistence")]
pub use storage::{APP_KEY, Storage, get_value, set_value};

/// Error type app creators may return.
pub type DynError = Box<dyn std::error::Error + Send + Sync>;

/// Creates the app with `app_creator` and runs it full-screen until it closes or the process
/// receives SIGINT/SIGTERM. The display is restored before returning.
///
/// `EBOARD_*` environment variables override `options` (see the crate docs).
pub fn run(
    mut options: Options,
    app_creator: impl FnOnce(&CreationContext<'_>) -> Result<Box<dyn App>, DynError>,
) -> Result<(), Error> {
    options.apply_env_overrides()?;
    log::debug!("Options: {options:?}");
    backends::run(&options, app_creator)
}

#[cfg(target_os = "linux")]
mod backends {
    use super::*;
    use platform::{Platform, Started};
    use render::Renderer as _;

    pub(super) fn run(
        options: &Options,
        app_creator: impl FnOnce(&CreationContext<'_>) -> Result<Box<dyn App>, DynError>,
    ) -> Result<(), Error> {
        match options.backend {
            #[cfg(feature = "kms")]
            Backend::Kms => run_on(platform::kms::open(options)?, options, app_creator),
            #[cfg(feature = "viv-fb")]
            Backend::VivFb => run_on(platform::vivfb::open(options)?, options, app_creator),
            #[cfg(feature = "linuxfb")]
            Backend::LinuxFb => run_on(platform::linuxfb::open(options)?, options, app_creator),
            Backend::Auto => {
                // Like Qt: GPU path first, framebuffer as the fallback.
                let mut failures: Vec<String> = Vec::new();
                #[cfg(feature = "kms")]
                match platform::kms::open(options) {
                    Ok(started) => return run_on(started, options, app_creator),
                    Err(e) => {
                        log::info!("kms backend unavailable: {e}");
                        failures.push(format!("kms: {e}"));
                    }
                }
                #[cfg(feature = "viv-fb")]
                match platform::vivfb::open(options) {
                    Ok(started) => return run_on(started, options, app_creator),
                    Err(e) => {
                        log::info!("viv-fb backend unavailable: {e}");
                        failures.push(format!("viv-fb: {e}"));
                    }
                }
                #[cfg(feature = "linuxfb")]
                match platform::linuxfb::open(options) {
                    Ok(started) => return run_on(started, options, app_creator),
                    Err(e) => {
                        log::info!("linuxfb backend unavailable: {e}");
                        failures.push(format!("linuxfb: {e}"));
                    }
                }
                if failures.is_empty() {
                    failures.push(
                        "no backend compiled in (enable `kms`, `viv-fb` or `linuxfb`)".into(),
                    );
                }
                drop(app_creator);
                Err(Error::NoBackend(failures.join("; ")))
            }
            #[allow(unreachable_patterns, reason = "depends on enabled features")]
            backend => Err(Error::BackendUnavailable(backend)),
        }
    }

    /// Runs the app on an initialized platform.
    fn run_on<P: Platform>(
        started: Started<P>,
        options: &Options,
        app_creator: impl FnOnce(&CreationContext<'_>) -> Result<Box<dyn App>, DynError>,
    ) -> Result<(), Error> {
        let Started {
            mut platform,
            mut renderer,
            renderer_info,
            #[cfg(feature = "glow")]
            gl,
        } = started;

        let rotation = options.output.rotation;
        let panel_size = platform.panel_size();
        let size_px = rotation.logical_size(panel_size);
        let native_pixels_per_point = options.output.pixels_per_point.unwrap_or_else(|| {
            let size_mm = platform
                .physical_size_mm()
                .map(|mm| rotation.logical_size(mm));
            pixels_per_point_from_size(size_px, size_mm)
        });

        let screen = ScreenInfo {
            backend: platform.name(),
            output_name: platform.output_name(),
            size_px,
            panel_size_px: panel_size,
            physical_size_mm: platform.physical_size_mm(),
            rotation,
            hardware_rotation: platform.hardware_rotation() != Rotation::Deg0,
            refresh_hz: platform.refresh_hz(),
            native_pixels_per_point,
            renderer_info,
        };
        log::info!(
            "Screen: {} ({}) {}x{} px, rotation {}°, pixels_per_point {native_pixels_per_point}",
            screen.output_name,
            screen.backend,
            size_px[0],
            size_px[1],
            rotation.degrees(),
        );

        let egui_ctx = egui::Context::default();
        #[cfg(feature = "persistence")]
        let storage = load_storage(options, &egui_ctx);
        let creation_context = CreationContext {
            egui_ctx: egui_ctx.clone(),
            #[cfg(feature = "glow")]
            gl: gl.clone(),
            screen: &screen,
            #[cfg(feature = "persistence")]
            storage: storage.as_deref(),
        };
        let mut app = match app_creator(&creation_context) {
            Ok(app) => app,
            Err(e) => {
                renderer.destroy();
                return Err(Error::AppCreation(e));
            }
        };

        let frame = Frame {
            screen,
            close_requested: false,
            cpu_usage: None,
            #[cfg(feature = "glow")]
            gl,
            #[cfg(feature = "persistence")]
            storage,
        };
        let result = runner::Runner {
            #[cfg(feature = "input-evdev")]
            input: input::Input::new(options, panel_size),
            platform: &mut platform,
            renderer: &mut renderer,
            egui_ctx,
            frame,
            size_px,
            native_pixels_per_point,
            handle_signals: options.handle_signals,
        }
        .run(app.as_mut());

        // The app may own GPU resources: drop it while the context is still alive.
        drop(app);
        renderer.destroy();
        drop(platform);
        result
    }

    /// Opens the state file and restores egui's memory from it, like eframe, before the app is
    /// created (so the app's own setup, e.g. its zoom, wins).
    #[cfg(feature = "persistence")]
    fn load_storage(options: &Options, egui_ctx: &egui::Context) -> Option<Box<dyn Storage>> {
        let Some(path) = storage::resolve_path(
            options.persistence_path.as_deref(),
            |key| std::env::var(key).ok(),
            &storage::app_name(),
        ) else {
            log::warn!(
                "No place to save the app state (set EBOARD_PERSISTENCE_PATH or HOME); \
                 persistence is off"
            );
            return None;
        };
        let storage = storage::FileStorage::load(path);
        if let Some(memory) = get_value::<egui::Memory>(&storage, storage::EGUI_MEMORY_KEY) {
            egui_ctx.memory_mut(|m| *m = memory);
        }
        Some(Box::new(storage))
    }
}

#[cfg(not(target_os = "linux"))]
mod backends {
    use super::*;

    pub(super) fn run(
        options: &Options,
        _app_creator: impl FnOnce(&CreationContext<'_>) -> Result<Box<dyn App>, DynError>,
    ) -> Result<(), Error> {
        Err(Error::BackendUnavailable(options.backend))
    }
}
