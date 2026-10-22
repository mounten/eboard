# eboard

[![crates.io](https://img.shields.io/crates/v/eboard.svg)](https://crates.io/crates/eboard)
[![docs.rs](https://docs.rs/eboard/badge.svg)](https://docs.rs/eboard)
[![License: MIT OR Apache-2.0](https://img.shields.io/crates/l/eboard.svg)](#license)

**eframe for embedded boards.** Run [egui](https://github.com/emilk/egui) apps full-screen on
embedded Linux **without a compositor, X11/Wayland or winit**, similar to Qt's `eglfs` and `linuxfb`
platform plugins. The API mirrors [eframe](https://github.com/emilk/egui/tree/main/crates/eframe),
so moving an app between desktop and board is mostly a change of the entry point. The only name
that differs is `Board` (eframe's `Frame`).

```rust
struct Hello;

impl eboard::App for Hello {
    fn ui(&mut self, ui: &mut egui::Ui, _board: &mut eboard::Board) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Hello from eboard");
        });
    }
}

fn main() {
    eboard::run(eboard::Options::default(), |_cc| Ok(Box::new(Hello))).unwrap();
}
```

Status: early, but usable: rendering plus touch, mouse and keyboard input. Tested on a
Raspberry Pi 5 with the Touch Display 2.

## Backends

| Backend | Cargo feature | Renders with | Use for |
|---|---|---|---|
| `kms` | `kms` (default) | OpenGL ES 2/3 (`egui_glow`) via DRM/KMS + GBM + EGL | Mesa GPUs (Raspberry Pi, i.MX8 etnaviv, ...), NVIDIA Jetson (NVGBM) |
| `viv-fb` | `viv-fb` (default) | OpenGL ES 2/3 (`egui_glow`) via Vivante's fbdev EGL on `/dev/fbN` | i.MX6 BSPs with the Vivante GPU driver but no KMS/GBM (like Qt's `eglfs_viv`) |
| `linuxfb` | `linuxfb` (default) | CPU (built-in software rasterizer) | `/dev/fbN` without GPU/KMS drivers, e.g. i.MX6 BSPs |

Input is the separate `input-evdev` feature (default), `osk` (default) adds an on-screen keyboard,
`default_fonts` (default) bundles egui's fonts, and `persistence` saves app state between runs.

The backend is picked at runtime. `auto` (default) tries `kms` first, then `viv-fb`, then
`linuxfb`.

- The UI only repaints when something changes (input, animation, `request_repaint`), so idle CPU
  stays near 0%.
- `kms` uses vsync via page flips. It picks the DRM device that has a connected display, which
  matters on SoCs where the GPU and the display controller are separate devices.
- `viv-fb` sets `FB_MULTI_BUFFER=2` (unless it is already set) for double buffering with vsync,
  like Qt does. That needs a framebuffer with at least twice the screen height of virtual space;
  eboard warns if it is smaller.
- `linuxfb` redraws only the parts of the screen whose shapes changed since the last frame (a
  blinking cursor or a changing label costs a fraction of a full frame), copies only those to the
  framebuffer, and supports 16/24/32 bpp formats.
  With vsync on it double buffers (page flipping with `FBIOPAN_DISPLAY`) if the driver can pan
  and the framebuffer has room for two screens; otherwise it draws into the visible screen.
- SIGINT/SIGTERM exit cleanly: `App::on_exit` is called and the display/console is restored.
- Optional idle blanking and backlight control, see below.

## Input

With the `input-evdev` feature (default), touchscreens, mice and keyboards are read from
`/dev/input/event*` directly (no libinput needed). Devices plugged in later are picked up
automatically.

- **Touch**: multi-touch (protocol B) and single-touch panels. egui gets touch events, and the first
  finger also acts as the mouse pointer, so all widgets work by tapping. Touch follows
  `rotation`; for touch controllers mounted differently from the panel use `touch_transform`.
- **Mouse**: motion, buttons, wheel. A cursor is drawn by eboard (there is no compositor).
- **Keyboard**: US layout (other layouts are planned via xkbcommon). Ctrl+C/X/V work inside the
  app with an internal clipboard.
- Keyboards are grabbed by default (`EVIOCGRAB`), so typing doesn't also go to the text console
  behind the app (e.g. a login prompt). Ctrl+C on that keyboard then doesn't stop the app.
- **On-screen keyboard** (`osk` feature, default): on touch-only devices a keyboard slides in at
  the bottom while a text field has the focus, and panels lay out above it (it is reported as the
  bottom safe-area inset, `ctx.content_rect()`). US letters with shift (double tap locks caps), a
  `?123` symbols page, backspace (repeats while held), enter and a hide key. Enter in a single-line field and the hide
  key drop the focus, which hides the keyboard. By default (`input.osk = auto`) it only appears
  while no hardware keyboard is attached; `always` shows it anyway, `off` never.

## Screen blanking and backlight

Panels that run around the clock should go dark when nobody uses them (burn-in, power). With
`blank_after` (or `EBOARD_BLANK_AFTER=300`), eboard powers the display down after that long
without input: DPMS off with `kms`, `FBIOBLANK` with `viv-fb` and `linuxfb`; the kernel switches
the panel's backlight off with it. Nothing is rendered while it is dark, so the CPU stays idle.

The next touch, click or key press wakes the display, and that whole gesture is swallowed: a tap
on a dark screen can't press a button nobody could see. Apps can change the timeout at runtime,
or turn blanking off while a video or an alarm is shown, with `Board::set_blank_after`. Blanking
needs input (`input-evdev`), otherwise nothing could wake the display.

`Board::brightness()` and `Board::set_brightness(0.0..=1.0)` control the backlight through
`/sys/class/backlight`. The device is picked automatically (`firmware`, then `platform`, then
`raw`, like systemd) or set with `output.backlight` / `EBOARD_BACKLIGHT`. Writing it needs the
`video` group on most distributions, or a udev rule such as:

```
# /etc/udev/rules.d/90-backlight.rules
SUBSYSTEM=="backlight", ACTION=="add", RUN+="/bin/chgrp video /sys/class/backlight/%k/brightness", RUN+="/bin/chmod g+w /sys/class/backlight/%k/brightness"
```

On many panels a brightness of 0 switches the backlight off completely, so let sliders start a
little above 0.

## Persistence

With the `persistence` feature, eboard saves state like eframe does: implement `App::save`, read
the state back from `CreationContext::storage`, and egui's memory (collapsed headers, scroll
positions, zoom) is saved too.

```rust
impl eboard::App for MyApp {
    fn save(&mut self, storage: &mut dyn eboard::Storage) {
        eboard::set_value(storage, eboard::APP_KEY, &self.settings);
    }
    // ...
}

eboard::run(eboard::Options::default(), |cc| {
    let settings = cc
        .storage
        .and_then(|s| eboard::get_value(s, eboard::APP_KEY))
        .unwrap_or_default();
    Ok(Box::new(MyApp { settings }))
})?;
```

`App::save` runs on exit (also on SIGINT/SIGTERM) and every `App::auto_save_interval` (30 s) after
the UI was repainted. Boards lose power without warning and flash wears out, so the file is only
written when its content changed, and atomically (temporary file, fsync, rename, fsync of the
directory): after a power cut there is either the old or the new state. A file that doesn't parse
is moved to `app.ron.bad` and the app starts with empty state. Settings that must survive an
immediate power cut can be saved and flushed right away through `Board::storage_mut`.

The file is `persistence_path` if set, else `$STATE_DIRECTORY/app.ron` (systemd
`StateDirectory=`, the usual choice for a kiosk service), else `$XDG_DATA_HOME/<app>/app.ron`, else
`~/.local/share/<app>/app.ron`, where `<app>` is the executable's name.

## Fonts

`default_fonts` bundles egui's fonts (about 1 MB). To ship your own fonts instead, turn off the
default features, keep the ones you need, and install the fonts when the app is created:

```toml
eboard = { version = "0.1", default-features = false, features = ["kms", "viv-fb", "linuxfb", "input-evdev", "osk"] }
```

```rust
eboard::run(eboard::Options::default(), |cc| {
    let mut fonts = egui::FontDefinitions::empty();
    let font = std::fs::read("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf")?;
    fonts.font_data.insert("DejaVuSans".into(), Arc::new(egui::FontData::from_owned(font)));
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.insert(family, vec!["DejaVuSans".into()]);
    }
    cc.egui_ctx.set_fonts(fonts);
    Ok(Box::new(MyApp))
})?;
```

Without any fonts egui draws boxes instead of text. Cargo features are additive, so if another
dependency enables `egui/default_fonts`, the fonts are bundled anyway.

## Options

Every option can be set in code (`eboard::Options`) or overridden at runtime with an environment
variable, so a deployed binary can be reconfigured without rebuilding.

| `Options` field | Environment variable | Example | Meaning |
|---|---|---|---|
| `backend` | `EBOARD_BACKEND` | `kms`, `viv-fb`, `linuxfb` | Backend (`auto` by default) |
| `output.device` | `EBOARD_KMS_DEVICE` | `/dev/dri/card1` | DRM device (default: first one with a connected display) |
| `output.connector` | `EBOARD_KMS_CONNECTOR` | `DSI-1`, `HDMI-A-1` | Output (default: first connected) |
| `output.mode` | `EBOARD_KMS_MODE` | `1280x720@60` | Display mode (default: preferred mode) |
| `output.fb_device` | `EBOARD_FB_DEVICE` | `/dev/fb1` | Framebuffer for `viv-fb` and `linuxfb` (default `/dev/fb0`) |
| `output.rotation` | `EBOARD_ROTATION` | `90` | Rotate the UI clockwise: `0`, `90`, `180`, `270` |
| `output.physical_size_mm` | `EBOARD_PHYSICAL_SIZE` | `155x88` | Panel size in mm, used for the scale factor |
| `output.pixels_per_point` | `EBOARD_PPP` | `1.5` | Fixed scale factor (default: from the panel's pixel density, 160 dpi = 1.0) |
| `output.backlight` | `EBOARD_BACKLIGHT` | `panel_backlight@1` | Device in `/sys/class/backlight` for `Board::set_brightness` (default: picked automatically) |
| `vsync` | `EBOARD_VSYNC` | `0` | Wait for vblank (default on) |
| `multisampling` | – | `4` | MSAA samples (GL backends `kms`/`viv-fb`, default 0) |
| `dithering` | – | `false` | Dithering in the GL shader (`kms`/`viv-fb`, default on) |
| `shader_version` | – | | Force a GLSL version (`kms`/`viv-fb`) |
| `handle_signals` | – | `false` | Install SIGINT/SIGTERM handlers (default on) |
| `input.enabled` | `EBOARD_INPUT` | `0` | Read input devices (default on) |
| `input.grab` | `EBOARD_INPUT_GRAB` | `none`, `keyboards`, `all` | Exclusive access (default `keyboards`) |
| `input.touch_transform` | `EBOARD_TOUCH_TRANSFORM` | `swapxy,invx,invy` | Fix touch axes that don't match the panel |
| `input.show_cursor` | – | `false` | Draw a mouse cursor while a mouse is used (default on) |
| `input.osk` | `EBOARD_OSK` | `off`, `auto`, `always` | On-screen keyboard (feature `osk`, default `auto`: only without a hardware keyboard) |
| `blank_after` | `EBOARD_BLANK_AFTER` | `300` | Power the display down after this many seconds without input (default off, `0` = off) |
| `persistence_path` | `EBOARD_PERSISTENCE_PATH` | `/var/lib/myapp/state.ron` | State file (feature `persistence`, see above) |

Rotation: `90` turns a native portrait panel (e.g. 720×1280) into a landscape UI (1280×720).
`kms` lets the display controller do 180° if it can and does everything else on the GPU, `viv-fb`
rotates on the GPU, and `linuxfb` rotates while copying.

Set `RUST_LOG=info` (or `debug`) with a logger such as `env_logger` to see the chosen device,
mode, pixel format and GPU.

## Overlays (chroma key)

For a HUD that external hardware (an FPGA, a video mixer) mixes with live video, fill everything
that is not UI with a key color. The screen is cleared to `App::clear_color` before every frame,
so:

- Return the key color from `App::clear_color`.
- Give the central panel no frame (`egui::CentralPanel::default().frame(egui::Frame::NONE)`);
  by default it paints `panel_fill` over the whole screen.
- Turn off feathering (`ctx.tessellation_options_mut(|o| o.feathering = false)`), otherwise the
  anti-aliased edges of shapes blend with the key into colors the keyer won't recognize.
- Put text on opaque plates: glyphs are always anti-aliased, and on a plate their edges blend with
  the plate instead of the key. Avoid translucent fills and shadows over the key.
- Pick a key that the framebuffer format stores exactly. Channels of `00` or `ff` (e.g. pure green
  `#00ff00`) work in every format, RGB565 included.

`examples/hud.rs` does all of this (`--key RRGGBB` picks another key color).

## Building

The crate targets `*-linux-gnu` (vendor GPU libraries are glibc-linked). Nothing native is linked
at build time: libgbm, EGL and GLES are loaded at runtime with `dlopen` (including Vivante's fbdev
functions for `viv-fb`). So one binary per architecture runs on every board: where libgbm is
missing (e.g. i.MX6 with the Vivante fbdev BSP), `kms` fails to start and `auto` moves on to
`viv-fb` and `linuxfb`.

Cross-compiling with [cross](https://github.com/cross-rs/cross) (Docker or Podman). The image in
`docker/` is based on Ubuntu 20.04 so the binaries run on glibc 2.31 and newer:

```sh
# Raspberry Pi 4/5, Jetson, i.MX8
cross build --release --target aarch64-unknown-linux-gnu --examples
# 32-bit ARM (i.MX6, 32-bit Raspberry Pi OS)
cross build --release --target armv7-unknown-linux-gnueabihf --examples
# Optional: a smaller binary without `kms` for boards that can't use it (i.MX6 Vivante fbdev BSP)
cross build --release --target armv7-unknown-linux-gnueabihf --examples --no-default-features --features viv-fb,linuxfb,input-evdev,osk,default_fonts
# Intel/AMD boards and industrial PCs
cross build --release --target x86_64-unknown-linux-gnu --examples
```

Every GitHub release has all examples for these targets attached, one archive per board
(`eboard-examples-aarch64.tar.gz`, `-armv7`, `-x86_64`), ready to copy to a board.

## Running

- Nothing else may own the display: stop the desktop / display manager
  (`sudo systemctl isolate multi-user.target`) and run from the console or over SSH.
- The user needs the `video` group (`/dev/dri/card*`, `/dev/fb*`, Vivante's `/dev/galcore`),
  `render` for the GPU and `input` for `/dev/input/event*`.
- With `viv-fb` and `linuxfb`, the console is switched to graphics mode so its text and cursor
  don't draw over the UI. That needs root, or the app running on the console itself (e.g. a
  systemd service with `TTYPath=`).
- On exit, `viv-fb` and `linuxfb` fill the framebuffer with `App::clear_color`, so the last frame
  doesn't stay on screen (fbcon only redraws it if it is bound to that framebuffer). For an
  overlay, that is the key color: the HUD disappears from the mixed video.

```sh
RUST_LOG=info ./hello                              # egui's hello world; Quit button or Ctrl+C
RUST_LOG=info ./stress_test                        # grid of animated circles, repaints every frame
RUST_LOG=info ./particles                          # bouncing particles, repaints every frame
RUST_LOG=info ./hud                                # overlay HUD on a green key color
EBOARD_ROTATION=90 RUST_LOG=info ./hello
EBOARD_BACKEND=viv-fb RUST_LOG=info ./hello      # i.MX6 with the Vivante fbdev driver
EBOARD_BACKEND=linuxfb RUST_LOG=info ./hello
```

Every example shows the same header: backend, output, size, rotation, refresh rate, renderer,
frame rate and CPU time per frame, and a Quit button. `stress_test` and `particles` are for
finding a board's limits: raise the shape count until the frame rate drops below the refresh
rate.

## Roadmap

- Keyboard layouts via xkbcommon (also for the on-screen keyboard), touchpads, hardware cursor
- wgpu + Vulkan `VK_KHR_display`, EGLDevice for old Jetson releases, multiple outputs

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md): Conventional Commits, CI checks, releases with release-plz.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.
