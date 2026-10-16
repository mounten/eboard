//! Vivante GPU on the Linux framebuffer, the equivalent of Qt's `eglfs_viv`.
//!
//! For i.MX6 BSPs (and other Vivante GC-series boards) whose `imx-gpu-viv` driver is built for
//! fbdev instead of KMS/GBM. Its libEGL brings a small window system of its own:
//! `fbGetDisplayByIndex` opens `/dev/fbN`, and `fbCreateWindow` returns the native window for
//! `eglCreateWindowSurface`. The GPU renders straight into the framebuffer; with
//! `FB_MULTI_BUFFER=2` the driver double-buffers in the framebuffer's virtual area and
//! `eglSwapBuffers` pans to the new page at vblank.
//!
//! Nothing is linked at build time: the fb* functions come from the same `libEGL.so.1` that
//! [`Egl`] loads.

use std::ffi::{c_int, c_ulong, c_void};
use std::fs::File;
use std::os::fd::{AsRawFd as _, BorrowedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use egui_glow::glow;

use super::egl::{ConfigRequest, Egl, NativeDisplay};
use super::{Platform, Started, fbdev, vt};
use crate::options::fb_index;
use crate::render::glow::GlowRenderer;
use crate::{Error, Options, Rotation};

const LIBEGL: &str = "libEGL.so.1";

/// Fraction of the frame period used as the minimum frame interval while `eglSwapBuffers` blocks
/// on vblank. Shorter than a period, so a refresh rate computed slightly too low cannot make
/// frame starts drift past vblank and drop a frame every few seconds; the swap does the pacing.
const BLOCKING_SWAP_INTERVAL: f32 = 0.9;

// Signatures from Vivante's `EGL/eglvivante.h` (`EGL_API_FB`).
type FbGetDisplayByIndex = unsafe extern "C" fn(c_int) -> *mut c_void;
type FbGetDisplayInfo =
    unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int, *mut c_ulong, *mut c_int, *mut c_int);
type FbCreateWindow = unsafe extern "C" fn(*mut c_void, c_int, c_int, c_int, c_int) -> *mut c_void;
type FbDestroy = unsafe extern "C" fn(*mut c_void);

/// The fbdev window-system functions of Vivante's libEGL.
struct FbApi {
    get_display_by_index: FbGetDisplayByIndex,
    get_display_info: FbGetDisplayInfo,
    destroy_display: FbDestroy,
    create_window: FbCreateWindow,
    destroy_window: FbDestroy,
    /// Keeps the function pointers valid.
    _library: libloading::Library,
}

impl FbApi {
    fn load() -> Result<Self, Error> {
        // SAFETY: loading libEGL runs its initializers, as loading it for EGL itself does.
        let library = unsafe { libloading::Library::new(LIBEGL) }
            .map_err(|e| Error::Egl(format!("loading {LIBEGL}: {e}")))?;
        // SAFETY: the types match the declarations in `eglvivante.h`.
        unsafe {
            let get_display_by_index = symbol(&library, "fbGetDisplayByIndex").map_err(|_| {
                Error::Egl(format!(
                    "{LIBEGL} is not the Vivante fbdev EGL (it has no fbGetDisplayByIndex)"
                ))
            })?;
            Ok(Self {
                get_display_by_index,
                get_display_info: symbol(&library, "fbGetDisplayInfo")?,
                destroy_display: symbol(&library, "fbDestroyDisplay")?,
                create_window: symbol(&library, "fbCreateWindow")?,
                destroy_window: symbol(&library, "fbDestroyWindow")?,
                _library: library,
            })
        }
    }
}

/// # Safety
/// `T` must be the function pointer type of the symbol.
unsafe fn symbol<T: Copy>(library: &libloading::Library, name: &str) -> Result<T, Error> {
    unsafe { library.get::<T>(name.as_bytes()) }
        .map(|symbol| *symbol)
        .map_err(|e| Error::Egl(format!("{LIBEGL}: {e}")))
}

/// Brings up the display and the GLES renderer.
pub(crate) fn open(options: &Options) -> Result<Started<VivFbPlatform>, Error> {
    let platform = VivFbPlatform::new(options)?;
    let gl = platform
        .gl
        .clone()
        .expect("GL context is created in VivFbPlatform::new");
    super::glow_started(platform, gl, options)
}

pub(crate) struct VivFbPlatform {
    // Teardown order is handled in `Drop`: GL, EGL surface, native window, EGL context and
    // display, native display, then libEGL itself and the console mode.
    egl: Option<Egl>,
    gl: Option<Arc<glow::Context>>,
    /// From `fbCreateWindow`, null until created.
    window: *mut c_void,
    /// From `fbGetDisplayByIndex`, never null.
    display: *mut c_void,
    fb: FbApi,

