//! Display platforms: bring up a surface without a window system and present frames to it.

#[cfg(all(target_os = "linux", any(feature = "kms", feature = "viv-fb")))]
pub(crate) mod egl;
#[cfg(all(target_os = "linux", any(feature = "linuxfb", feature = "viv-fb")))]
mod fbdev;
#[cfg(all(target_os = "linux", feature = "kms"))]
pub(crate) mod kms;
#[cfg(all(target_os = "linux", feature = "linuxfb"))]
pub(crate) mod linuxfb;
#[cfg(all(target_os = "linux", feature = "viv-fb"))]
pub(crate) mod vivfb;
#[cfg(all(target_os = "linux", any(feature = "linuxfb", feature = "viv-fb")))]
mod vt;

#[cfg(target_os = "linux")]
use std::os::fd::BorrowedFd;
#[cfg(feature = "glow")]
use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "glow")]
use crate::glow;
use crate::render::Renderer;
#[cfg(all(target_os = "linux", any(feature = "kms", feature = "viv-fb")))]
use crate::render::glow::{GlowRenderer, renderer_info};
use crate::{Error, Rotation};

/// A display output that the renderer draws to.
pub(crate) trait Platform {
    /// The renderer that draws for this platform.
    type Renderer: Renderer;

    /// Short backend name for logs and [`crate::ScreenInfo::backend`].
    fn name(&self) -> &'static str;

    /// Human-readable output description, e.g. `"vc4 DSI-1"`.
    fn output_name(&self) -> String;

    /// Size of the scanout surface in pixels (native panel orientation).
    fn panel_size(&self) -> [u32; 2];

    /// Physical panel size in mm (native orientation), if known.
    fn physical_size_mm(&self) -> Option<[u32; 2]>;

    fn refresh_hz(&self) -> f32;

    /// Part of the requested rotation that the display hardware applies. The renderer does the
    /// rest.
    fn hardware_rotation(&self) -> Rotation;

    /// False while a presented frame is still waiting for vblank. Rendering then has to wait,
    /// otherwise we would block in the driver or run out of buffers.
    fn can_render(&self) -> bool;

    /// Shows the frame the renderer just painted.
    fn present(&mut self, renderer: &mut Self::Renderer) -> Result<(), Error>;

    /// Minimum time between frame starts, for platforms that have no vblank event to pace
    /// animations. `None`: pacing comes from [`Self::can_render`] and [`Self::event_fd`].
    fn frame_interval(&self) -> Option<Duration> {
        None
    }

    /// File descriptor to poll for readability; [`Self::dispatch_events`] handles it.
    #[cfg(target_os = "linux")]
    fn event_fd(&self) -> Option<BorrowedFd<'_>>;

    /// Handles pending platform events (page-flip completion).
    fn dispatch_events(&mut self) -> Result<(), Error>;
}

/// An initialized platform with its renderer, ready to run an app.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct Started<P: Platform> {
    pub platform: P,
    pub renderer: P::Renderer,
    /// For logs and [`crate::ScreenInfo::renderer_info`].
    pub renderer_info: String,
    #[cfg(feature = "glow")]
    pub gl: Option<Arc<glow::Context>>,
}

/// Wraps a GL platform, whose context is current on the scanout surface, with a
/// [`GlowRenderer`]. Whatever rotation the display hardware does not apply, the renderer does.
#[cfg(all(target_os = "linux", any(feature = "kms", feature = "viv-fb")))]
pub(crate) fn glow_started<P: Platform<Renderer = GlowRenderer>>(
    platform: P,
    gl: Arc<glow::Context>,
    options: &crate::Options,
) -> Result<Started<P>, Error> {
    let rotation = options.output.rotation;
    let sw_rotation = if platform.hardware_rotation() == rotation {
        Rotation::Deg0
    } else {
        rotation
    };
    let renderer = GlowRenderer::new(
        gl.clone(),
        options.shader_version,
        options.dithering,
        sw_rotation,
        platform.panel_size(),
    )?;
    let renderer_info = renderer_info(&gl);
    log::info!("GL: {renderer_info}");
    Ok(Started {
        platform,
        renderer,
        renderer_info,
        gl: Some(gl),
    })
}
