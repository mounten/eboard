//! Linux framebuffer platform (`/dev/fbN`), the equivalent of Qt's `linuxfb`.
//!
//! egui is rasterized on the CPU ([`SoftRenderer`]). Each frame, only the pixels that changed are
//! converted (pixel format, rotation) and copied into the memory-mapped framebuffer. Works on any
//! fbdev driver with packed truecolor pixels (16/24/32 bpp), e.g. the i.MX6 `mxsfb`/`mxc_ipuv3_fb`
//! BSP drivers or DRM's fbdev emulation.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd as _, BorrowedFd};
use std::path::PathBuf;
use std::time::Duration;

use super::{Platform, Started, vt};
use crate::render::soft::SoftRenderer;
use crate::render::soft::blit::{self, Channel, PixelFormat, PixelRect, Target};
use crate::{Error, Options, Rotation};

const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;
const FBIOGET_FSCREENINFO: libc::c_ulong = 0x4602;
const FBIOBLANK: libc::c_ulong = 0x4611;
/// `_IOW('F', 0x20, __u32)`
const FBIO_WAITFORVSYNC: libc::c_ulong = 0x4004_4620;

const FB_TYPE_PACKED_PIXELS: u32 = 0;
const FB_VISUAL_TRUECOLOR: u32 = 2;
const FB_VISUAL_DIRECTCOLOR: u32 = 4;
const FB_BLANK_UNBLANK: libc::c_int = 0;

/// `struct fb_bitfield` from `<linux/fb.h>`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct FbBitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

/// `struct fb_var_screeninfo` from `<linux/fb.h>`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct FbVarScreeninfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: FbBitfield,
    green: FbBitfield,
    blue: FbBitfield,
    transp: FbBitfield,
    nonstd: u32,
    activate: u32,
    /// Physical height in mm.
    height: u32,
    /// Physical width in mm.
    width: u32,
    accel_flags: u32,
    /// Pixel clock in picoseconds.
    pixclock: u32,
    left_margin: u32,
    right_margin: u32,
    upper_margin: u32,
    lower_margin: u32,
    hsync_len: u32,
    vsync_len: u32,
    sync: u32,
    vmode: u32,
    rotate: u32,
    colorspace: u32,
    reserved: [u32; 4],
}

const _: () = assert!(size_of::<FbVarScreeninfo>() == 160);

/// `struct fb_fix_screeninfo` from `<linux/fb.h>`.
#[repr(C)]
#[derive(Clone, Copy)]
struct FbFixScreeninfo {
    id: [u8; 16],
    smem_start: libc::c_ulong,
    smem_len: u32,
    type_: u32,
    type_aux: u32,
    visual: u32,
    xpanstep: u16,
    ypanstep: u16,
    ywrapstep: u16,
    line_length: u32,
    mmio_start: libc::c_ulong,
    mmio_len: u32,
    accel: u32,
    capabilities: u16,
    reserved: [u16; 2],
}

/// Opens the framebuffer and creates the software renderer.
pub(crate) fn open(options: &Options) -> Result<Started<LinuxFbPlatform>, Error> {
    let platform = LinuxFbPlatform::new(options)?;
    Ok(Started {
        platform,
        renderer: SoftRenderer::new(),
        renderer_info: "software rasterizer (CPU)".to_owned(),
        #[cfg(feature = "glow")]
        gl: None,
    })
}

pub(crate) struct LinuxFbPlatform {
    file: File,
    path: PathBuf,
    id: String,
    memory: *mut u8,
    memory_len: usize,
    /// Byte offset of the visible page (panned framebuffers).
    visible_offset: usize,
    stride: usize,
    format: PixelFormat,
    panel_size: [usize; 2],
    rotation: Rotation,
    refresh_hz: f32,
    physical_size_mm: Option<[u32; 2]>,
    vsync: bool,
    /// Last frame copied to the framebuffer (logical canvas), for damage detection.
    previous: Vec<[u8; 4]>,
    row: Vec<u8>,
    _vt: Option<vt::VtGuard>,
}

fn fb_err(path: &std::path::Path, what: &str) -> Error {
    Error::Framebuffer(format!("{}: {what}", path.display()))
}

