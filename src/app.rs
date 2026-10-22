#[cfg(feature = "glow")]
use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "glow")]
use crate::glow;

#[cfg(feature = "persistence")]
use crate::Storage;
use crate::platform::backlight::Backlight;
use crate::{Error, Rotation};

/// Implement this for your application. Mirrors `eframe::App`.
pub trait App {
    /// Build the UI. Called every time the screen is repainted, which only happens when something
    /// changed (input, animation, [`egui::Context::request_repaint`]).
    fn ui(&mut self, ui: &mut egui::Ui, board: &mut Board);

    /// Called before [`Self::ui`] each frame, for logic that does not need a [`egui::Ui`].
    fn logic(&mut self, _ctx: &egui::Context, _board: &mut Board) {}

    /// Called once before the run loop exits (window close, [`Board::close`], SIGINT/SIGTERM),
    /// after `save` (feature `persistence`). The display is still up and the GL context is
    /// current.
    fn on_exit(&mut self) {}

    /// Save the app state, usually with [`crate::set_value`] under [`crate::APP_KEY`]. Called on
    /// exit and every [`Self::auto_save_interval`] after something was repainted (feature
    /// `persistence`). The file is only written if the stored state changed.
    ///
    /// The process may be killed or lose power without exiting, so anything that must not get
    /// lost should be saved (and flushed with [`Board::storage_mut`]) right away.
    #[cfg(feature = "persistence")]
    fn save(&mut self, _storage: &mut dyn Storage) {}

    /// Time between automatic calls to [`Self::save`]. Saving only writes to flash if something
    /// changed, but a shorter interval means more writes while the UI is in use.
    #[cfg(feature = "persistence")]
    fn auto_save_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(30)
    }

    /// Whether egui's memory (collapsed headers, scroll positions, zoom, ...) is saved too.
    #[cfg(feature = "persistence")]
    fn persist_egui_memory(&self) -> bool {
        true
    }

    /// Color the screen is cleared to before egui paints, in gamma space (`[r, g, b, a]`, 0..=1).
    /// Called every frame. The scanout buffer is opaque, so alpha has no effect on the panel.
    ///
    /// Pixels egui does not paint keep exactly this color, e.g. a key color for a video overlay.
    /// The default central panel covers the screen with `panel_fill`, though; give it
    /// [`egui::Frame::NONE`] to show the clear color (see `examples/hud.rs`).
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    /// Edit the input before egui sees it, e.g. to inject events from your own input source.
    fn raw_input_hook(&mut self, _ctx: &egui::Context, _raw_input: &mut egui::RawInput) {}
}

/// Static facts about the output egui is drawing to.
#[derive(Clone, Debug)]
pub struct ScreenInfo {
    /// Name of the backend in use, e.g. `"kms"`.
    pub backend: &'static str,
    /// Human-readable output name, e.g. `"vc4 DSI-1"`.
    pub output_name: String,
    /// Size egui renders at, in pixels (after rotation).
    pub size_px: [u32; 2],
    /// Size of the scanout buffer in pixels (the panel's native orientation).
    pub panel_size_px: [u32; 2],
    /// Physical size of the panel in mm (native orientation), if known.
    pub physical_size_mm: Option<[u32; 2]>,
    pub rotation: Rotation,
    /// Whether the rotation is done by the display controller (no extra GPU pass).
    pub hardware_rotation: bool,
    /// Refresh rate of the display mode in Hz.
    pub refresh_hz: f32,
    /// `native_pixels_per_point` handed to egui.
    pub native_pixels_per_point: f32,
    /// `GL_RENDERER`, `GL_VERSION` etc. for diagnostics.
    pub renderer_info: String,
}

