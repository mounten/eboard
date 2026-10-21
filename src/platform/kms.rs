//! DRM/KMS + GBM + EGL platform, the equivalent of Qt's `eglfs_kms`.
//!
//! Works with Mesa (v3d/vc4, etnaviv, panfrost, freedreno, Intel, AMD), NVIDIA Jetson (NVGBM,
//! JetPack 5.1.2+) and other drivers that implement `EGL_KHR_platform_gbm`.
//!
//! Frame flow: `eglSwapBuffers` → `gbm_surface_lock_front_buffer` → DRM framebuffer (cached per
//! buffer object) → page flip. The previous buffer goes back to GBM when the flip event arrives.
//!
//! libgbm is loaded at runtime (see [`super::gbm`]): without it, this platform fails to open and
//! `Backend::Auto` moves on.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use drm::buffer::{Buffer as _, DrmFourcc, DrmModifier};
use drm::control::{
    self, Device as _, FbCmd2Flags, Mode, ModeTypeFlags, PageFlipFlags, ResourceHandles, connector,
    crtc, framebuffer, plane, property,
};
use drm::{ClientCapability, Device as _, DriverCapability};
use egui_glow::glow::{self, HasContext as _};

use super::egl::{ConfigRequest, Egl, NativeDisplay, fourcc_name};
use super::gbm::{self, GBM_BO_USE_RENDERING, GBM_BO_USE_SCANOUT, GbmApi, LockedBo};
use super::{Platform, Started};
use crate::options::{ModeCandidate, OutputConfig, select_mode};
use crate::render::glow::GlowRenderer;
use crate::{Error, Options, Rotation};

/// `EGL_PLATFORM_GBM_KHR` (same value as `EGL_PLATFORM_GBM_MESA`).
const EGL_PLATFORM_GBM_KHR: u32 = 0x31D7;

/// Values of the plane `rotation` bitmask property (counter-clockwise in DRM terms; 180° is the
/// same either way, and it is the only one we let the hardware do).
const DRM_MODE_ROTATE_0: u64 = 1 << 0;
const DRM_MODE_ROTATE_180: u64 = 1 << 2;

/// Value of the plane `type` enum property for primary planes.
const DRM_PLANE_TYPE_PRIMARY: u64 = 1;

/// Values of the connector `DPMS` enum property.
const DRM_MODE_DPMS_ON: u64 = 0;
const DRM_MODE_DPMS_OFF: u64 = 3;

const MASTER_HINT: &str = "another process is DRM master. Stop the desktop / display manager \
     (e.g. `sudo systemctl isolate multi-user.target`) and run from a console or over SSH";

/// An open `/dev/dri/cardN`.
struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for Card {}
impl control::Device for Card {}

type Fb = framebuffer::Handle;

/// The chosen connector, mode and CRTC.
struct Output {
    path: PathBuf,
    driver: String,
    connector: connector::Handle,
    connector_name: String,
    mode: Mode,
    crtc: crtc::Handle,
    size_mm: Option<[u32; 2]>,
}

struct PlaneRotation {
    plane: plane::Handle,
    property: property::Handle,
    original: u64,
    changed: bool,
}

/// Brings up the display and the GLES renderer.
pub(crate) fn open(options: &Options) -> Result<Started<KmsPlatform>, Error> {
    let platform = KmsPlatform::new(options)?;
    let gl = platform.gl();
    super::glow_started(platform, gl, options)
}

pub(crate) struct KmsPlatform {
    // Teardown order is handled in `Drop`: buffers, framebuffers, EGL, GBM surface, GBM device.
    // The fields are declared in that order too, and the card (whose fd GBM uses) comes last.
    egl: Option<Egl>,
    gl: Option<Arc<glow::Context>>,
    /// Buffer on screen.
    front: Option<LockedBo>,
    /// Buffer whose page flip is queued.
    pending: Option<LockedBo>,
    /// DRM framebuffer per GBM buffer object. A GBM surface cycles through a fixed set of
    /// buffers that live as long as the surface, so the pointer identifies the buffer.
    framebuffers: Vec<(*mut c_void, Fb)>,
    surface: Option<gbm::Surface>,
    gbm: gbm::Device,
    card: Card,

