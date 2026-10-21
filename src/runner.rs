//! The run loop: no winit. Builds `RawInput`, runs egui, paints, presents, then sleeps in
//! `poll()` until a page flip completes, a repaint is requested or a repaint deadline passes.
//! Platforms without vblank events (linuxfb) are paced with [`Platform::frame_interval`].

use std::os::fd::{AsFd as _, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustix::event::{EventfdFlags, PollFd, PollFlags};

use crate::idle::IdleTimer;
use crate::platform::Platform;
use crate::render::Renderer;
use crate::{App, Error, Frame, signals};

/// Wakes the run loop from any thread (egui repaint requests) and carries the earliest requested
/// repaint time.
struct RepaintSignal {
    deadline: Mutex<Option<Instant>>,
    eventfd: OwnedFd,
}

impl RepaintSignal {
    fn new() -> Result<Self, Error> {
        let eventfd = rustix::event::eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)
            .map_err(std::io::Error::from)?;
        Ok(Self {
            deadline: Mutex::new(None),
            eventfd,
        })
    }

    fn request(&self, delay: Duration) {
        let Some(at) = Instant::now().checked_add(delay) else {
            return; // "never"
        };
        {
            let mut deadline = self.deadline.lock().unwrap_or_else(|e| e.into_inner());
            *deadline = Some(deadline.map_or(at, |d| d.min(at)));
        }
        let _ = rustix::io::write(&self.eventfd, &1u64.to_ne_bytes());
    }

    fn take_deadline(&self) -> Option<Instant> {
        self.deadline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    fn drain(&self) {
        let mut buf = [0u8; 8];
        let _ = rustix::io::read(&self.eventfd, &mut buf);
    }
}

pub(crate) struct Runner<'a, P: Platform> {
    pub platform: &'a mut P,
    pub renderer: &'a mut P::Renderer,
    pub egui_ctx: egui::Context,
    pub frame: Frame,
    /// Size egui lays out for, in pixels (after rotation).
    pub size_px: [u32; 2],
    pub native_pixels_per_point: f32,
    pub handle_signals: bool,
    #[cfg(feature = "input-evdev")]
    pub input: Option<crate::input::Input>,
    /// Swallows the gesture that wakes the display.
    #[cfg(feature = "input-evdev")]
    pub wake_filter: crate::idle::WakeFilter,
    pub idle: IdleTimer,
    /// There is input that can wake a blanked display.
    pub can_wake: bool,
}