/// Handle to the board and the integration, passed to [`App`] every frame. Plays the role of
/// `eframe::Frame`.
pub struct Board {
    pub(crate) screen: ScreenInfo,
    pub(crate) close_requested: bool,
    pub(crate) cpu_usage: Option<f32>,
    #[cfg(feature = "glow")]
    pub(crate) gl: Option<Arc<glow::Context>>,
    #[cfg(feature = "persistence")]
    pub(crate) storage: Option<Box<dyn Storage>>,
    pub(crate) backlight: Option<Backlight>,
    /// Set by [`Board::set_blank_after`], applied by the runner after the frame.
    pub(crate) blank_after_request: Option<Option<Duration>>,
}

impl Board {
    /// The output egui draws to.
    pub fn screen(&self) -> &ScreenInfo {
        &self.screen
    }

    /// CPU time (seconds) spent on the previous frame: running the UI, tessellating and issuing
    /// GL calls. Excludes waiting for vsync.
    pub fn cpu_usage(&self) -> Option<f32> {
        self.cpu_usage
    }

    /// Brightness of the panel backlight, `0.0..=1.0`. `None` if there is no backlight device
    /// (see [`crate::OutputConfig::backlight`]) or it can't be read.
    pub fn brightness(&self) -> Option<f32> {
        self.backlight
            .as_ref()?
            .get()
            .inspect_err(|e| log::debug!("Reading the backlight: {e}"))
            .ok()
    }

    /// Sets the backlight brightness, `0.0..=1.0`. On many panels 0 turns the backlight off
    /// completely, so a UI slider should start above 0. Fails if there is no backlight device or
    /// no permission to write it (the user needs the `video` group or a udev rule).
    pub fn set_brightness(&mut self, brightness: f32) -> Result<(), Error> {
        let backlight = self.backlight.as_ref().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no backlight device")
        })?;
        Ok(backlight.set(brightness)?)
    }

    /// Changes how long the display waits without input before it blanks (see
    /// [`crate::Options::blank_after`]); `None` turns blanking off, e.g. while a video plays or
    /// an alarm is shown. Takes effect after this frame and counts from the last input.
    pub fn set_blank_after(&mut self, blank_after: Option<Duration>) {
        self.blank_after_request = Some(blank_after);
    }

    /// Exit the run loop after this frame. Same as sending
    /// [`egui::ViewportCommand::Close`].
    pub fn close(&mut self) {
        self.close_requested = true;
    }

    /// The OpenGL ES context, for custom painting with [`egui::PaintCallback`] and
    /// [`egui_glow::CallbackFn`].
    ///
    /// When the UI is rotated in software, egui is drawn into an offscreen framebuffer. A paint
    /// callback that binds its own framebuffer must restore the previous binding
    /// (`GL_FRAMEBUFFER_BINDING`) rather than binding framebuffer 0.
    #[cfg(feature = "glow")]
    pub fn gl(&self) -> Option<&Arc<glow::Context>> {
        self.gl.as_ref()
    }

    /// The state store, `None` if no path for it could be found (see
    /// [`crate::Options::persistence_path`]).
    #[cfg(feature = "persistence")]
    pub fn storage(&self) -> Option<&dyn Storage> {
        self.storage.as_deref()
    }

    /// The state store, e.g. to save and [`Storage::flush`] something important immediately.
    #[cfg(feature = "persistence")]
    pub fn storage_mut(&mut self) -> Option<&mut (dyn Storage + 'static)> {
        self.storage.as_deref_mut()
    }
}

/// Passed to the app creator in [`crate::run`]. Mirrors `eframe::CreationContext`.
pub struct CreationContext<'a> {
    /// Set fonts, styles and zoom here.
    pub egui_ctx: egui::Context,

    /// The OpenGL ES context, for creating textures and other GL resources up front.
    #[cfg(feature = "glow")]
    pub gl: Option<Arc<glow::Context>>,

    /// The output the app will be drawn to.
    pub screen: &'a ScreenInfo,

    /// State saved by [`App::save`] in an earlier run (feature `persistence`). Read it with
    /// [`crate::get_value`]. `None` if no path for it could be found.
    #[cfg(feature = "persistence")]
    pub storage: Option<&'a dyn Storage>,
}
