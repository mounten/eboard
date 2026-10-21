//! The few GBM functions the `kms` platform needs, loaded from libgbm at runtime.
//!
//! Nothing is linked at build time, so a binary with `kms` still starts on boards without libgbm
//! (i.MX6 Vivante fbdev BSPs): [`GbmApi::load`] fails, and `Backend::Auto` moves on to the next
//! backend.
//!
//! The wrappers keep what they were made from alive: a [`Surface`] holds its device, a
//! [`LockedBo`] its surface. The DRM device fd passed to [`Device::new`] has to outlive them all.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::io;
use std::num::NonZeroU32;
use std::os::fd::{AsRawFd as _, BorrowedFd};
use std::rc::Rc;

use drm::buffer::{self, DrmFourcc, DrmModifier};

use crate::Error;

const LIBGBM: &str = "libgbm.so.1";
/// Development symlink, for systems that only have that.
const LIBGBM_DEV: &str = "libgbm.so";

/// `GBM_BO_USE_SCANOUT`: the buffer can be shown by the display controller.
pub(crate) const GBM_BO_USE_SCANOUT: u32 = 1 << 0;
/// `GBM_BO_USE_RENDERING`: the GPU can render into the buffer.
pub(crate) const GBM_BO_USE_RENDERING: u32 = 1 << 2;

/// `union gbm_bo_handle`, returned by value.
#[repr(C)]
#[derive(Clone, Copy)]
union BoHandle {
    _ptr: *mut c_void,
    u32_: u32,
    _u64: u64,
}

// Signatures from Mesa's `gbm.h`. Devices, surfaces and buffer objects are opaque pointers.
type CreateDevice = unsafe extern "C" fn(c_int) -> *mut c_void;
type DeviceDestroy = unsafe extern "C" fn(*mut c_void);
type DeviceGetBackendName = unsafe extern "C" fn(*mut c_void) -> *const c_char;
type SurfaceCreate = unsafe extern "C" fn(*mut c_void, u32, u32, u32, u32) -> *mut c_void;
type SurfaceCreateWithModifiers =
    unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const u64, c_uint) -> *mut c_void;
type SurfaceDestroy = unsafe extern "C" fn(*mut c_void);
type SurfaceLockFrontBuffer = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type SurfaceReleaseBuffer = unsafe extern "C" fn(*mut c_void, *mut c_void);
type BoGetU32 = unsafe extern "C" fn(*mut c_void) -> u32;
type BoGetHandle = unsafe extern "C" fn(*mut c_void) -> BoHandle;
type BoGetModifier = unsafe extern "C" fn(*mut c_void) -> u64;
type BoGetPlaneCount = unsafe extern "C" fn(*mut c_void) -> c_int;
type BoGetHandleForPlane = unsafe extern "C" fn(*mut c_void, c_int) -> BoHandle;
type BoGetU32ForPlane = unsafe extern "C" fn(*mut c_void, c_int) -> u32;

/// The libgbm functions we use.
pub(crate) struct GbmApi {
    create_device: CreateDevice,
    device_destroy: DeviceDestroy,
    device_get_backend_name: DeviceGetBackendName,
    surface_create: SurfaceCreate,
    /// Missing in old libgbm builds.
    surface_create_with_modifiers: Option<SurfaceCreateWithModifiers>,
    surface_destroy: SurfaceDestroy,
    surface_lock_front_buffer: SurfaceLockFrontBuffer,
    surface_release_buffer: SurfaceReleaseBuffer,
    bo_get_width: BoGetU32,
    bo_get_height: BoGetU32,
    bo_get_stride: BoGetU32,
    bo_get_format: BoGetU32,
    bo_get_handle: BoGetHandle,
    /// Missing in old libgbm builds; without it, framebuffers are added without modifiers.
    planes: Option<PlaneApi>,
    /// Keeps the function pointers valid.
    _library: libloading::Library,
}

