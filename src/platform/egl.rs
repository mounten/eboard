//! EGL setup shared by every GL platform: KMS/GBM and Vivante fbdev (EGLDevice later). It must
//! stay free of GBM, so platforms without libgbm can use it.
//!
//! libEGL and libGLESv2 are loaded with `dlopen`, so cross builds need no EGL in the sysroot and the
//! vendor's EGL (Mesa, NVIDIA, Vivante, Mali) is picked up at runtime.

use std::ffi::{CStr, c_void};

use egui_glow::glow;
use khronos_egl as egl;

use crate::Error;

type EglInstance = egl::DynamicInstance<egl::EGL1_4>;

type GetPlatformDisplayExt =
    unsafe extern "system" fn(egl::Enum, *mut c_void, *const egl::Int) -> egl::EGLDisplay;
type CreatePlatformWindowSurfaceExt = unsafe extern "system" fn(
    egl::EGLDisplay,
    egl::EGLConfig,
    *mut c_void,
    *const egl::Int,
) -> egl::EGLSurface;

/// How to obtain the `EGLDisplay`.
pub(crate) enum NativeDisplay {
    /// `eglGetPlatformDisplay[EXT](platform, native)`, e.g. `EGL_PLATFORM_GBM_KHR` with a
    /// `gbm_device*`. Falls back to `eglGetDisplay(native)` if the EGL has no platform support.
    #[cfg_attr(not(feature = "kms"), allow(dead_code))]
    Platform {
        platform: egl::Enum,
        native: *mut c_void,
    },
    /// Plain `eglGetDisplay(native)`, for EGLs with their own window system, e.g. Vivante's fbdev
    /// EGL with the display from `fbGetDisplayByIndex`.
    #[cfg_attr(not(feature = "viv-fb"), allow(dead_code))]
    Legacy(*mut c_void),
}

/// Requirements for the EGL config.
pub(crate) struct ConfigRequest<'a> {
    /// Only accept configs whose `EGL_NATIVE_VISUAL_ID` is one of these, in order of preference.
    /// For GBM this is the DRM fourcc of the scanout format. Empty: accept any.
    pub native_visual_ids: &'a [u32],
    /// MSAA samples, 0 = off.
    pub samples: u16,
    /// Prefer configs with this `EGL_BUFFER_SIZE` (e.g. 16 for an RGB565 framebuffer). Falls back
    /// to the first matching config.
    pub buffer_size: Option<i32>,
}

pub(crate) struct Egl {
    egl: EglInstance,
    display: egl::Display,
    config: egl::Config,
    context: egl::Context,
    surface: Option<egl::Surface>,
    /// The display came from the platform API, so surfaces must too.
    platform_display: bool,
    gles_lib: Option<libloading::Library>,
    /// `EGL_NATIVE_VISUAL_ID` of the chosen config.
    #[cfg_attr(not(feature = "kms"), allow(dead_code))]
    pub(crate) native_visual_id: u32,
    pub(crate) gles_major: i32,
}

fn egl_err(what: &str, e: egl::Error) -> Error {
    Error::Egl(format!("{what}: {e}"))
}

