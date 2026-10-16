# eboard

[![crates.io](https://img.shields.io/crates/v/eboard.svg)](https://crates.io/crates/eboard)
[![docs.rs](https://docs.rs/eboard/badge.svg)](https://docs.rs/eboard)
[![License: MIT OR Apache-2.0](https://img.shields.io/crates/l/eboard.svg)](#license)

**eframe for embedded boards.** Run [egui](https://github.com/emilk/egui) apps full-screen on
embedded Linux **without a compositor, X11/Wayland or winit**, similar to Qt's `eglfs` and `linuxfb`
platform plugins. The API mirrors [eframe](https://github.com/emilk/egui/tree/main/crates/eframe),
so moving an app between desktop and board is mostly a change of the entry point.

```rust
struct Hello;

impl eboard::App for Hello {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eboard::Frame) {
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

Input is the separate `input-evdev` feature (default), and `default_fonts` (default) bundles
egui's fonts.

The backend is picked at runtime. `auto` (default) tries `kms` first, then `viv-fb`, then
`linuxfb`.

- The UI only repaints when something changes (input, animation, `request_repaint`), so idle CPU
  stays near 0%.
- `kms` uses vsync via page flips. It picks the DRM device that has a connected display, which
  matters on SoCs where the GPU and the display controller are separate devices.
- `viv-fb` sets `FB_MULTI_BUFFER=2` (unless it is already set) for double buffering with vsync,
  like Qt does. That needs a framebuffer with at least twice the screen height of virtual space;
  eboard warns if it is smaller.
- `linuxfb` copies only the changed area to the framebuffer and supports 16/24/32 bpp formats.
- SIGINT/SIGTERM exit cleanly: `App::on_exit` is called and the display/console is restored.

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

## Fonts

`default_fonts` bundles egui's fonts (about 1 MB). To ship your own fonts instead, turn off the
default features, keep the ones you need, and install the fonts when the app is created:

```toml
eboard = { version = "0.1", default-features = false, features = ["kms", "viv-fb", "linuxfb", "input-evdev"] }
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
| `vsync` | `EBOARD_VSYNC` | `0` | Wait for vblank (default on) |
| `multisampling` | – | `4` | MSAA samples (GL backends `kms`/`viv-fb`, default 0) |
| `dithering` | – | `false` | Dithering in the GL shader (`kms`/`viv-fb`, default on) |
| `shader_version` | – | | Force a GLSL version (`kms`/`viv-fb`) |
| `handle_signals` | – | `false` | Install SIGINT/SIGTERM handlers (default on) |
| `input.enabled` | `EBOARD_INPUT` | `0` | Read input devices (default on) |
| `input.grab` | `EBOARD_INPUT_GRAB` | `none`, `keyboards`, `all` | Exclusive access (default `keyboards`) |
| `input.touch_transform` | `EBOARD_TOUCH_TRANSFORM` | `swapxy,invx,invy` | Fix touch axes that don't match the panel |
| `input.show_cursor` | – | `false` | Draw a mouse cursor while a mouse is used (default on) |

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

The crate targets `*-linux-gnu` (vendor GPU libraries are glibc-linked). EGL and GLES are loaded at
runtime with `dlopen` (including Vivante's fbdev functions for `viv-fb`); the only library linked at
build time is `libgbm` (`kms` feature).

Cross-compiling with [cross](https://github.com/cross-rs/cross) (Docker or Podman). The image in
`docker/` is based on Ubuntu 20.04 so the binaries run on glibc 2.31 and newer:

```sh
# Raspberry Pi 4/5, Jetson, i.MX8
cross build --release --target aarch64-unknown-linux-gnu --example hello
# 32-bit ARM
cross build --release --target armv7-unknown-linux-gnueabihf --example hello
# Boards without libgbm (e.g. i.MX6 with the Vivante fbdev BSP): Vivante GPU or CPU on /dev/fbN
cross build --release --target armv7-unknown-linux-gnueabihf --example hello --no-default-features --features viv-fb,linuxfb,input-evdev,default_fonts
# Intel/AMD boards and industrial PCs
cross build --release --target x86_64-unknown-linux-gnu --example hello
```

Every GitHub release has the `hello` example for these targets attached, ready to copy to a board.

## Running

- Nothing else may own the display: stop the desktop / display manager
  (`sudo systemctl isolate multi-user.target`) and run from the console or over SSH.
- The user needs the `video` group (`/dev/dri/card*`, `/dev/fb*`, Vivante's `/dev/galcore`),
  `render` for the GPU and `input` for `/dev/input/event*`.
- With `viv-fb` and `linuxfb`, the console is switched to graphics mode so its text and cursor
  don't draw over the UI. That needs root, or the app running on the console itself (e.g. a
  systemd service with `TTYPath=`).

```sh
RUST_LOG=info ./hello                              # interactive demo; Quit button or Ctrl+C
RUST_LOG=info ./hello --animate                    # repaint every frame
RUST_LOG=info ./hud                                # overlay HUD on a green key color
EBOARD_ROTATION=90 RUST_LOG=info ./hello
EBOARD_BACKEND=viv-fb RUST_LOG=info ./hello      # i.MX6 with the Vivante fbdev driver
EBOARD_BACKEND=linuxfb RUST_LOG=info ./hello
```

## Roadmap

- Keyboard layouts via xkbcommon, touchpads, hardware cursor
- wgpu + Vulkan `VK_KHR_display`, EGLDevice for old Jetson releases, multiple outputs

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md): Conventional Commits, CI checks, releases with release-plz.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.