/// Modifier and per-plane queries, all or nothing.
struct PlaneApi {
    get_modifier: BoGetModifier,
    get_plane_count: BoGetPlaneCount,
    get_handle_for_plane: BoGetHandleForPlane,
    get_stride_for_plane: BoGetU32ForPlane,
    get_offset: BoGetU32ForPlane,
}

impl GbmApi {
    pub(crate) fn load() -> Result<Self, Error> {
        // SAFETY: loading libgbm runs its initializers, as linking it would.
        let library = unsafe { libloading::Library::new(LIBGBM) }
            .or_else(|e| unsafe { libloading::Library::new(LIBGBM_DEV) }.map_err(|_| e))
            // The dlopen error names the library already.
            .map_err(|e| Error::Kms(format!("{e} (no GBM on this board)")))?;
        // SAFETY: the types match the declarations in `gbm.h`.
        unsafe {
            let planes = (|| {
                Some(PlaneApi {
                    get_modifier: optional(&library, "gbm_bo_get_modifier")?,
                    get_plane_count: optional(&library, "gbm_bo_get_plane_count")?,
                    get_handle_for_plane: optional(&library, "gbm_bo_get_handle_for_plane")?,
                    get_stride_for_plane: optional(&library, "gbm_bo_get_stride_for_plane")?,
                    get_offset: optional(&library, "gbm_bo_get_offset")?,
                })
            })();
            Ok(Self {
                create_device: symbol(&library, "gbm_create_device")?,
                device_destroy: symbol(&library, "gbm_device_destroy")?,
                device_get_backend_name: symbol(&library, "gbm_device_get_backend_name")?,
                surface_create: symbol(&library, "gbm_surface_create")?,
                surface_create_with_modifiers: optional(
                    &library,
                    "gbm_surface_create_with_modifiers",
                ),
                surface_destroy: symbol(&library, "gbm_surface_destroy")?,
                surface_lock_front_buffer: symbol(&library, "gbm_surface_lock_front_buffer")?,
                surface_release_buffer: symbol(&library, "gbm_surface_release_buffer")?,
                bo_get_width: symbol(&library, "gbm_bo_get_width")?,
                bo_get_height: symbol(&library, "gbm_bo_get_height")?,
                bo_get_stride: symbol(&library, "gbm_bo_get_stride")?,
                bo_get_format: symbol(&library, "gbm_bo_get_format")?,
                bo_get_handle: symbol(&library, "gbm_bo_get_handle")?,
                planes,
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
        .map_err(|e| Error::Kms(format!("{LIBGBM}: {e}")))
}

/// # Safety
/// `T` must be the function pointer type of the symbol.
unsafe fn optional<T: Copy>(library: &libloading::Library, name: &str) -> Option<T> {
    let symbol = unsafe { library.get::<T>(name.as_bytes()) };
    if symbol.is_err() {
        log::debug!("{LIBGBM} has no {name}");
    }
    symbol.ok().map(|symbol| *symbol)
}

/// A `gbm_device`.
pub(crate) struct Device(Rc<DeviceInner>);

struct DeviceInner {
    raw: *mut c_void,
    api: GbmApi,
}

impl Drop for DeviceInner {
    fn drop(&mut self) {
        // SAFETY: `raw` came from `gbm_create_device`, and every surface (which holds an `Rc` on
        // us) is gone.
        unsafe { (self.api.device_destroy)(self.raw) };
    }
}

impl Device {
    /// `gbm_create_device` on an open DRM device.
    ///
    /// # Safety
    /// `fd` must stay open until the device and every surface and buffer made from it are
    /// dropped.
    pub(crate) unsafe fn new(api: GbmApi, fd: BorrowedFd<'_>) -> io::Result<Self> {
        // SAFETY: `fd` is a valid open file; the caller keeps it open long enough.
        let raw = unsafe { (api.create_device)(fd.as_raw_fd()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(Rc::new(DeviceInner { raw, api })))
    }

    /// The `gbm_device*`, the native display for EGL.
    pub(crate) fn as_raw(&self) -> *mut c_void {
        self.0.raw
    }

    pub(crate) fn backend_name(&self) -> String {
        // SAFETY: the device is valid; the name is a static C string (or null).
        let name = unsafe { (self.0.api.device_get_backend_name)(self.0.raw) };
        if name.is_null() {
            return "unknown".to_owned();
        }
        // SAFETY: non-null and NUL-terminated.
        unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned()
    }

    /// `gbm_surface_create`, with `GBM_BO_USE_*` flags.
    pub(crate) fn create_surface(
        &self,
        width: u32,
        height: u32,
        format: DrmFourcc,
        flags: u32,
    ) -> io::Result<Surface> {
        let device = &self.0;
        // SAFETY: the device is valid.
        let raw =
            unsafe { (device.api.surface_create)(device.raw, width, height, format as u32, flags) };
        self.surface(raw)
    }

    /// `gbm_surface_create_with_modifiers`.
    pub(crate) fn create_surface_with_modifiers(
        &self,
        width: u32,
        height: u32,
        format: DrmFourcc,
        modifiers: &[DrmModifier],
    ) -> io::Result<Surface> {
        let device = &self.0;
        let Some(create) = device.api.surface_create_with_modifiers else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{LIBGBM} has no gbm_surface_create_with_modifiers"),
            ));
        };
        let modifiers: Vec<u64> = modifiers.iter().map(|&m| m.into()).collect();
        // SAFETY: the device is valid, and the modifier array has `len` entries.
        let raw = unsafe {
            create(
                device.raw,
                width,
                height,
                format as u32,
                modifiers.as_ptr(),
                modifiers.len() as c_uint,
            )
        };
        self.surface(raw)
    }

    fn surface(&self, raw: *mut c_void) -> io::Result<Surface> {
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Surface(Rc::new(SurfaceInner {
            raw,
            device: self.0.clone(),
        })))
    }
}

/// A `gbm_surface`, the native window for EGL.
pub(crate) struct Surface(Rc<SurfaceInner>);

struct SurfaceInner {
    raw: *mut c_void,
    device: Rc<DeviceInner>,
}

impl Drop for SurfaceInner {
    fn drop(&mut self) {
        // SAFETY: `raw` came from `gbm_surface_create*`, and every locked buffer (which holds an
        // `Rc` on us) has been released. The EGL surface on it must be gone already.
        unsafe { (self.device.api.surface_destroy)(self.raw) };
    }
}

impl Surface {
    /// The `gbm_surface*`, the native window for EGL.
    pub(crate) fn as_raw(&self) -> *mut c_void {
        self.0.raw
    }

