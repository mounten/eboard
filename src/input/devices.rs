//! Finding, opening and reading `/dev/input/event*`. New devices (USB keyboard or mouse plugged
//! in later) are picked up through inotify on `/dev/input`.

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd as _, AsRawFd as _, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use evdev::raw_stream::RawDevice;
use evdev::{AbsoluteAxisCode, KeyCode, PropType, RelativeAxisCode};
use rustix::fs::inotify;

use super::{AxisRange, Decoder, KeyboardDecoder, PointerDecoder, Shared, TouchDecoder};
use crate::{InputGrab, InputOptions};

const INPUT_DIR: &str = "/dev/input";

struct Device {
    path: PathBuf,
    raw: RawDevice,
    decoders: Vec<Decoder>,
}

pub(crate) struct InputDevices {
    devices: Vec<Device>,
    inotify: Option<OwnedFd>,
    options: InputOptions,
    next_id: u64,
}

impl InputDevices {
    pub(crate) fn open(options: &InputOptions) -> Self {
        let inotify = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)
            .and_then(|fd| {
                // Nodes appear with CREATE; udev fixes their permissions shortly after (ATTRIB).
                inotify::add_watch(
                    &fd,
                    INPUT_DIR,
                    inotify::WatchFlags::CREATE | inotify::WatchFlags::ATTRIB,
                )?;
                Ok(fd)
            })
            .inspect_err(|e| log::debug!("No input hotplug (inotify on {INPUT_DIR}: {e})"))
            .ok();

        let mut this = Self {
            devices: Vec::new(),
            inotify,
            options: options.clone(),
            next_id: 0,
        };
        let mut denied = 0;
        for path in event_nodes() {
            match this.add(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => denied += 1,
                Err(e) => log::debug!("{}: {e}", path.display()),
            }
        }
        if this.devices.is_empty() {
            if denied > 0 {
                log::warn!(
                    "No access to {INPUT_DIR}/event* ({denied} devices): add the user to the \
                     `input` group"
                );
            } else {
                log::info!("No input devices found");
            }
        }
        this
    }

    /// Opens and classifies a device. Devices that are neither touchscreen, mouse nor keyboard
    /// (power buttons, HDMI jacks, ...) are skipped.
    fn add(&mut self, path: &Path) -> io::Result<()> {
        if self.devices.iter().any(|d| d.path == path) {
            return Ok(());
        }
        let mut raw = RawDevice::open(path)?;
        set_nonblocking(raw.as_fd())?;
        let name = raw.name().unwrap_or("unnamed").to_owned();
        let decoders = decoders_for(&raw, self.next_id, &self.options);
        self.next_id += 1;
        if decoders.is_empty() {
            log::debug!("{}: {name}: not an input device we use", path.display());
            return Ok(());
        }
        let grab = match self.options.grab {
            InputGrab::None => false,
            InputGrab::Keyboards => decoders.iter().any(|d| matches!(d, Decoder::Keyboard(_))),
            InputGrab::All => true,
        };
        let grabbed = grab
            && raw
                .grab()
                .inspect_err(|e| log::warn!("{}: EVIOCGRAB failed: {e}", path.display()))
                .is_ok();
        let kinds: Vec<&str> = decoders.iter().map(Decoder::kind).collect();
        log::info!(
            "Input: {} \"{name}\" ({}{})",
            path.display(),
            kinds.join(", "),
            if grabbed { ", grabbed" } else { "" }
        );
        self.devices.push(Device {
            path: path.to_owned(),
            raw,
            decoders,
        });
        Ok(())
    }

    /// File descriptors to poll for readability.
    pub(crate) fn fds(&self) -> impl Iterator<Item = BorrowedFd<'_>> {
        self.inotify
            .iter()
            .map(|fd| fd.as_fd())
            .chain(self.devices.iter().map(|d| d.raw.as_fd()))
    }

    /// Reads everything pending from all devices (non-blocking).
    pub(crate) fn dispatch(&mut self, shared: &mut Shared) {
        self.handle_hotplug();

        let mut removed = Vec::new();
        for (index, device) in self.devices.iter_mut().enumerate() {
            loop {
                match device.raw.fetch_events() {
                    Ok(events) => {
                        let mut any = false;
                        for event in events {
                            any = true;
                            for decoder in &mut device.decoders {
                                decoder.event(
                                    event.event_type().0,
                                    event.code(),
                                    event.value(),
                                    shared,
                                );
                            }
                        }
                        if !any {
                            break;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        log::info!("Input: {} removed ({e})", device.path.display());
                        removed.push(index);
                        break;
                    }
                }
            }
        }
        for index in removed.into_iter().rev() {
            self.devices.remove(index);
            // A touch or button may have been held when the device went away.
            shared.events.push(egui::Event::PointerGone);
        }
    }

    fn handle_hotplug(&mut self) {
        let Some(fd) = &self.inotify else {
            return;
        };
        let mut buf = [MaybeUninit::<u8>::uninit(); 1024];
        let mut reader = inotify::Reader::new(fd, &mut buf);
        let mut paths = Vec::new();
        while let Ok(event) = reader.next() {
            if let Some(name) = event.file_name().and_then(|n| n.to_str().ok())
                && name.starts_with("event")
            {
                paths.push(Path::new(INPUT_DIR).join(name));
            }
        }
        for path in paths {
            if let Err(e) = self.add(&path) {
                // Usually EACCES until udev has set the permissions; ATTRIB will retry.
                log::debug!("{}: {e}", path.display());
            }
        }
    }
}