    output: Output,
    saved_crtc: crtc::Info,
    modeset_done: bool,
    plane_rotation: Option<PlaneRotation>,
    /// The connector's `DPMS` property, for idle blanking.
    dpms: Option<property::Handle>,
    blanked: bool,
    hw_rotation: Rotation,
    physical_size_mm: Option<[u32; 2]>,
    refresh_hz: f32,
    flip_flags: PageFlipFlags,
    addfb2_modifiers: bool,
}

impl KmsPlatform {
    pub(crate) fn new(options: &Options) -> Result<Self, Error> {
        // First, so that `Auto` moves on before touching a card on boards without GBM.
        let gbm_api = GbmApi::load()?;
        let (card, output) = open_output(&options.output)?;

        if let Err(e) = card.acquire_master_lock() {
            return Err(Error::Kms(format!(
                "{}: cannot become DRM master ({e}): {MASTER_HINT}",
                output.path.display()
            )));
        }
        if let Err(e) = card.set_client_capability(ClientCapability::UniversalPlanes, true) {
            log::debug!("DRM_CLIENT_CAP_UNIVERSAL_PLANES: {e}");
        }

        let saved_crtc = card
            .get_crtc(output.crtc)
            .map_err(|e| Error::Kms(format!("reading CRTC state: {e}")))?;
        let cap = |c| card.get_driver_capability(c).unwrap_or(0) != 0;
        let addfb2_modifiers = cap(DriverCapability::AddFB2Modifiers);
        let mut flip_flags = PageFlipFlags::EVENT;
        if !options.vsync {
            if cap(DriverCapability::ASyncPageFlip) {
                flip_flags |= PageFlipFlags::ASYNC;
            } else {
                log::info!("vsync off requested, but the driver has no async page flips");
            }
        }
        let plane_rotation = find_primary_plane(&card, output.crtc).and_then(|plane| {
            let (property, original) = find_property(&card, plane, "rotation")?;
            Some(PlaneRotation {
                plane,
                property,
                original,
                changed: false,
            })
        });

        let dpms = find_property(&card, output.connector, "DPMS").map(|(prop, _)| prop);
        let refresh_hz = mode_refresh_hz(&output.mode);
        let (w, h) = output.mode.size();
        let physical_size_mm = options.output.physical_size_mm.or(output.size_mm);
        log::info!(
            "KMS: {} ({}), connector {}, mode {w}x{h}@{refresh_hz:.2}, physical size {}",
            output.path.display(),
            output.driver,
            output.connector_name,
            physical_size_mm.map_or("unknown".to_owned(), |[w, h]| format!("{w}x{h} mm")),
        );

        // SAFETY: `card` is stored after `gbm` in `Self`, so its fd is closed only after the GBM
        // device and everything made from it are gone.
        let gbm = unsafe { gbm::Device::new(gbm_api, card.as_fd()) }
            .map_err(|e| Error::Kms(format!("gbm_create_device: {e}")))?;
        log::info!("GBM backend: {}", gbm.backend_name());

        let mut this = Self {
            egl: None,
            gl: None,
            front: None,
            pending: None,
            framebuffers: Vec::new(),
            surface: None,
            gbm,
            card,
            output,
            saved_crtc,
            modeset_done: false,
            plane_rotation,
            dpms,
            blanked: false,
            hw_rotation: Rotation::Deg0,
            physical_size_mm,
            refresh_hz,
            flip_flags,
            addfb2_modifiers,
        };
        this.init_egl(options, [u32::from(w), u32::from(h)])?;
        this.hw_rotation = this.apply_plane_rotation(options.output.rotation);
        this.first_modeset()?;
        Ok(this)
    }

    /// The GLES context, current on the scanout surface.
    pub(crate) fn gl(&self) -> Arc<glow::Context> {
        self.gl
            .clone()
            .expect("GL context is created in KmsPlatform::new")
    }

