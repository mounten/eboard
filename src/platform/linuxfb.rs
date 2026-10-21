//! Linux framebuffer platform (`/dev/fbN`), the equivalent of Qt's `linuxfb`.
//!
//! egui is rasterized on the CPU ([`SoftRenderer`]). Each frame, only the pixels that changed are
//! converted (pixel format, rotation) and copied into the memory-mapped framebuffer. Works on any
//! fbdev driver with packed truecolor pixels (16/24/32 bpp), e.g. the i.MX6 `mxsfb`/`mxc_ipuv3_fb`
//! BSP drivers or DRM's fbdev emulation.
//!
//! With vsync on, frames are double buffered if the driver can pan and the framebuffer holds two
//! pages (raising the virtual height if the memory allows): a frame is drawn into the hidden page
//! and shown with `FBIOPAN_DISPLAY`. The hidden page is one frame behind, so it gets this frame's
//! changes plus the last one's.

use std::fs::File;
use std::os::fd::{AsRawFd as _, BorrowedFd};
use std::path::PathBuf;
use std::time::Duration;

use super::fbdev::{self, FbBitfield, FbFixScreeninfo, FbVarScreeninfo, fb_err};
use super::{Platform, Started, vt};
use crate::render::soft::SoftRenderer;
use crate::render::soft::blit::{self, Channel, Paging, PixelFormat, PixelRect, Target};
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
    /// Byte offset of the visible page without page flipping (panned framebuffers).
    visible_offset: usize,
    stride: usize,
    /// Screen info at startup, restored on drop.
    original_var: FbVarScreeninfo,
    /// The virtual height was raised for page flipping.
    resized: bool,
    flip: Option<Flip>,
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

/// Double buffering: pages at `yoffset` 0 and `yres`.
struct Flip {
    /// Screen info for panning; `yoffset` is the shown page's.
    var: FbVarScreeninfo,
    /// The page on screen (0 or 1).
    front: usize,
    /// Where the hidden page differs from the shown one (canvas coordinates).
    stale: Option<PixelRect>,
}

/// Bytes per line and size of the mapping.
fn geometry(var: &FbVarScreeninfo, fix: &FbFixScreeninfo, bpp: usize) -> (usize, usize) {
    let stride = match fix.line_length as usize {
        0 => var.xres_virtual as usize * bpp,
        n => n,
    };
    let memory_len = match fix.smem_len as usize {
        0 => stride * var.yres_virtual as usize,
        n => n,
    };
    (stride, memory_len)
}

/// Byte offset of the screen-sized area at `var.xoffset`, line `yoffset`.
fn area_offset(var: &FbVarScreeninfo, yoffset: u32, stride: usize, bpp: usize) -> usize {
    yoffset as usize * stride + var.xoffset as usize * bpp
}

/// Bytes from the first to the last visible pixel of a screen-sized area.
fn area_len(var: &FbVarScreeninfo, stride: usize, bpp: usize) -> usize {
    (var.yres as usize).saturating_sub(1) * stride + var.xres as usize * bpp
}