impl Egl {
    /// Loads libEGL, initializes the display and creates a GLES 3 context (falling back to GLES 2).
    ///
    /// # Safety
    /// The native display pointer must be valid for the given platform and outlive `Self`.
    pub(crate) unsafe fn new(
        native: NativeDisplay,
        request: &ConfigRequest<'_>,
    ) -> Result<Self, Error> {
        let egl = unsafe { EglInstance::load_required() }
            .map_err(|e| Error::Egl(format!("loading libEGL.so.1: {e}")))?;

        let client_extensions = egl
            .query_string(None, egl::EXTENSIONS)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        log::debug!("EGL client extensions: {client_extensions}");

        let (display, platform_display) = match native {
            NativeDisplay::Platform { platform, native } => unsafe {
                get_platform_display(&egl, &client_extensions, platform, native)?
            },
            NativeDisplay::Legacy(native) => {
                let display = unsafe { egl.get_display(native) }
                    .ok_or_else(|| Error::Egl("eglGetDisplay returned EGL_NO_DISPLAY".into()))?;
                (display, false)
            }
        };

        let (major, minor) = egl
            .initialize(display)
            .map_err(|e| egl_err("eglInitialize", e))?;
        let query = |name| {
            egl.query_string(Some(display), name)
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        log::info!(
            "EGL {major}.{minor}, vendor: {}, version: {}, client APIs: {}",
            query(egl::VENDOR),
            query(egl::VERSION),
            query(egl::CLIENT_APIS)
        );
        log::debug!("EGL display extensions: {}", query(egl::EXTENSIONS));

        egl.bind_api(egl::OPENGL_ES_API)
            .map_err(|e| egl_err("eglBindAPI(GLES)", e))?;

        let (config, native_visual_id) = match choose_config(&egl, display, request) {
            Ok(c) => c,
            Err(e) => {
                let _ = egl.terminate(display);
                return Err(e);
            }
        };

        let mut gles_major = 0;
        let mut context = None;
        for version in [3, 2] {
            let attribs = [egl::CONTEXT_CLIENT_VERSION, version, egl::NONE];
            match egl.create_context(display, config, None, &attribs) {
                Ok(ctx) => {
                    gles_major = version;
                    context = Some(ctx);
                    break;
                }
                Err(e) => log::debug!("Creating a GLES {version} context failed: {e}"),
            }
        }
        let Some(context) = context else {
            let _ = egl.terminate(display);
            return Err(Error::Egl(
                "could not create a GLES 3 or GLES 2 context".into(),
            ));
        };
        log::debug!("Created GLES {gles_major} context");

        Ok(Self {
            egl,
            display,
            config,
            context,
            surface: None,
            platform_display,
            gles_lib: None,
            native_visual_id,
            gles_major,
        })
    }

    /// Creates the window surface and makes the context current on it.
    ///
    /// # Safety
    /// `native_window` must be a valid native window for the display's platform (for GBM a
    /// `gbm_surface*`) and outlive the surface.
    pub(crate) unsafe fn create_window_surface(
        &mut self,
        native_window: *mut c_void,
    ) -> Result<(), Error> {
        let surface = if self.platform_display {
            unsafe { self.create_platform_window_surface(native_window)? }
        } else {
            unsafe {
                self.egl
                    .create_window_surface(self.display, self.config, native_window, None)
            }
            .map_err(|e| egl_err("eglCreateWindowSurface", e))?
        };
        self.surface = Some(surface);
        self.egl
            .make_current(
                self.display,
                Some(surface),
                Some(surface),
                Some(self.context),
            )
            .map_err(|e| egl_err("eglMakeCurrent", e))
    }

    unsafe fn create_platform_window_surface(
        &self,
        native_window: *mut c_void,
    ) -> Result<egl::Surface, Error> {
        if let Some(egl15) = self.egl.upcast::<egl::EGL1_5>() {
            return unsafe {
                egl15.create_platform_window_surface(
                    self.display,
                    self.config,
                    native_window,
                    &[egl::ATTRIB_NONE],
                )
            }
            .map_err(|e| egl_err("eglCreatePlatformWindowSurface", e));
        }
        let f = self
            .egl
            .get_proc_address("eglCreatePlatformWindowSurfaceEXT")
            .ok_or_else(|| Error::Egl("eglCreatePlatformWindowSurfaceEXT is missing".into()))?;
        let f: CreatePlatformWindowSurfaceExt = unsafe { std::mem::transmute(f) };
        let attribs = [egl::NONE];
        let ptr = unsafe {
            f(
                self.display.as_ptr(),
                self.config.as_ptr(),
                native_window,
                attribs.as_ptr(),
            )
        };
        if ptr.is_null() {
            Err(Error::Egl(format!(
                "eglCreatePlatformWindowSurfaceEXT: {:?}",
                self.egl.get_error()
            )))
        } else {
            Ok(unsafe { egl::Surface::from_ptr(ptr) })
        }
    }

    /// Releases the context and destroys the window surface, so the native window can be destroyed
    /// before the context and display.
    pub(crate) fn destroy_surface(&mut self) {
        let _ = self.egl.make_current(self.display, None, None, None);
        if let Some(surface) = self.surface.take() {
            let _ = self.egl.destroy_surface(self.display, surface);
        }
    }

    /// `eglSwapInterval`: 0 = don't wait for vblank. Needs a current context.
    #[cfg_attr(not(feature = "viv-fb"), allow(dead_code))]
    pub(crate) fn set_swap_interval(&self, interval: i32) -> Result<(), Error> {
        self.egl
            .swap_interval(self.display, interval)
            .map_err(|e| egl_err("eglSwapInterval", e))
    }

    pub(crate) fn swap_buffers(&self) -> Result<(), Error> {
        let surface = self
            .surface
            .ok_or_else(|| Error::Egl("no window surface".into()))?;
        self.egl
            .swap_buffers(self.display, surface)
            .map_err(|e| egl_err("eglSwapBuffers", e))
    }

    /// Creates a `glow` context for the current GLES context.
    pub(crate) fn create_glow_context(&mut self) -> glow::Context {
        // EGL < 1.5 without EGL_KHR_get_all_proc_addresses only returns extension functions from
        // eglGetProcAddress, so look up core functions in libGLESv2 first.
        if self.gles_lib.is_none() {
            self.gles_lib = ["libGLESv2.so.2", "libGLESv2.so"]
                .into_iter()
                .find_map(|name| unsafe { libloading::Library::new(name) }.ok());
            if self.gles_lib.is_none() {
                log::debug!(
                    "libGLESv2 not found, resolving GL functions via eglGetProcAddress only"
                );
            }
        }
        let gles_lib = self.gles_lib.as_ref();
        let egl = &self.egl;
        unsafe {
            glow::Context::from_loader_function_cstr(|name: &CStr| {
                if let Some(lib) = gles_lib
                    && let Ok(sym) = lib.get::<*const c_void>(name.to_bytes_with_nul())
                    && !sym.is_null()
                {
                    return *sym;
                }
                name.to_str()
                    .ok()
                    .and_then(|name| egl.get_proc_address(name))
                    .map_or(std::ptr::null(), |f| f as *const c_void)
            })
        }
    }
}

impl Drop for Egl {
    fn drop(&mut self) {
        self.destroy_surface();
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.terminate(self.display);
    }
}

unsafe fn get_platform_display(
    egl: &EglInstance,
    client_extensions: &str,
    platform: egl::Enum,
    native: *mut c_void,
) -> Result<(egl::Display, bool), Error> {
    if let Some(egl15) = egl.upcast::<egl::EGL1_5>() {
        match unsafe { egl15.get_platform_display(platform, native, &[egl::ATTRIB_NONE]) } {
            Ok(display) => return Ok((display, true)),
            Err(e) => log::debug!("eglGetPlatformDisplay({platform:#x}) failed: {e}"),
        }
    }
    if has_extension(client_extensions, "EGL_EXT_platform_base")
        && let Some(f) = egl.get_proc_address("eglGetPlatformDisplayEXT")
    {
        let f: GetPlatformDisplayExt = unsafe { std::mem::transmute(f) };
        let attribs = [egl::NONE];
        let ptr = unsafe { f(platform, native, attribs.as_ptr()) };
        if !ptr.is_null() {
            return Ok((unsafe { egl::Display::from_ptr(ptr) }, true));
        }
        log::debug!(
            "eglGetPlatformDisplayEXT({platform:#x}) failed: {:?}",
            egl.get_error()
        );
    }
    log::debug!("Falling back to eglGetDisplay");
    let display = unsafe { egl.get_display(native) }
        .ok_or_else(|| Error::Egl("eglGetDisplay returned EGL_NO_DISPLAY".into()))?;
    Ok((display, false))
}

fn has_extension(extensions: &str, name: &str) -> bool {
    extensions.split_ascii_whitespace().any(|e| e == name)
}

fn choose_config(
    egl: &EglInstance,
    display: egl::Display,
    request: &ConfigRequest<'_>,
) -> Result<(egl::Config, u32), Error> {
    let samples = egl::Int::from(request.samples);
    let attribs = [
        egl::SURFACE_TYPE,
        egl::WINDOW_BIT,
        egl::RENDERABLE_TYPE,
        egl::OPENGL_ES2_BIT,
        egl::RED_SIZE,
        5,
        egl::GREEN_SIZE,
        6,
        egl::BLUE_SIZE,
        5,
        egl::SAMPLE_BUFFERS,
        i32::from(samples > 0),
        egl::SAMPLES,
        samples,
        egl::NONE,
    ];
    let count = egl
        .matching_config_count(display, &attribs)
        .map_err(|e| egl_err("eglChooseConfig", e))?;
    let mut configs = Vec::with_capacity(count);
    egl.choose_config(display, &attribs, &mut configs)
        .map_err(|e| egl_err("eglChooseConfig", e))?;

    let visual_id = |config| {
        egl.get_config_attrib(display, config, egl::NATIVE_VISUAL_ID)
            .map(|v| v as u32)
            .unwrap_or(0)
    };

    let chosen = if request.native_visual_ids.is_empty() {
        let buffer_size = |config| {
            egl.get_config_attrib(display, config, egl::BUFFER_SIZE)
                .ok()
        };
        request
            .buffer_size
            .and_then(|size| {
                configs
                    .iter()
                    .copied()
                    .find(|&c| buffer_size(c) == Some(size))
            })
            .or_else(|| configs.first().copied())
    } else {
        request
            .native_visual_ids
            .iter()
            .find_map(|&id| configs.iter().copied().find(|&c| visual_id(c) == id))
    };
    let config = chosen.ok_or_else(|| {
        let available: Vec<String> = configs.iter().map(|&c| fourcc_name(visual_id(c))).collect();
        Error::Egl(format!(
            "no EGL config matches the native formats {:?} (available: {available:?})",
            request
                .native_visual_ids
                .iter()
                .map(|&id| fourcc_name(id))
                .collect::<Vec<_>>()
        ))
    })?;

    let attr = |a| egl.get_config_attrib(display, config, a).unwrap_or(-1);
    log::debug!(
        "EGL config: R{}G{}B{}A{} (buffer {}) depth {} stencil {} samples {} visual {}",
        attr(egl::RED_SIZE),
        attr(egl::GREEN_SIZE),
        attr(egl::BLUE_SIZE),
        attr(egl::ALPHA_SIZE),
        attr(egl::BUFFER_SIZE),
        attr(egl::DEPTH_SIZE),
        attr(egl::STENCIL_SIZE),
        attr(egl::SAMPLES),
        fourcc_name(visual_id(config)),
    );
    Ok((config, visual_id(config)))
}

/// Renders a DRM fourcc like `XR24` for logs; other visual ids as hex.
pub(crate) fn fourcc_name(id: u32) -> String {
    let bytes = id.to_le_bytes();
    if bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        format!("{id:#x}")
    }
}