    fn init_egl(&mut self, options: &Options, [w, h]: [u32; 2]) -> Result<(), Error> {
        let formats = [DrmFourcc::Xrgb8888 as u32, DrmFourcc::Argb8888 as u32];
        let native = self.gbm.as_raw();
        // SAFETY: the gbm device outlives `Egl` (dropped first in `Drop`).
        let egl = unsafe {
            Egl::new(
                NativeDisplay::Platform {
                    platform: EGL_PLATFORM_GBM_KHR,
                    native,
                },
                &ConfigRequest {
                    native_visual_ids: &formats,
                    samples: options.multisampling,
                    buffer_size: None,
                },
            )?
        };
        let format = DrmFourcc::try_from(egl.native_visual_id).map_err(|_| {
            Error::Egl(format!(
                "EGL config has unknown native visual {:#x}",
                egl.native_visual_id
            ))
        })?;
        let egl = self.egl.insert(egl);

        let surface = self
            .gbm
            .create_surface(w, h, format, GBM_BO_USE_SCANOUT | GBM_BO_USE_RENDERING)
            .or_else(|e| {
                log::debug!("gbm_surface_create failed ({e}), retrying with a linear modifier");
                self.gbm
                    .create_surface_with_modifiers(w, h, format, &[DrmModifier::Linear])
            })
            .map_err(|e| {
                Error::Kms(format!(
                    "creating {w}x{h} {} GBM surface: {e}",
                    fourcc_name(format as u32)
                ))
            })?;
        let surface = self.surface.insert(surface);

        // SAFETY: the gbm surface outlives the EGL surface (EGL is dropped first in `Drop`).
        unsafe { egl.create_window_surface(surface.as_raw())? };
        log::debug!(
            "GBM surface {w}x{h} {}, GLES {}",
            fourcc_name(format as u32),
            egl.gles_major
        );

        self.gl = Some(Arc::new(egl.create_glow_context()));
        Ok(())
    }

    /// Lets the primary plane do a 180° rotation if it can. Returns the rotation it applies.
    fn apply_plane_rotation(&mut self, requested: Rotation) -> Rotation {
        let Some(pr) = &mut self.plane_rotation else {
            return Rotation::Deg0;
        };
        let (value, rotation) = if requested == Rotation::Deg180 {
            (DRM_MODE_ROTATE_180, Rotation::Deg180)
        } else {
            (DRM_MODE_ROTATE_0, Rotation::Deg0)
        };
        if pr.original == value {
            return rotation;
        }
        match self.card.set_property(pr.plane, pr.property, value) {
            Ok(()) => {
                pr.changed = true;
                if rotation != Rotation::Deg0 {
                    log::info!("Rotating 180° with the display controller (plane rotation)");
                }
                rotation
            }
            Err(e) => {
                log::debug!("Setting plane rotation {value:#x} failed: {e}");
                Rotation::Deg0
            }
        }
    }

    /// Shows a black frame and sets the mode.
    fn first_modeset(&mut self) -> Result<(), Error> {
        let gl = self.gl();
        unsafe {
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }
        let (bo, fb) = self.swap_and_lock()?;

        if let Err(e) = self.set_crtc(fb) {
            if self.hw_rotation == Rotation::Deg0 {
                return Err(e);
            }
            log::warn!("Modeset with plane rotation failed ({e}), rotating in software instead");
            self.apply_plane_rotation(Rotation::Deg0);
            self.hw_rotation = Rotation::Deg0;
            self.set_crtc(fb)?;
        }
        self.front = Some(bo);
        self.modeset_done = true;
        Ok(())
    }

    fn set_crtc(&self, fb: Fb) -> Result<(), Error> {
        let o = &self.output;
        self.card
            .set_crtc(o.crtc, Some(fb), (0, 0), &[o.connector], Some(o.mode))
            .map_err(|e| {
                if matches!(e.kind(), io::ErrorKind::PermissionDenied) {
                    Error::Kms(format!(
                        "setting mode on {}: {e}: {MASTER_HINT}",
                        o.connector_name
                    ))
                } else {
                    Error::Kms(format!("setting mode on {}: {e}", o.connector_name))
                }
            })
    }

    fn swap_and_lock(&mut self) -> Result<(LockedBo, Fb), Error> {
        self.egl
            .as_ref()
            .expect("EGL is initialized")
            .swap_buffers()?;
        let surface = self.surface.as_ref().expect("GBM surface is initialized");
        // SAFETY: called exactly once after eglSwapBuffers.
        let bo = unsafe { surface.lock_front_buffer() }
            .ok_or_else(|| Error::Kms("gbm_surface_lock_front_buffer failed".into()))?;
        let fb = self.framebuffer_for(&bo)?;
        Ok((bo, fb))
    }