    /// `gbm_surface_lock_front_buffer`: the buffer that the last `eglSwapBuffers` finished. It
    /// goes back to the surface when the [`LockedBo`] is dropped. `None` if GBM fails or returns
    /// a buffer without a GEM handle.
    ///
    /// # Safety
    /// Must be called exactly once after each `eglSwapBuffers` on this surface.
    pub(crate) unsafe fn lock_front_buffer(&self) -> Option<LockedBo> {
        let api = &self.0.device.api;
        // SAFETY: the surface is valid; the caller just swapped buffers.
        let raw = unsafe { (api.surface_lock_front_buffer)(self.0.raw) };
        if raw.is_null() {
            return None;
        }
        // SAFETY: `raw` is a valid buffer object, and the union is initialized as a whole.
        let handle = unsafe { (api.bo_get_handle)(raw).u32_ };
        let Some(handle) = NonZeroU32::new(handle) else {
            // SAFETY: locked just above, released once.
            unsafe { (api.surface_release_buffer)(self.0.raw, raw) };
            return None;
        };
        Some(LockedBo {
            raw,
            handle: handle.into(),
            surface: self.0.clone(),
        })
    }
}

/// A buffer object locked by `gbm_surface_lock_front_buffer`, released to its surface on drop.
pub(crate) struct LockedBo {
    raw: *mut c_void,
    /// The GEM handle.
    handle: buffer::Handle,
    surface: Rc<SurfaceInner>,
}

impl Drop for LockedBo {
    fn drop(&mut self) {
        // SAFETY: `raw` was locked from this surface and is released once.
        unsafe { (self.surface.device.api.surface_release_buffer)(self.surface.raw, self.raw) };
    }
}

impl LockedBo {
    fn api(&self) -> &GbmApi {
        &self.surface.device.api
    }