    /// Our handle on the framebuffer, for blanking.
    file: File,
    blanked: bool,
    output_name: String,
    panel_size: [u32; 2],
    physical_size_mm: Option<[u32; 2]>,
    refresh_hz: f32,
    /// `eglSwapBuffers` waits for vblank (double-buffered with vsync).
    swap_blocks: bool,
    _vt: Option<vt::VtGuard>,
}

impl VivFbPlatform {
    fn new(options: &Options) -> Result<Self, Error> {
        // First, so that `Auto` moves on quickly on boards with another EGL (Mesa, Mali, ...).
        let fb = FbApi::load()?;

        let path = options
            .output
            .fb_device
            .clone()
            .unwrap_or_else(|| PathBuf::from("/dev/fb0"));
        let index = fb_index(&path)
            .or_else(|| fb_index(&std::fs::canonicalize(&path).ok()?))
            .ok_or_else(|| fbdev::fb_err(&path, "not a /dev/fbN device"))?;
        // Our own handle for what the Vivante API does not tell: timings, physical and virtual
        // size. Opening it here also gives a clear error for permission problems.
        let (file, _, _, id) = fbdev::open_and_query(&path)?;
        fbdev::unblank(file.as_raw_fd());

        // Like Qt's eglfs_viv: the driver reads this when it opens the display.
        if options.vsync && std::env::var_os("FB_MULTI_BUFFER").is_none() {
            // SAFETY: eboard has not started any threads yet (input and the app come later). Like
            // Qt, this assumes the app does not use the environment from other threads while
            // `run` starts up.
            unsafe { std::env::set_var("FB_MULTI_BUFFER", "2") };
            log::info!("viv-fb: setting FB_MULTI_BUFFER=2 (double buffering, vsync)");
        }
        let multi_buffer: u32 = std::env::var("FB_MULTI_BUFFER")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1);

        // SAFETY: plain C call; null on failure.
        let display = unsafe { (fb.get_display_by_index)(index) };
        if display.is_null() {
            return Err(Error::Egl(format!(
                "fbGetDisplayByIndex({index}) failed{}",
                galcore_hint()
            )));
        }
        let mut this = Self {
            egl: None,
            gl: None,
            window: std::ptr::null_mut(),
            display,
            fb,
            file,
            blanked: false,
            output_name: format!("{id} {}", path.display()),
            panel_size: [0, 0],
            physical_size_mm: None,
            refresh_hz: 60.0,
            swap_blocks: false,
            _vt: None,
        };

        let (mut width, mut height, mut stride, mut bpp): (c_int, c_int, c_int, c_int) =
            (0, 0, 0, 0);
        let mut physical: c_ulong = 0;
        // SAFETY: the display is valid; the function writes the five out-parameters.
        unsafe {
            (this.fb.get_display_info)(
                display,
                &mut width,
                &mut height,
                &mut physical,
                &mut stride,
                &mut bpp,
            )
        };
        if width <= 0 || height <= 0 {
            return Err(fbdev::fb_err(
                &path,
                &format!("fbGetDisplayInfo reports {width}x{height} px"),
            ));
        }
        this.panel_size = [width as u32, height as u32];

        // Read again: with FB_MULTI_BUFFER the driver enlarges the virtual size itself.
        let var = fbdev::var_screeninfo(this.file.as_raw_fd())
            .map_err(|e| fbdev::fb_err(&path, &format!("FBIOGET_VSCREENINFO: {e}")))?;
        this.refresh_hz = fbdev::refresh_hz(&var);
        this.physical_size_mm = options
            .output
            .physical_size_mm
            .or_else(|| fbdev::physical_size_mm(&var));
        log::info!(
            "viv-fb: {} ({id}), {width}x{height} {bpp} bpp (stride {stride}), virtual {}x{}, \
             {:.1} Hz, FB_MULTI_BUFFER={multi_buffer}, physical size {}",
            path.display(),
            var.xres_virtual,
            var.yres_virtual,
            this.refresh_hz,
            this.physical_size_mm
                .map_or("unknown".to_owned(), |[w, h]| format!("{w}x{h} mm")),
        );
        let enough_pages = var.yres_virtual >= var.yres.saturating_mul(multi_buffer);
        this.swap_blocks = options.vsync && multi_buffer >= 2 && enough_pages;
        if multi_buffer >= 2 && !enough_pages {
            log::warn!(
                "{} has {} lines of virtual space, FB_MULTI_BUFFER={multi_buffer} needs {}: the \
                 driver may fall back to a single buffer (tearing). Reserve more framebuffer \
                 memory in the kernel/device tree",
                path.display(),
                var.yres_virtual,
                var.yres.saturating_mul(multi_buffer),
            );
        }