/// Sets up page flipping if the driver can pan and the framebuffer has, or can get, two pages.
/// Returns the screen info and fixed info to use, whether the virtual height was raised, and
/// the flip state (page 0 shown).
fn setup_flip(
    fd: libc::c_int,
    path: &std::path::Path,
    var: FbVarScreeninfo,
    fix: FbFixScreeninfo,
    bpp: usize,
) -> (FbVarScreeninfo, FbFixScreeninfo, bool, Option<Flip>) {
    let no_flip = |why: &str| log::debug!("{}: no page flipping: {why}", path.display());
    let (stride, memory_len) = geometry(&var, &fix, bpp);
    let paging = blit::paging(
        var.yres as usize,
        var.yres_virtual as usize,
        fix.ypanstep as usize,
        stride,
        memory_len,
    );
    let (new_var, new_fix, resized) = match paging {
        Paging::Single => {
            no_flip("one page, or the driver cannot pan");
            return (var, fix, false, None);
        }
        Paging::Flip => (var, fix, false),
        Paging::Resize => {
            let wanted = FbVarScreeninfo {
                yres_virtual: var.yres * 2,
                yoffset: 0,
                ..var
            };
            let resized = fbdev::put_var_screeninfo(fd, &wanted)
                .and_then(|new_var| Ok((new_var, fbdev::fix_screeninfo(fd)?)));
            match resized {
                Ok((new_var, new_fix)) => (new_var, new_fix, true),
                Err(e) => {
                    no_flip(&format!("raising the virtual height failed ({e})"));
                    return (var, fix, false, None);
                }
            }
        }
    };

    // The driver may have adjusted what we asked for.
    let (stride, memory_len) = geometry(&new_var, &new_fix, bpp);
    let front = FbVarScreeninfo {
        yoffset: 0,
        ..new_var
    };
    let fits = new_var.yres_virtual >= 2 * new_var.yres
        && area_offset(&front, new_var.yres, stride, bpp) + area_len(&front, stride, bpp)
            <= memory_len;
    let panned = if fits {
        fbdev::pan_display(fd, &front).map_err(|e| format!("FBIOPAN_DISPLAY failed ({e})"))
    } else {
        Err("two pages do not fit".to_owned())
    };
    match panned {
        Ok(()) => (
            front,
            new_fix,
            resized,
            Some(Flip {
                var: front,
                front: 0,
                stale: None,
            }),
        ),
        Err(why) => {
            no_flip(&why);
            if !resized {
                return (var, fix, false, None);
            }
            match fbdev::put_var_screeninfo(fd, &var).and_then(|_| fbdev::fix_screeninfo(fd)) {
                Ok(fix) => (var, fix, false, None),
                Err(e) => {
                    log::warn!("{}: restoring the screen info: {e}", path.display());
                    (new_var, new_fix, true, None)
                }
            }
        }
    }
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
        let (stride, memory_len) = geometry(&var, &fix, bpp);
        let needed = area_offset(&var, var.yoffset, stride, bpp) + area_len(&var, stride, bpp);
        if panel_size.contains(&0) || needed > memory_len {
            return Err(fb_err(
                &path,
                &format!("{panel_size:?} px at stride {stride} does not fit {memory_len} bytes"),
            ));
        }

        let original_var = var;
        let (var, fix, resized, flip) = if options.vsync {
            setup_flip(fd, &path, var, fix, bpp)
        } else {
            (var, fix, false, None)
        };
        let (stride, memory_len) = geometry(&var, &fix, bpp);
        let visible_offset = area_offset(&var, var.yoffset, stride, bpp);

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
            if resized {
                let _ = fbdev::put_var_screeninfo(fd, &original_var);
            }
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
             page flipping {}, physical size {}",
            path.display(),
            panel_size[0],
            panel_size[1],
            format.name(),
            var.bits_per_pixel,
            if vsync { "on" } else { "off" },
            if flip.is_some() { "on" } else { "off" },
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
            original_var,
            resized,
            flip,
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
        // With page flipping, draw into the hidden page, which also lacks the last frame's changes.
        let (rect, offset) = match &mut self.flip {
            Some(flip) => {
                let copy = flip.stale.map_or(rect, |stale| stale.union(rect));
                flip.stale = Some(rect);
                let back = 1 - flip.front;
                let yoffset = back as u32 * flip.var.yres;
                let bpp = self.format.bytes_per_pixel;
                (copy, area_offset(&flip.var, yoffset, self.stride, bpp))
            }
            None => (rect, self.visible_offset),
        };
        let panel_rect = blit::logical_to_panel_rect(rect, self.rotation, self.panel_size);

        // Also makes sure the last pan took effect, so the hidden page isn't scanned out anymore.
        if self.vsync
            && let Err(e) = wait_for_vsync(self.file.as_raw_fd())
        {
            log::debug!("FBIO_WAITFORVSYNC failed ({e}), disabling vsync");
            self.vsync = false;
        }

        // SAFETY: the mapping is valid until Drop, and `new`/`setup_flip` checked that the
        // visible page, or both pages, fit.
        let memory = unsafe {
            std::slice::from_raw_parts_mut(self.memory.add(offset), self.memory_len - offset)
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

        if let Some(flip) = &mut self.flip {
            let back = 1 - flip.front;
            let var = FbVarScreeninfo {
                yoffset: back as u32 * flip.var.yres,
                ..flip.var
            };
            match fbdev::pan_display(self.file.as_raw_fd(), &var) {
                Ok(()) => {
                    flip.var = var;
                    flip.front = back;
                }
                Err(e) => {
                    log::warn!("FBIOPAN_DISPLAY failed ({e}), page flipping turned off");
                    // Draw into the shown page from now on, all of it next time.
                    self.visible_offset = area_offset(
                        &flip.var,
                        flip.var.yoffset,
                        self.stride,
                        self.format.bytes_per_pixel,
                    );
                    self.flip = None;
                    self.previous.clear();
                }
            }
        }
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

impl LinuxFbPlatform {
    /// After page flipping: puts the last frame (the clear-on-exit frame) where the screen was at
    /// startup and shows that again, with the original virtual size.
    fn restore_view(&mut self) {
        let fd = self.file.as_raw_fd();
        let bpp = self.format.bytes_per_pixel;
        let shown = match &self.flip {
            Some(flip) => area_offset(&flip.var, flip.var.yoffset, self.stride, bpp),
            None => self.visible_offset,
        };
        let original = &self.original_var;
        let original_offset = area_offset(original, original.yoffset, self.stride, bpp);
        let len = area_len(original, self.stride, bpp);
        if shown != original_offset && original_offset.max(shown) + len <= self.memory_len {
            // SAFETY: both areas are inside the mapping (checked above); `copy` allows overlap.
            unsafe {
                std::ptr::copy(
                    self.memory.add(shown),
                    self.memory.add(original_offset),
                    len,
                );
            }
        }
        let restored = if self.resized {
            fbdev::put_var_screeninfo(fd, original).map(drop)
        } else {
            fbdev::pan_display(fd, original)
        };
        if let Err(e) = restored {
            log::warn!("{}: restoring the screen info: {e}", self.path.display());
        }
    }
}

impl Drop for LinuxFbPlatform {
    fn drop(&mut self) {
        if self.blanked {
            fbdev::unblank(self.file.as_raw_fd());
        }
        if self.flip.is_some() || self.resized {
            self.restore_view();
        }
        // SAFETY: mapped in `new` with this length.
        unsafe { libc::munmap(self.memory.cast(), self.memory_len) };
    }
}