    /// The `gbm_bo*`. Stable for the lifetime of the surface (it cycles a fixed set of buffers).
    pub(crate) fn as_raw(&self) -> *mut c_void {
        self.raw
    }

    fn get(&self, f: BoGetU32) -> u32 {
        // SAFETY: the buffer object is valid while locked.
        unsafe { f(self.raw) }
    }

    /// The buffer seen with its modifier and planes, for `drmModeAddFB2WithModifiers`. `None` if
    /// libgbm can't tell the modifier (missing functions or `DRM_FORMAT_MOD_INVALID`).
    pub(crate) fn planar(&self) -> Option<PlanarBo<'_>> {
        let planes = self.api().planes.as_ref()?;
        // SAFETY: the buffer object is valid while locked.
        let modifier = DrmModifier::from(unsafe { (planes.get_modifier)(self.raw) });
        if modifier == DrmModifier::Invalid {
            return None;
        }
        // SAFETY: as above.
        let count = unsafe { (planes.get_plane_count)(self.raw) }.clamp(1, 4);
        Some(PlanarBo {
            bo: self,
            planes,
            modifier,
            count,
        })
    }
}

impl buffer::Buffer for LockedBo {
    fn size(&self) -> (u32, u32) {
        (
            self.get(self.api().bo_get_width),
            self.get(self.api().bo_get_height),
        )
    }

    fn format(&self) -> DrmFourcc {
        DrmFourcc::try_from(self.get(self.api().bo_get_format))
            .expect("libgbm returned an unknown buffer format")
    }

    fn pitch(&self) -> u32 {
        self.get(self.api().bo_get_stride)
    }

    fn handle(&self) -> buffer::Handle {
        self.handle
    }
}

/// A [`LockedBo`] with its modifier and plane layout.
pub(crate) struct PlanarBo<'a> {
    bo: &'a LockedBo,
    planes: &'a PlaneApi,
    modifier: DrmModifier,
    count: c_int,
}

impl PlanarBo<'_> {
    /// `f(plane)` for each plane, `default` for the unused ones.
    fn per_plane<T: Copy>(&self, default: T, f: impl Fn(c_int) -> T) -> [T; 4] {
        let mut values = [default; 4];
        for (plane, value) in (0..self.count).zip(&mut values) {
            *value = f(plane);
        }
        values
    }
}

impl buffer::PlanarBuffer for PlanarBo<'_> {
    fn size(&self) -> (u32, u32) {
        buffer::Buffer::size(self.bo)
    }

    fn format(&self) -> DrmFourcc {
        buffer::Buffer::format(self.bo)
    }

    fn modifier(&self) -> Option<DrmModifier> {
        Some(self.modifier)
    }

    fn pitches(&self) -> [u32; 4] {
        // SAFETY: the buffer object is valid while locked, and `plane` is below its plane count.
        self.per_plane(0, |plane| unsafe {
            (self.planes.get_stride_for_plane)(self.bo.raw, plane)
        })
    }

    fn handles(&self) -> [Option<buffer::Handle>; 4] {
        self.per_plane(None, |plane| {
            // SAFETY: as in `pitches`; the union is initialized as a whole.
            let handle = unsafe { (self.planes.get_handle_for_plane)(self.bo.raw, plane).u32_ };
            NonZeroU32::new(handle).map(buffer::Handle::from)
        })
    }

    fn offsets(&self) -> [u32; 4] {
        // SAFETY: as in `pitches`.
        self.per_plane(0, |plane| unsafe {
            (self.planes.get_offset)(self.bo.raw, plane)
        })
    }
}