    /// DRM framebuffer for a GBM buffer, created on first use and cached (GBM surfaces cycle
    /// through a fixed set of buffers).
    fn framebuffer_for(&mut self, bo: &LockedBo) -> Result<Fb, Error> {
        if let Some(&(_, fb)) = self
            .framebuffers
            .iter()
            .find(|(raw, _)| *raw == bo.as_raw())
        {
            return Ok(fb);
        }
        let with_modifier = match bo.planar() {
            Some(planar) if self.addfb2_modifiers => self
                .card
                .add_planar_framebuffer(&planar, FbCmd2Flags::MODIFIERS)
                .inspect_err(|e| log::debug!("drmModeAddFB2WithModifiers failed: {e}"))
                .ok(),
            _ => None,
        };
        let fb = match with_modifier {
            Some(fb) => fb,
            None => {
                let (depth, bpp) = match bo.format() {
                    DrmFourcc::Argb8888 => (32, 32),
                    _ => (24, 32),
                };
                self.card
                    .add_framebuffer(bo, depth, bpp)
                    .map_err(|e| Error::Kms(format!("drmModeAddFB: {e}")))?
            }
        };
        self.framebuffers.push((bo.as_raw(), fb));
        Ok(fb)
    }

    fn queue_flip(&mut self, fb: Fb) -> io::Result<()> {
        let crtc = self.output.crtc;
        match self.card.page_flip(crtc, fb, self.flip_flags, None) {
            Err(e) if self.flip_flags.contains(PageFlipFlags::ASYNC) => {
                log::info!("Async page flip failed ({e}), falling back to vsync");
                self.flip_flags.remove(PageFlipFlags::ASYNC);
                self.card.page_flip(crtc, fb, self.flip_flags, None)
            }
            result => result,
        }
    }

    fn wait_for_pending_flip(&mut self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while self.pending.is_some() && std::time::Instant::now() < deadline {
            let mut fds = [rustix::event::PollFd::new(
                &self.card,
                rustix::event::PollFlags::IN,
            )];
            let timeout = rustix::time::Timespec {
                tv_sec: 0,
                tv_nsec: 100_000_000,
            };
            match rustix::event::poll(&mut fds, Some(&timeout)) {
                Ok(n) if n > 0 => {
                    if let Err(e) = self.dispatch_events() {
                        log::debug!("{e}");
                        break;
                    }
                }
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => {
                    log::debug!("poll: {e}");
                    break;
                }
            }
        }
    }

    fn set_dpms(&self, value: u64) -> Result<(), Error> {
        let dpms = self
            .dpms
            .ok_or_else(|| Error::Kms(format!("{} has no DPMS property", self.output_name())))?;
        self.card
            .set_property(self.output.connector, dpms, value)
            .map_err(|e| Error::Kms(format!("setting DPMS: {e}")))
    }

    fn restore_crtc(&self) {
        let o = &self.output;
        let saved = &self.saved_crtc;
        let result = match (saved.framebuffer(), saved.mode()) {
            (Some(fb), Some(mode)) => self.card.set_crtc(
                o.crtc,
                Some(fb),
                saved.position(),
                &[o.connector],
                Some(mode),
            ),
            _ => self.card.set_crtc(o.crtc, None, (0, 0), &[], None),
        };
        if let Err(e) = result {
            log::warn!("Restoring the previous display configuration failed: {e}");
        }
    }
}

impl Platform for KmsPlatform {
    type Renderer = GlowRenderer;

    fn name(&self) -> &'static str {
        "kms"
    }

    fn output_name(&self) -> String {
        format!("{} {}", self.output.driver, self.output.connector_name)
    }

    fn panel_size(&self) -> [u32; 2] {
        let (w, h) = self.output.mode.size();
        [w.into(), h.into()]
    }

    fn physical_size_mm(&self) -> Option<[u32; 2]> {
        self.physical_size_mm
    }

    fn refresh_hz(&self) -> f32 {
        self.refresh_hz
    }

    fn hardware_rotation(&self) -> Rotation {
        self.hw_rotation
    }

    fn can_render(&self) -> bool {
        self.pending.is_none()
    }

    fn present(&mut self, _renderer: &mut GlowRenderer) -> Result<(), Error> {
        let (bo, fb) = self.swap_and_lock()?;
        self.queue_flip(fb)
            .map_err(|e| Error::Kms(format!("page flip: {e}")))?;
        self.pending = Some(bo);
        Ok(())
    }

    fn event_fd(&self) -> Option<BorrowedFd<'_>> {
        Some(self.card.as_fd())
    }

    fn dispatch_events(&mut self) -> Result<(), Error> {
        let events = self
            .card
            .receive_events()
            .map_err(|e| Error::Kms(format!("reading DRM events: {e}")))?;
        for event in events {
            if let control::Event::PageFlip(_) = event
                && let Some(bo) = self.pending.take()
            {
                // Dropping the old front buffer hands it back to the GBM surface.
                self.front = Some(bo);
            }
        }
        Ok(())
    }

    fn set_blanked(&mut self, blanked: bool) -> Result<(), Error> {
        if blanked {
            // A flip can't complete on a display that is off.
            self.wait_for_pending_flip();
        }
        self.set_dpms(if blanked {
            DRM_MODE_DPMS_OFF
        } else {
            DRM_MODE_DPMS_ON
        })?;
        self.blanked = blanked;
        Ok(())
    }
}