impl<P: Platform> Runner<'_, P> {
    /// Runs until the app closes or SIGINT/SIGTERM arrives. Calls [`App::on_exit`] before
    /// returning, while the display and GL context are still up.
    pub(crate) fn run(mut self, app: &mut dyn App) -> Result<(), Error> {
        let signal = Arc::new(RepaintSignal::new()?);
        {
            let signal = signal.clone();
            self.egui_ctx.set_request_repaint_callback(move |info| {
                if info.viewport_id == egui::ViewportId::ROOT {
                    signal.request(info.delay);
                }
            });
        }
        let _signal_guard = if self.handle_signals {
            Some(signals::install(signal.eventfd.as_fd())?)
        } else {
            None
        };

        let result = self.event_loop(app, &signal);
        if self.idle.is_blanked() {
            self.wake();
        }
        #[cfg(feature = "persistence")]
        self.save(app);
        app.on_exit();
        if P::CLEAR_ON_EXIT {
            self.clear(app);
        }
        self.egui_ctx.set_request_repaint_callback(|_| {});
        result
    }

    /// Presents an empty frame in the app's clear color, so the UI doesn't stay on screen after
    /// exit. With a chroma-key mixer the clear color is the key, so the overlay disappears.
    fn clear(&mut self, app: &mut dyn App) {
        let clear_color = app.clear_color(&self.egui_ctx.global_style().visuals);
        self.renderer.paint(
            self.size_px,
            self.pixels_per_point(),
            clear_color,
            &[],
            &mut egui::TexturesDelta::default(),
        );
        if let Err(e) = self.platform.present(self.renderer) {
            log::warn!("Clearing the display on exit: {e}");
        }
    }

    fn event_loop(&mut self, app: &mut dyn App, signal: &RepaintSignal) -> Result<(), Error> {
        let start = Instant::now();
        let mut next_repaint = Some(start);
        // Earliest start of the next frame on platforms paced by a frame interval.
        let mut earliest_frame: Option<Instant> = None;
        // Next auto-save, armed by the first frame after the previous save: a change followed by
        // hours of idle still gets saved, and an idle UI never wakes up just to save.
        #[cfg(feature = "persistence")]
        let mut save_deadline: Option<Instant> = None;

        loop {
            if self.handle_signals && signals::quit_requested() {
                log::info!("Signal received, exiting");
                return Ok(());
            }

            if let Some(requested) = signal.take_deadline() {
                next_repaint = Some(next_repaint.map_or(requested, |t| t.min(requested)));
            }

            let now = Instant::now();
            #[cfg(feature = "persistence")]
            if save_deadline.is_some_and(|t| t <= now) {
                save_deadline = None;
                self.save(app);
            }
            if self.idle.should_blank(now) {
                self.blank();
            }
            let blanked = self.idle.is_blanked();
            let ready_at = match (next_repaint, earliest_frame) {
                (Some(t), Some(earliest)) => Some(t.max(earliest)),
                (t, _) => t,
            };
            let due = ready_at.is_some_and(|t| t <= now);
            // While the display is dark, repaint requests wait until it wakes up.
            if due && !blanked && self.platform.can_render() {
                // Back-to-back frames (slow frames, egui's follow-up passes) never reach the poll
                // below: read input that arrived meanwhile without waiting.
                if self.wait(signal, Some(Duration::ZERO))? {
                    self.idle.activity(Instant::now());
                }
                earliest_frame = self
                    .platform
                    .frame_interval()
                    .and_then(|i| now.checked_add(i));
                let (repaint_delay, close) = self.frame(app, start)?;
                next_repaint = Instant::now().checked_add(repaint_delay);
                if let Some(blank_after) = self.frame.blank_after_request.take() {
                    self.set_blank_after(blank_after);
                }
                #[cfg(feature = "persistence")]
                if save_deadline.is_none() && self.frame.storage.is_some() {
                    save_deadline = Instant::now().checked_add(app.auto_save_interval());
                }
                if close {
                    log::info!("App requested close");
                    return Ok(());
                }
                continue;
            }

            // Sleep until the pending page flip completes (a frame is due but cannot be rendered
            // yet) or until the next repaint. With nothing to do, block indefinitely: idle CPU
            // stays at zero.
            let timeout = if blanked {
                None // only input (or a signal) wakes us
            } else if due {
                if self.platform.event_fd().is_some() {
                    None
                } else {
                    Some(Duration::from_millis(1))
                }
            } else {
                ready_at.map(|t| t.saturating_duration_since(now))
            };
            let timeout = earliest(
                timeout,
                self.idle
                    .deadline()
                    .map(|t| t.saturating_duration_since(now)),
            );
            #[cfg(feature = "persistence")]
            let timeout = earliest(
                timeout,
                save_deadline.map(|t| t.saturating_duration_since(now)),
            );
            if self.wait(signal, timeout)? {
                // Input: repaint right away (still paced by vsync / the frame interval).
                let now = Instant::now();
                self.idle.activity(now);
                if self.idle.is_blanked() {
                    self.wake();
                }
                next_repaint = Some(next_repaint.map_or(now, |t| t.min(now)));
            }
        }
    }

    /// Lets the app save its state, adds egui's memory and writes the file if anything changed.
    #[cfg(feature = "persistence")]
    fn save(&mut self, app: &mut dyn App) {
        let Some(storage) = self.frame.storage.as_deref_mut() else {
            return;
        };
        app.save(storage);
        if app.persist_egui_memory() {
            self.egui_ctx.memory(|memory| {
                crate::set_value(storage, crate::storage::EGUI_MEMORY_KEY, memory);
            });
        }
        storage.flush();
    }

    /// Powers the display down after the idle timeout.
    fn blank(&mut self) {
        match self.platform.set_blanked(true) {
            Ok(()) => {
                log::info!("No input for a while, display blanked");
                self.idle.set_blanked(true);
            }
            Err(e) => {
                log::warn!("Idle blanking turned off: {e}");
                self.idle.set_blank_after(None);
            }
        }
    }

    /// Powers the display up again; the input that woke it is swallowed.
    fn wake(&mut self) {
        if let Err(e) = self.platform.set_blanked(false) {
            log::warn!("Waking the display: {e}");
        }
        self.idle.set_blanked(false);
        #[cfg(feature = "input-evdev")]
        self.wake_filter.start();
        log::info!("Display woken by input");
    }

    fn set_blank_after(&mut self, blank_after: Option<Duration>) {
        if blank_after.is_some() && !self.can_wake {
            log::warn!("Idle blanking needs input to wake the display; it stays off");
            return;
        }
        self.idle.set_blank_after(blank_after);
    }

    /// egui's current points-to-pixels factor.
    fn pixels_per_point(&self) -> f32 {
        self.native_pixels_per_point * self.egui_ctx.zoom_factor()
    }

    /// Blocks until something happens or `timeout` passes. Returns true if input arrived.
    fn wait(&mut self, signal: &RepaintSignal, timeout: Option<Duration>) -> Result<bool, Error> {
        let timeout = timeout.map(|d| {
            rustix::time::Timespec::try_from(d).unwrap_or(rustix::time::Timespec {
                tv_sec: i64::MAX,
                tv_nsec: 0,
            })
        });
        let (wake_ready, platform_ready, input_ready) = {
            // Order: eventfd, platform fd (if any), input fds.
            let mut fds = vec![PollFd::new(&signal.eventfd, PollFlags::IN)];
            let platform_fd = self.platform.event_fd();
            let has_platform_fd = platform_fd.is_some();
            fds.extend(platform_fd.map(|fd| PollFd::from_borrowed_fd(fd, PollFlags::IN)));
            #[cfg(feature = "input-evdev")]
            if let Some(input) = &self.input {
                fds.extend(
                    input
                        .fds()
                        .map(|fd| PollFd::from_borrowed_fd(fd, PollFlags::IN)),
                );
            }
            match rustix::event::poll(&mut fds, timeout.as_ref()) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(std::io::Error::from(e).into()),
            }
            let ready = |fd: &PollFd<'_>| !fd.revents().is_empty();
            let input_start = 1 + usize::from(has_platform_fd);
            (
                ready(&fds[0]),
                has_platform_fd && ready(&fds[1]),
                fds[input_start..].iter().any(ready),
            )
        };
        if wake_ready {
            signal.drain();
        }
        if platform_ready {
            self.platform.dispatch_events()?;
        }
        #[cfg(feature = "input-evdev")]
        if input_ready {
            let pixels_per_point = self.pixels_per_point();
            if let Some(input) = &mut self.input {
                return Ok(input.dispatch(pixels_per_point));
            }
        }
        #[cfg(not(feature = "input-evdev"))]
        let _ = input_ready;
        Ok(false)
    }

    /// Runs one egui pass, paints and presents. Returns egui's repaint delay and whether the app
    /// asked to close.
    fn frame(&mut self, app: &mut dyn App, start: Instant) -> Result<(Duration, bool), Error> {
        let frame_start = Instant::now();

        let mut raw_input = self.raw_input(start);
        app.raw_input_hook(&self.egui_ctx, &mut raw_input);

        let frame = &mut self.frame;
        #[cfg(feature = "input-evdev")]
        let input = &self.input;
        let full_output = self.egui_ctx.run_pass(raw_input, |ctx| {
            app.logic(ctx, frame);
            ctx.root_ui(|ui| {
                // The root Ui covers the whole screen; keep the app out of the safe-area insets
                // (the on-screen keyboard). Always through this child Ui, so that widget ids
                // don't change when the keyboard comes or goes.
                let content = egui::UiBuilder::new().max_rect(ctx.content_rect());
                ui.scope_builder(content, |ui| app.ui(ui, frame));
            });
            #[cfg(feature = "input-evdev")]
            if let Some(input) = input {
                input.paint_osk(ctx);
                input.paint_cursor(ctx);
            }
        });

        let egui::FullOutput {
            platform_output,
            mut textures_delta,
            shapes,
            pixels_per_point,
            viewport_output,
        } = full_output;

        #[cfg(feature = "input-evdev")]
        let input_changed = self
            .input
            .as_mut()
            .is_some_and(|input| input.handle_output(&platform_output));
        #[cfg(not(feature = "input-evdev"))]
        let input_changed = {
            let _ = platform_output;
            false
        };

        let mut close = std::mem::take(&mut self.frame.close_requested);
        let mut repaint_delay = Duration::MAX;
        if input_changed {
            repaint_delay = Duration::ZERO;
        }
        if let Some(output) = viewport_output.get(&egui::ViewportId::ROOT) {
            repaint_delay = repaint_delay.min(output.repaint_delay);
            for command in &output.commands {
                match command {
                    egui::ViewportCommand::Close => close = true,
                    other => log::trace!("Ignoring viewport command {other:?}"),
                }
            }
        }

        let primitives = self.egui_ctx.tessellate(shapes, pixels_per_point);
        let clear_color = app.clear_color(&self.egui_ctx.global_style().visuals);
        self.renderer.paint(
            self.size_px,
            pixels_per_point,
            clear_color,
            &primitives,
            &mut textures_delta,
        );
        self.frame.cpu_usage = Some(frame_start.elapsed().as_secs_f32());

        self.platform.present(self.renderer)?;
        Ok((repaint_delay, close))
    }

    fn raw_input(&mut self, start: Instant) -> egui::RawInput {
        let native_ppp = self.native_pixels_per_point;
        let pixels_per_point = self.pixels_per_point();
        let [w, h] = self.size_px;
        let screen_rect = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(w as f32, h as f32) / pixels_per_point,
        );

        let mut viewports = egui::ViewportIdMap::default();
        viewports.insert(
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                native_pixels_per_point: Some(native_ppp),
                monitor_size: Some(screen_rect.size()),
                inner_rect: Some(screen_rect),
                outer_rect: Some(screen_rect),
                focused: Some(true),
                fullscreen: Some(true),
                ..Default::default()
            },
        );

        #[cfg_attr(not(feature = "input-evdev"), expect(unused_mut))]
        let mut raw_input = egui::RawInput {
            viewport_id: egui::ViewportId::ROOT,
            viewports,
            screen_rect: Some(screen_rect),
            max_texture_side: Some(self.renderer.max_texture_side()),
            time: Some(start.elapsed().as_secs_f64()),
            predicted_dt: 1.0 / self.platform.refresh_hz().max(1.0),
            focused: true,
            ..Default::default()
        };
        #[cfg(feature = "input-evdev")]
        if let Some(input) = &mut self.input {
            // Modifier state reaches egui as `ModifiersChanged` events.
            raw_input.events = self.wake_filter.filter(input.take_events());
            input.prepare_raw_input(&mut raw_input);
        }
        raw_input
    }
}

/// The shorter of two poll timeouts, where `None` means "no timeout".
fn earliest(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}
