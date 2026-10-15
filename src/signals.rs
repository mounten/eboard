//! SIGINT/SIGTERM handling: ask the run loop to exit so the display gets restored.

use std::io;
use std::os::fd::{AsRawFd as _, BorrowedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

static QUIT: AtomicBool = AtomicBool::new(false);
/// eventfd the run loop polls; written from the handler to wake it.
static WAKE_FD: AtomicI32 = AtomicI32::new(-1);

const SIGNALS: [libc::c_int; 2] = [libc::SIGINT, libc::SIGTERM];

extern "C" fn handle_signal(_signal: libc::c_int) {
    if QUIT.swap(true, Ordering::SeqCst) {
        // Second signal: the loop did not get to exit (e.g. a hung GPU). Leave now; the kernel
        // gives the display back to the console when the DRM fd is closed.
        unsafe { libc::_exit(130) };
    }
    let fd: RawFd = WAKE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        let one = 1u64.to_ne_bytes();
        // write(2) is async-signal-safe.
        unsafe { libc::write(fd, one.as_ptr().cast(), one.len()) };
    }
}

pub(crate) fn quit_requested() -> bool {
    QUIT.load(Ordering::SeqCst)
}

/// Installed handlers; dropping it restores the previous ones.
pub(crate) struct SignalGuard {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

/// Installs the handlers. `wake_fd` must stay open until the guard is dropped.
pub(crate) fn install(wake_fd: BorrowedFd<'_>) -> io::Result<SignalGuard> {
    QUIT.store(false, Ordering::SeqCst);
    WAKE_FD.store(wake_fd.as_raw_fd(), Ordering::SeqCst);

    let mut guard = SignalGuard {
        previous: Vec::new(),
    };
    for signal in SIGNALS {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handle_signal as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            let mut previous: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(signal, &action, &mut previous) != 0 {
                return Err(io::Error::last_os_error());
            }
            guard.previous.push((signal, previous));
        }
    }
    Ok(guard)
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        for (signal, previous) in self.previous.drain(..) {
            unsafe { libc::sigaction(signal, &previous, std::ptr::null_mut()) };
        }
        WAKE_FD.store(-1, Ordering::SeqCst);
    }
}