impl LinuxFbPlatform {
    fn new(options: &Options) -> Result<Self, Error> {
        let path = options
            .output
            .fb_device
            .clone()
            .unwrap_or_else(|| PathBuf::from("/dev/fb0"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| {
                let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
                    " (add the user to the `video` group)"
                } else {
                    ""
                };
                fb_err(&path, &format!("{e}{hint}"))
            })?;
        let fd = file.as_raw_fd();

        // SAFETY: the ioctls fill the structs, which match the kernel ABI.
        let mut var = FbVarScreeninfo::default();
        if unsafe { libc::ioctl(fd, FBIOGET_VSCREENINFO as _, &mut var) } != 0 {
            let e = std::io::Error::last_os_error();
            return Err(fb_err(&path, &format!("FBIOGET_VSCREENINFO: {e}")));
        }
        let mut fix: FbFixScreeninfo = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(fd, FBIOGET_FSCREENINFO as _, &mut fix) } != 0 {
            let e = std::io::Error::last_os_error();
            return Err(fb_err(&path, &format!("FBIOGET_FSCREENINFO: {e}")));
        }
        let id = String::from_utf8_lossy(fix.id.split(|&b| b == 0).next().unwrap_or_default())
            .into_owned();
        log::debug!("{}: {id}, {var:?}", path.display());

        if fix.type_ != FB_TYPE_PACKED_PIXELS
            || !matches!(fix.visual, FB_VISUAL_TRUECOLOR | FB_VISUAL_DIRECTCOLOR)
        {
            return Err(fb_err(
                &path,
                &format!(
                    "type {} / visual {} is not supported (packed truecolor only)",
                    fix.type_, fix.visual
                ),
            ));
        }
        if var.bits_per_pixel % 8 != 0 {
            return Err(fb_err(
                &path,
                &format!("{} bpp is not supported", var.bits_per_pixel),
            ));
        }
        let channel = |b: FbBitfield| Channel {
            offset: b.offset,
            length: b.length,
        };
        let format = PixelFormat {
            bytes_per_pixel: var.bits_per_pixel as usize / 8,
            red: channel(var.red),
            green: channel(var.green),
            blue: channel(var.blue),
            alpha: (var.transp.length > 0).then(|| channel(var.transp)),
        };
        format.validate().map_err(|e| fb_err(&path, &e))?;

        let bpp = format.bytes_per_pixel;
        let panel_size = [var.xres as usize, var.yres as usize];
        let stride = match fix.line_length as usize {
            0 => var.xres_virtual as usize * bpp,
            n => n,
        };
        let memory_len = match fix.smem_len as usize {
            0 => stride * var.yres_virtual as usize,
            n => n,
        };
        let visible_offset = var.yoffset as usize * stride + var.xoffset as usize * bpp;
        let needed =
            visible_offset + panel_size[1].saturating_sub(1) * stride + panel_size[0] * bpp;
        if panel_size.contains(&0) || needed > memory_len {
            return Err(fb_err(
                &path,
                &format!("{panel_size:?} px at stride {stride} does not fit {memory_len} bytes"),
            ));
        }

        // SAFETY: maps the framebuffer; unmapped in Drop.
        let memory = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                memory_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if memory == libc::MAP_FAILED {
            let e = std::io::Error::last_os_error();
            return Err(fb_err(&path, &format!("mmap: {e}")));
        }

        // Wake the display if it was blanked by the console's power saving.
        unsafe { libc::ioctl(fd, FBIOBLANK as _, FB_BLANK_UNBLANK) };

        let vsync = options.vsync && wait_for_vsync(fd).is_ok();
        let refresh_hz = refresh_hz(&var);
        let physical_size_mm = options.output.physical_size_mm.or_else(|| {
            let plausible = |mm: u32| (1..10_000).contains(&mm);
            (plausible(var.width) && plausible(var.height)).then_some([var.width, var.height])
        });
        log::info!(
            "linuxfb: {} ({id}), {}x{} {} ({} bpp, stride {stride}), {refresh_hz:.1} Hz, vsync {}, \
             physical size {}",
            path.display(),
            panel_size[0],
            panel_size[1],
            format.name(),
            var.bits_per_pixel,
            if vsync { "on" } else { "off" },
            physical_size_mm.map_or("unknown".to_owned(), |[w, h]| format!("{w}x{h} mm")),
        );