        this.init_egl(options, bpp)?;
        this._vt = vt::enter_graphics_mode();
        Ok(this)
    }

    fn init_egl(&mut self, options: &Options, bpp: c_int) -> Result<(), Error> {
        // SAFETY: the native display outlives `Egl` (destroyed after it in `Drop`).
        let egl = unsafe {
            Egl::new(
                NativeDisplay::Legacy(self.display),
                &ConfigRequest {
                    native_visual_ids: &[],
                    samples: options.multisampling,
                    buffer_size: Some(bpp),
                },
            )
        }
        .map_err(|e| match e {
            Error::Egl(msg) => Error::Egl(format!("{msg}{}", galcore_hint())),
            e => e,
        })?;
        let egl = self.egl.insert(egl);

        let [w, h] = self.panel_size.map(|v| v as c_int);
        // SAFETY: the display is valid.
        self.window = unsafe { (self.fb.create_window)(self.display, 0, 0, w, h) };
        if self.window.is_null() {
            return Err(Error::Egl(format!("fbCreateWindow({w}x{h}) failed")));
        }
        // SAFETY: the native window outlives the EGL surface (destroyed after it in `Drop`).
        unsafe { egl.create_window_surface(self.window)? };
        log::debug!("Vivante window {w}x{h}, GLES {}", egl.gles_major);

        if !options.vsync
            && let Err(e) = egl.set_swap_interval(0)
        {
            log::info!("vsync off requested, but {e}");
        }

        self.gl = Some(Arc::new(egl.create_glow_context()));
        Ok(())
    }
}

/// Hint for EGL failures when the GPU kernel driver is missing.
fn galcore_hint() -> &'static str {
    if Path::new("/dev/galcore").exists() {
        ""
    } else {
        " (/dev/galcore is missing: is the galcore kernel module loaded?)"
    }
}

impl Platform for VivFbPlatform {
    type Renderer = GlowRenderer;

    fn name(&self) -> &'static str {
        "viv-fb"
    }

    fn output_name(&self) -> String {
        self.output_name.clone()
    }

    fn panel_size(&self) -> [u32; 2] {
        self.panel_size
    }

    fn physical_size_mm(&self) -> Option<[u32; 2]> {
        self.physical_size_mm
    }

    fn refresh_hz(&self) -> f32 {
        self.refresh_hz
    }

    fn hardware_rotation(&self) -> Rotation {
        Rotation::Deg0
    }

    fn can_render(&self) -> bool {
        true
    }

    fn present(&mut self, _renderer: &mut GlowRenderer) -> Result<(), Error> {
        // Blocks until vblank when the driver double-buffers (FB_MULTI_BUFFER >= 2).
        self.egl
            .as_ref()
            .expect("EGL is initialized")
            .swap_buffers()
    }

    /// Paces animations when `eglSwapBuffers` does not block (single buffer or vsync off). When
    /// it does, this is only a safety net and stays below one frame period.
    fn frame_interval(&self) -> Option<Duration> {
        let fraction = if self.swap_blocks {
            BLOCKING_SWAP_INTERVAL
        } else {
            1.0
        };
        Some(Duration::from_secs_f32(fraction / self.refresh_hz))
    }

    fn event_fd(&self) -> Option<BorrowedFd<'_>> {
        None
    }

    fn dispatch_events(&mut self) -> Result<(), Error> {
        Ok(())
    }

    fn set_blanked(&mut self, blanked: bool) -> Result<(), Error> {
        fbdev::blank(self.file.as_raw_fd(), blanked)
            .map_err(|e| Error::Framebuffer(format!("{}: FBIOBLANK: {e}", self.output_name)))?;
        self.blanked = blanked;
        Ok(())
    }
}

impl Drop for VivFbPlatform {
    fn drop(&mut self) {
        if self.blanked {
            fbdev::unblank(self.file.as_raw_fd());
        }
        self.gl = None;
        if let Some(egl) = &mut self.egl {
            egl.destroy_surface();
        }
        if !self.window.is_null() {
            // SAFETY: created by fbCreateWindow; its EGL surface is gone.
            unsafe { (self.fb.destroy_window)(self.window) };
        }
        // Context and eglTerminate before the native display they were created on.
        self.egl = None;
        // SAFETY: created by fbGetDisplayByIndex; nothing uses it anymore.
        unsafe { (self.fb.destroy_display)(self.display) };
        // `fb` (unloading libEGL) and the console guard are dropped after this, in field order.
    }
}
