//! Console (VT) handling for framebuffer output: in `KD_GRAPHICS` mode the kernel's fbcon stops
//! drawing text and the blinking cursor into the framebuffer we are using. Like Qt's
//! `QFbVtHandler`.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd as _, FromRawFd as _};

const KDSETMODE: libc::c_ulong = 0x4B3A;
const KDGETMODE: libc::c_ulong = 0x4B3B;
const KD_GRAPHICS: libc::c_int = 1;

/// Restores the previous console mode when dropped.
pub(crate) struct VtGuard {
    tty: File,
    previous_mode: libc::c_int,
}

/// Switches the active VT to graphics mode. Best effort: needs the process to run on the VT
/// (e.g. a systemd service with `TTYPath=`, or started from the console) or root.
pub(crate) fn enter_graphics_mode() -> Option<VtGuard> {
    let candidates = [
        stdin_tty(),
        OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty0")
            .ok(),
    ];
    for tty in candidates.into_iter().flatten() {
        let mut mode: libc::c_int = 0;
        // SAFETY: KDGETMODE writes one int.
        if unsafe { libc::ioctl(tty.as_raw_fd(), KDGETMODE as _, &mut mode) } != 0 {
            continue; // not a VT
        }
        // SAFETY: KDSETMODE takes the mode as the argument value.
        if unsafe { libc::ioctl(tty.as_raw_fd(), KDSETMODE as _, KD_GRAPHICS) } != 0 {
            log::debug!(
                "KDSETMODE(KD_GRAPHICS): {}",
                std::io::Error::last_os_error()
            );
            continue;
        }
        log::debug!("Console switched to graphics mode");
        return Some(VtGuard {
            tty,
            previous_mode: mode,
        });
    }
    log::warn!(
        "Could not switch the console to graphics mode (needs root or running on the VT); \
         console text and the cursor may draw over the UI"
    );
    None
}

fn stdin_tty() -> Option<File> {
    // SAFETY: dup returns a new fd that the File owns.
    let fd = unsafe { libc::dup(libc::STDIN_FILENO) };
    (fd >= 0).then(|| unsafe { File::from_raw_fd(fd) })
}

impl Drop for VtGuard {
    fn drop(&mut self) {
        // Back in text mode, fbcon redraws the console.
        // SAFETY: see enter_graphics_mode.
        if unsafe { libc::ioctl(self.tty.as_raw_fd(), KDSETMODE as _, self.previous_mode) } != 0 {
            log::warn!(
                "Restoring the console mode failed: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}