impl Drop for KmsPlatform {
    fn drop(&mut self) {
        self.wait_for_pending_flip();
        if self.blanked
            && let Err(e) = self.set_dpms(DRM_MODE_DPMS_ON)
        {
            log::warn!("{e}");
        }
        if self.modeset_done {
            self.restore_crtc();
        }
        if let Some(pr) = &self.plane_rotation
            && pr.changed
            && let Err(e) = self.card.set_property(pr.plane, pr.property, pr.original)
        {
            log::debug!("Restoring plane rotation failed: {e}");
        }
        self.pending = None;
        self.front = None;
        for (_, fb) in self.framebuffers.drain(..) {
            let _ = self.card.destroy_framebuffer(fb);
        }
        self.gl = None;
        // EGL surface before the GBM surface it wraps, GBM surface before the device.
        self.egl = None;
        self.surface = None;
        let _ = self.card.release_master_lock();
    }
}

/// Finds the device, connector, mode and CRTC to use.
fn open_output(config: &OutputConfig) -> Result<(Card, Output), Error> {
    let paths = match &config.device {
        Some(path) => vec![path.clone()],
        None => list_cards()?,
    };
    if paths.is_empty() {
        return Err(Error::Kms(
            "no /dev/dri/card* devices (is the KMS driver loaded?)".into(),
        ));
    }

    let mut checked = Vec::new();
    for path in paths {
        let card = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => Card(file),
            Err(e) => {
                let hint = if e.kind() == io::ErrorKind::PermissionDenied {
                    " (add the user to the `video` group)"
                } else {
                    ""
                };
                checked.push(format!("{}: {e}{hint}", path.display()));
                continue;
            }
        };
        let driver = card
            .get_driver()
            .map(|d| d.name().to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unknown".into());
        let resources = match card.resource_handles() {
            Ok(r) if !r.connectors().is_empty() => r,
            // Render-only GPUs (e.g. v3d on the Raspberry Pi) have no KMS resources.
            _ => {
                log::debug!(
                    "{} ({driver}): no display connectors, skipping",
                    path.display()
                );
                checked.push(format!("{} ({driver}): render-only", path.display()));
                continue;
            }
        };
        match find_output(&card, &resources, config)? {
            Ok(found) => {
                let output = Output {
                    path,
                    driver,
                    ..found
                };
                return Ok((card, output));
            }
            Err(connectors) => checked.push(format!(
                "{} ({driver}): {}",
                path.display(),
                connectors.join(", ")
            )),
        }
    }

    let what = match &config.connector {
        Some(name) => format!("connector {name:?}"),
        None => "a connected display".to_owned(),
    };
    Err(Error::Kms(format!(
        "could not find {what}. Checked: {}",
        checked.join("; ")
    )))
}

fn list_cards() -> Result<Vec<PathBuf>, Error> {
    let dir = Path::new("/dev/dri");
    let entries =
        std::fs::read_dir(dir).map_err(|e| Error::Kms(format!("{}: {e}", dir.display())))?;
    let mut cards: Vec<(u32, PathBuf)> = entries
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            let index = name.strip_prefix("card")?.parse().ok()?;
            Some((index, dir.join(name)))
        })
        .collect();
    cards.sort();
    Ok(cards.into_iter().map(|(_, path)| path).collect())
}

