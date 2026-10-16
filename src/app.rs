#[cfg(feature = "glow")]
use std::sync::Arc;

#[cfg(feature = "glow")]
use crate::glow;

use crate::Rotation;

/// Implement this for your application. Mirrors `eframe::App`.
pub trait App {
    /// Build the UI. Called every time the screen is repainted, which only happens when something
    /// changed (input, animation, [`egui::Context::request_repaint`]).
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame);

    /// Called before [`Self::ui`] each frame, for logic that does not need a [`egui::Ui`].
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut Frame) {}

    /// Called once before the run loop exits (window close, [`Frame::close`], SIGINT/SIGTERM).
    /// The display is still up and the GL context is current.
    fn on_exit(&mut self) {}

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

/// Information about the integration, passed to [`App`] every frame. Mirrors `eframe::Frame`.
pub struct Frame {
    pub(crate) screen: ScreenInfo,
    pub(crate) close_requested: bool,
    pub(crate) cpu_usage: Option<f32>,
    #[cfg(feature = "glow")]
    pub(crate) gl: Option<Arc<glow::Context>>,
}

impl Frame {
    /// The output egui draws to.
    pub fn screen(&self) -> &ScreenInfo {
        &self.screen
    }

    /// CPU time (seconds) spent on the previous frame: running the UI, tessellating and issuing
    /// GL calls. Excludes waiting for vsync.
    pub fn cpu_usage(&self) -> Option<f32> {
        self.cpu_usage
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
}
