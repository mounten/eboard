//! Linux framebuffer (`/dev/fbN`) structs and ioctls from `<linux/fb.h>`, shared by `linuxfb` and
//! `viv-fb`.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd as _;
use std::path::Path;

use crate::Error;

const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;
#[cfg(feature = "linuxfb")]
const FBIOPUT_VSCREENINFO: libc::c_ulong = 0x4601;
const FBIOGET_FSCREENINFO: libc::c_ulong = 0x4602;
#[cfg(feature = "linuxfb")]
const FBIOPAN_DISPLAY: libc::c_ulong = 0x4606;
const FBIOBLANK: libc::c_ulong = 0x4611;

#[cfg(feature = "linuxfb")]
const FB_ACTIVATE_NOW: u32 = 0;
/// `vmode` flag: `yoffset` wraps around instead of panning.
#[cfg(feature = "linuxfb")]
const FB_VMODE_YWRAP: u32 = 256;

const FB_BLANK_UNBLANK: libc::c_int = 0;
const FB_BLANK_POWERDOWN: libc::c_int = 4;

/// `struct fb_bitfield` from `<linux/fb.h>`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FbBitfield {
    pub offset: u32,
    pub length: u32,
    pub msb_right: u32,
}

/// `struct fb_var_screeninfo` from `<linux/fb.h>`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FbVarScreeninfo {
    pub xres: u32,
    pub yres: u32,
    pub xres_virtual: u32,
    pub yres_virtual: u32,
    pub xoffset: u32,
    pub yoffset: u32,
    pub bits_per_pixel: u32,
    pub grayscale: u32,
    pub red: FbBitfield,
    pub green: FbBitfield,
    pub blue: FbBitfield,
    pub transp: FbBitfield,
    pub nonstd: u32,
    pub activate: u32,
    /// Physical height in mm.
    pub height: u32,
    /// Physical width in mm.
    pub width: u32,
    pub accel_flags: u32,
    /// Pixel clock in picoseconds.
    pub pixclock: u32,
    pub left_margin: u32,
    pub right_margin: u32,
    pub upper_margin: u32,
    pub lower_margin: u32,
    pub hsync_len: u32,
    pub vsync_len: u32,
    pub sync: u32,
    pub vmode: u32,
    pub rotate: u32,
    pub colorspace: u32,
    pub reserved: [u32; 4],
}

const _: () = assert!(size_of::<FbVarScreeninfo>() == 160);

/// `struct fb_fix_screeninfo` from `<linux/fb.h>`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct FbFixScreeninfo {
    pub id: [u8; 16],
    pub smem_start: libc::c_ulong,
    pub smem_len: u32,
    pub type_: u32,
    pub type_aux: u32,
    pub visual: u32,
    pub xpanstep: u16,
    pub ypanstep: u16,
    pub ywrapstep: u16,
    pub line_length: u32,
    pub mmio_start: libc::c_ulong,
    pub mmio_len: u32,
    pub accel: u32,
    pub capabilities: u16,
    pub reserved: [u16; 2],
}

pub(crate) fn fb_err(path: &Path, what: &str) -> Error {
    Error::Framebuffer(format!("{}: {what}", path.display()))
}

/// Opens the framebuffer read-write and reads its variable and fixed screen info. Also returns
/// the driver id (e.g. `mxc_ipuv3_fb`).
pub(crate) fn open_and_query(
    path: &Path,
) -> Result<(File, FbVarScreeninfo, FbFixScreeninfo, String), Error> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| {
            let hint = if e.kind() == std::io::ErrorKind::PermissionDenied {
                " (add the user to the `video` group)"
            } else {
                ""
            };
            fb_err(path, &format!("{e}{hint}"))
        })?;
    let fd = file.as_raw_fd();

    let var = var_screeninfo(fd).map_err(|e| fb_err(path, &format!("FBIOGET_VSCREENINFO: {e}")))?;
    let fix = fix_screeninfo(fd).map_err(|e| fb_err(path, &format!("FBIOGET_FSCREENINFO: {e}")))?;
    let id =
        String::from_utf8_lossy(fix.id.split(|&b| b == 0).next().unwrap_or_default()).into_owned();
    log::debug!("{}: {id}, {var:?}", path.display());
    Ok((file, var, fix, id))
}

pub(crate) fn var_screeninfo(fd: libc::c_int) -> std::io::Result<FbVarScreeninfo> {
    let mut var = FbVarScreeninfo::default();
    // SAFETY: the ioctl fills the struct, which matches the kernel ABI.
    if unsafe { libc::ioctl(fd, FBIOGET_VSCREENINFO as _, &mut var) } == 0 {
        Ok(var)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub(crate) fn fix_screeninfo(fd: libc::c_int) -> std::io::Result<FbFixScreeninfo> {
    // SAFETY: plain integers and arrays, all-zero is valid.
    let mut fix: FbFixScreeninfo = unsafe { std::mem::zeroed() };
    // SAFETY: the ioctl fills the struct, which matches the kernel ABI.
    if unsafe { libc::ioctl(fd, FBIOGET_FSCREENINFO as _, &mut fix) } == 0 {
        Ok(fix)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Sets the screen info now. Returns what the driver made of it (it may round values).
#[cfg(feature = "linuxfb")]
pub(crate) fn put_var_screeninfo(
    fd: libc::c_int,
    var: &FbVarScreeninfo,
) -> std::io::Result<FbVarScreeninfo> {
    let mut var = FbVarScreeninfo {
        activate: FB_ACTIVATE_NOW,
        ..*var
    };
    // SAFETY: the ioctl reads and updates the struct, which matches the kernel ABI.
    if unsafe { libc::ioctl(fd, FBIOPUT_VSCREENINFO as _, &mut var) } == 0 {
        Ok(var)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Shows the part of the virtual framebuffer that starts at `var.xoffset`/`var.yoffset`.
#[cfg(feature = "linuxfb")]
pub(crate) fn pan_display(fd: libc::c_int, var: &FbVarScreeninfo) -> std::io::Result<()> {
    let mut var = FbVarScreeninfo {
        activate: FB_ACTIVATE_NOW,
        vmode: var.vmode & !FB_VMODE_YWRAP,
        ..*var
    };
    // SAFETY: the ioctl reads the struct, which matches the kernel ABI.
    if unsafe { libc::ioctl(fd, FBIOPAN_DISPLAY as _, &mut var) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Wakes the display if it was blanked by the console's power saving.
pub(crate) fn unblank(fd: libc::c_int) {
    let _ = blank(fd, false);
}

/// Powers the display down (`FB_BLANK_POWERDOWN`) or back up.
pub(crate) fn blank(fd: libc::c_int, blanked: bool) -> std::io::Result<()> {
    let level = if blanked {
        FB_BLANK_POWERDOWN
    } else {
        FB_BLANK_UNBLANK
    };
    // SAFETY: FBIOBLANK takes the blank level as the argument value.
    if unsafe { libc::ioctl(fd, FBIOBLANK as _, level) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Refresh rate from the mode timings; 60 Hz if the driver does not report them (DRM fbdev
/// emulation reports a zero pixel clock).
pub(crate) fn refresh_hz(var: &FbVarScreeninfo) -> f32 {
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

/// Physical size from the screen info, if the driver reports a plausible one.
pub(crate) fn physical_size_mm(var: &FbVarScreeninfo) -> Option<[u32; 2]> {
    let plausible = |mm: u32| (1..10_000).contains(&mm);
    (plausible(var.width) && plausible(var.height)).then_some([var.width, var.height])
}