/// Returns the matching output on this device, or the list of connectors seen (for the error
/// message) if there is none. A requested connector that exists but cannot be used is an error.
fn find_output(
    card: &Card,
    resources: &ResourceHandles,
    config: &OutputConfig,
) -> Result<Result<Output, Vec<String>>, Error> {
    let mut seen = Vec::new();
    for &handle in resources.connectors() {
        // Force-probe like drmModeGetConnector, so the status is fresh at startup.
        let info = match card.get_connector(handle, true) {
            Ok(info) => info,
            Err(e) => {
                log::debug!("get_connector({handle:?}): {e}");
                continue;
            }
        };
        let name = format!("{}-{}", info.interface().as_str(), info.interface_id());
        let connected = info.state() == connector::State::Connected && !info.modes().is_empty();
        seen.push(format!(
            "{name} {}",
            if connected {
                "connected"
            } else {
                "disconnected"
            }
        ));

        if let Some(wanted) = &config.connector {
            if !wanted.eq_ignore_ascii_case(&name) {
                continue;
            }
            if !connected {
                return Err(Error::Kms(format!("connector {name} is not connected")));
            }
        } else if !connected {
            continue;
        }

        let modes = info.modes();
        let candidates: Vec<ModeCandidate> = modes.iter().map(mode_candidate).collect();
        let Some(index) = select_mode(&candidates, config.mode) else {
            let available: Vec<String> = candidates
                .iter()
                .map(|m| format!("{}x{}@{:.0}", m.width, m.height, m.refresh_hz))
                .collect();
            return Err(Error::Kms(format!(
                "{name} has no mode matching {:?}; available: {}",
                config.mode,
                available.join(", ")
            )));
        };
        let Some(crtc) = find_crtc(card, resources, &info) else {
            return Err(Error::Kms(format!("no CRTC available for {name}")));
        };
        return Ok(Ok(Output {
            path: PathBuf::new(),
            driver: String::new(),
            connector: handle,
            connector_name: name,
            mode: modes[index],
            crtc,
            size_mm: info.size().map(|(w, h)| [w, h]),
        }));
    }
    Ok(Err(seen))
}

fn find_crtc(
    card: &Card,
    resources: &ResourceHandles,
    info: &connector::Info,
) -> Option<crtc::Handle> {
    // Keep the CRTC that already drives this connector (e.g. from the boot console).
    if let Some(crtc) = info
        .current_encoder()
        .and_then(|enc| card.get_encoder(enc).ok())
        .and_then(|enc| enc.crtc())
    {
        return Some(crtc);
    }
    info.encoders()
        .iter()
        .filter_map(|&enc| card.get_encoder(enc).ok())
        .find_map(|enc| {
            resources
                .filter_crtcs(enc.possible_crtcs())
                .first()
                .copied()
        })
}

fn mode_candidate(mode: &Mode) -> ModeCandidate {
    let (width, height) = mode.size();
    ModeCandidate {
        width: width.into(),
        height: height.into(),
        refresh_hz: mode_refresh_hz(mode),
        preferred: mode.mode_type().contains(ModeTypeFlags::PREFERRED),
    }
}

/// Exact refresh rate from the pixel clock (e.g. 59.94 instead of the rounded 60).
fn mode_refresh_hz(mode: &Mode) -> f32 {
    let (_, _, htotal) = mode.hsync();
    let (_, _, vtotal) = mode.vsync();
    if htotal == 0 || vtotal == 0 {
        return mode.vrefresh() as f32;
    }
    (f64::from(mode.clock()) * 1000.0 / (f64::from(htotal) * f64::from(vtotal))) as f32
}

/// The primary plane feeding `crtc`, preferring the one currently attached to it.
fn find_primary_plane(card: &Card, crtc: crtc::Handle) -> Option<plane::Handle> {
    let resources = card.resource_handles().ok()?;
    let planes = card.plane_handles().ok()?;
    let mut fallback = None;
    for handle in planes {
        let Ok(info) = card.get_plane(handle) else {
            continue;
        };
        if !resources
            .filter_crtcs(info.possible_crtcs())
            .contains(&crtc)
        {
            continue;
        }
        let is_primary = find_property(card, handle, "type")
            .is_some_and(|(_, value)| value == DRM_PLANE_TYPE_PRIMARY);
        if !is_primary {
            continue;
        }
        if info.crtc() == Some(crtc) {
            return Some(handle);
        }
        fallback.get_or_insert(handle);
    }
    fallback
}

fn find_property<H: control::ResourceHandle>(
    card: &Card,
    handle: H,
    name: &str,
) -> Option<(property::Handle, property::RawValue)> {
    let props = card.get_properties(handle).ok()?;
    props.iter().find_map(|(&prop, &value)| {
        let info = card.get_property(prop).ok()?;
        (info.name().to_bytes() == name.as_bytes()).then_some((prop, value))
    })
}
