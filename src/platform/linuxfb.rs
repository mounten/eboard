//! Linux framebuffer platform (`/dev/fbN`), the equivalent of Qt's `linuxfb`.
//!
//! egui is rasterized on the CPU ([`SoftRenderer`]). Each frame, only the pixels that changed are
//! converted (pixel format, rotation) and copied into the memory-mapped framebuffer. Works on any
//! fbdev driver with packed truecolor pixels (16/24/32 bpp), e.g. the i.MX6 `mxsfb`/`mxc_ipuv3_fb`
//! BSP drivers or DRM's fbdev emulation.

use std::fs::File;
use std::os::fd::{AsRawFd as _, BorrowedFd};
use std::path::PathBuf;
use std::time::Duration;

use super::fbdev::{self, FbBitfield, fb_err};
use super::{Platform, Started, vt};
use crate::render::soft::SoftRenderer;
use crate::render::soft::blit::{self, Channel, PixelFormat, PixelRect, Target};
use crate::{Error, Options, Rotation};

/// `_IOW('F', 0x20, __u32)`
const FBIO_WAITFORVSYNC: libc::c_ulong = 0x4004_4620;

const FB_TYPE_PACKED_PIXELS: u32 = 0;
const FB_VISUAL_TRUECOLOR: u32 = 2;
const FB_VISUAL_DIRECTCOLOR: u32 = 4;

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
    blanked: bool,
    /// Last frame copied to the framebuffer (logical canvas), for damage detection.
    previous: Vec<[u8; 4]>,
    row: Vec<u8>,
    _vt: Option<vt::VtGuard>,
}

impl LinuxFbPlatform {
    fn new(options: &Options) -> Result<Self, Error> {
        let path = options
            .output
            .fb_device
            .clone()
            .unwrap_or_else(|| PathBuf::from("/dev/fb0"));
        let (file, var, fix, id) = fbdev::open_and_query(&path)?;
        let fd = file.as_raw_fd();

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

        fbdev::unblank(fd);

        let vsync = options.vsync && wait_for_vsync(fd).is_ok();
        let refresh_hz = fbdev::refresh_hz(&var);
        let physical_size_mm = options
            .output
            .physical_size_mm
            .or_else(|| fbdev::physical_size_mm(&var));
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
            blanked: false,
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

impl Platform for LinuxFbPlatform {
    type Renderer = SoftRenderer;

    const CLEAR_ON_EXIT: bool = true;

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

    fn set_blanked(&mut self, blanked: bool) -> Result<(), Error> {
        fbdev::blank(self.file.as_raw_fd(), blanked)
            .map_err(|e| fb_err(&self.path, &format!("FBIOBLANK: {e}")))?;
        self.blanked = blanked;
        if !blanked {
            // The console may have drawn over the framebuffer when it unblanked: copy everything
            // on the next present.
            self.previous.clear();
        }
        Ok(())
    }
}

impl Drop for LinuxFbPlatform {
    fn drop(&mut self) {
        if self.blanked {
            fbdev::unblank(self.file.as_raw_fd());
        }
        // SAFETY: mapped in `new` with this length.
        unsafe { libc::munmap(self.memory.cast(), self.memory_len) };
    }
}