/// `/dev/input/eventN`, sorted by N.
fn event_nodes() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(INPUT_DIR) else {
        return Vec::new();
    };
    let mut nodes: Vec<(u32, PathBuf)> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let index = entry
                .file_name()
                .to_str()?
                .strip_prefix("event")?
                .parse()
                .ok()?;
            Some((index, entry.path()))
        })
        .collect();
    nodes.sort();
    nodes.into_iter().map(|(_, path)| path).collect()
}

fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    // SAFETY: plain fcntl on a valid fd.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn decoders_for(raw: &RawDevice, id: u64, options: &InputOptions) -> Vec<Decoder> {
    let has_key = |k: KeyCode| raw.supported_keys().is_some_and(|s| s.contains(k));
    let has_abs =
        |a: AbsoluteAxisCode| raw.supported_absolute_axes().is_some_and(|s| s.contains(a));
    let has_rel =
        |r: RelativeAxisCode| raw.supported_relative_axes().is_some_and(|s| s.contains(r));

    let mut decoders = Vec::new();

    let multitouch = has_abs(AbsoluteAxisCode::ABS_MT_POSITION_X)
        && has_abs(AbsoluteAxisCode::ABS_MT_POSITION_Y)
        && has_abs(AbsoluteAxisCode::ABS_MT_SLOT);
    let single_touch = has_abs(AbsoluteAxisCode::ABS_X)
        && has_abs(AbsoluteAxisCode::ABS_Y)
        && has_key(KeyCode::BTN_TOUCH);
    if multitouch || single_touch {
        if raw.properties().contains(PropType::POINTER) {
            log::debug!("{:?}: touchpads are not supported yet", raw.name());
        } else if let Some(decoder) = touch_decoder(raw, id, multitouch, options) {
            decoders.push(Decoder::Touch(decoder));
        }
    }

    if has_rel(RelativeAxisCode::REL_X)
        && has_rel(RelativeAxisCode::REL_Y)
        && has_key(KeyCode::BTN_LEFT)
    {
        decoders.push(Decoder::Pointer(PointerDecoder::default()));
    }

    // Full keyboards and keypads/remotes with navigation keys (e.g. HDMI-CEC remotes).
    if has_key(KeyCode::KEY_A) || has_key(KeyCode::KEY_ENTER) || has_key(KeyCode::KEY_UP) {
        decoders.push(Decoder::Keyboard(KeyboardDecoder::default()));
    }
    decoders
}

fn touch_decoder(
    raw: &RawDevice,
    id: u64,
    multitouch: bool,
    options: &InputOptions,
) -> Option<TouchDecoder> {
    let (x_axis, y_axis) = if multitouch {
        (
            AbsoluteAxisCode::ABS_MT_POSITION_X,
            AbsoluteAxisCode::ABS_MT_POSITION_Y,
        )
    } else {
        (AbsoluteAxisCode::ABS_X, AbsoluteAxisCode::ABS_Y)
    };
    let (mut x, mut y, mut slots) = (None, None, None);
    for (axis, info) in raw.get_absinfo().ok()? {
        let range = AxisRange {
            min: info.minimum(),
            max: info.maximum(),
        };
        if axis == x_axis {
            x = Some(range);
        } else if axis == y_axis {
            y = Some(range);
        } else if axis == AbsoluteAxisCode::ABS_MT_SLOT {
            slots = Some((info.maximum().max(0) + 1) as usize);
        }
    }
    let (x, y) = (x?, y?);
    log::debug!(
        "{:?}: touch range x {x:?}, y {y:?}, slots {slots:?}",
        raw.name()
    );
    Some(TouchDecoder::new(
        id,
        x,
        y,
        if multitouch { slots } else { None },
        options.touch_transform,
    ))
}