        let vt = vt::enter_graphics_mode();
        Ok(Self {
            file,
            path,
            id,
            memory: memory.cast(),
            memory_len,
            visible_offset,
            stride,
            format,
            panel_size,
            rotation: options.output.rotation,
            refresh_hz,
            physical_size_mm,
            vsync,
            previous: Vec::new(),
            row: Vec::new(),
            _vt: vt,
        })
    }
}

fn wait_for_vsync(fd: libc::c_int) -> std::io::Result<()> {
    let crtc: u32 = 0;
    // SAFETY: FBIO_WAITFORVSYNC reads one u32.
    if unsafe { libc::ioctl(fd, FBIO_WAITFORVSYNC as _, &crtc) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Refresh rate from the mode timings; 60 Hz if the driver does not report them (DRM fbdev
/// emulation reports a zero pixel clock).
fn refresh_hz(var: &FbVarScreeninfo) -> f32 {
    let htotal = var.xres + var.left_margin + var.right_margin + var.hsync_len;
    let vtotal = var.yres + var.upper_margin + var.lower_margin + var.vsync_len;
    if var.pixclock == 0 || htotal == 0 || vtotal == 0 {
        return 60.0;
    }
    let hz = 1e12 / (f64::from(var.pixclock) * f64::from(htotal) * f64::from(vtotal));
    if (10.0..=240.0).contains(&hz) {
        hz as f32
    } else {
        60.0
    }
}

impl Platform for LinuxFbPlatform {
    type Renderer = SoftRenderer;

    fn name(&self) -> &'static str {
        "linuxfb"
    }

    fn output_name(&self) -> String {
        format!("{} {}", self.id, self.path.display())
    }

    fn panel_size(&self) -> [u32; 2] {
        self.panel_size.map(|v| v as u32)
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

    fn present(&mut self, renderer: &mut SoftRenderer) -> Result<(), Error> {
        let canvas = renderer.canvas();
        if self.rotation.logical_size(self.panel_size) != [canvas.width, canvas.height] {
            return Err(Error::Framebuffer(format!(
                "canvas {}x{} does not match the screen",
                canvas.width, canvas.height
            )));
        }

        let rect = if self.previous.len() != canvas.pixels.len() {
            self.previous = canvas.pixels.clone();
            PixelRect::full([canvas.width, canvas.height])
        } else {
            match blit::damage(&mut self.previous, &canvas.pixels, canvas.width) {
                Some(rect) => rect,
                None => return Ok(()), // nothing changed on screen
            }
        };
        let panel_rect = blit::logical_to_panel_rect(rect, self.rotation, self.panel_size);

        if self.vsync
            && let Err(e) = wait_for_vsync(self.file.as_raw_fd())
        {
            log::debug!("FBIO_WAITFORVSYNC failed ({e}), disabling vsync");
            self.vsync = false;
        }

        // SAFETY: the mapping is valid until Drop, and `new` checked that the visible page fits.
        let memory = unsafe {
            std::slice::from_raw_parts_mut(
                self.memory.add(self.visible_offset),
                self.memory_len - self.visible_offset,
            )
        };
        let mut target = Target {
            memory,
            stride: self.stride,
            format: self.format,
            size: self.panel_size,
        };
        blit::blit(
            &canvas.pixels,
            canvas.width,
            self.rotation,
            panel_rect,
            &mut target,
            &mut self.row,
        );
        log::trace!(
            "Copied {}x{} px to the framebuffer",
            panel_rect.x1 - panel_rect.x0,
            panel_rect.y1 - panel_rect.y0
        );
        Ok(())
    }

    fn frame_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs_f32(1.0 / self.refresh_hz))
    }

    fn event_fd(&self) -> Option<BorrowedFd<'_>> {
        None
    }

    fn dispatch_events(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

impl Drop for LinuxFbPlatform {
    fn drop(&mut self) {
        // SAFETY: mapped in `new` with this length.
        unsafe { libc::munmap(self.memory.cast(), self.memory_len) };
    }
}
