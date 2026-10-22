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

## Backends

| Backend | Cargo feature | Renders with | Use for |
|---|---|---|---|
| `kms` | `kms` (default) | OpenGL ES 2/3 (`egui_glow`) via DRM/KMS + GBM + EGL | Mesa GPUs (Raspberry Pi, i.MX8 etnaviv, ...), NVIDIA Jetson (NVGBM) |
| `viv-fb` | `viv-fb` (default) | OpenGL ES 2/3 (`egui_glow`) via Vivante's fbdev EGL on `/dev/fbN` | i.MX6 BSPs with the Vivante GPU driver but no KMS/GBM |
| `linuxfb` | `linuxfb` (default) | CPU (built-in software rasterizer) | `/dev/fbN` without GPU/KMS drivers, e.g. i.MX6 BSPs |

The backend is picked at runtime. `auto` (default) tries backends in order:
1. `kms`
2. `viv-fb`
3.  `linuxfb`

### Backend notes

- `kms` uses vsync via page flips. It picks the DRM device that has a connected display, which
  matters on SoCs where the GPU and the display controller are separate devices.
- `viv-fb` sets `FB_MULTI_BUFFER=2` (unless it is already set) for double buffering with vsync. This needs a framebuffer with at least twice the screen height of virtual space;
  eboard warns if it is smaller.
- `linuxfb` redraws only the parts of the screen whose shapes changed since the last frame, copies only those to the
  framebuffer, and supports 16/24/32 bpp formats.
  With vsync on it double buffers (page flipping with `FBIOPAN_DISPLAY`) if the driver can pan
  and the framebuffer has room for two screens; otherwise it draws into the visible screen.

## Input

With the `input-evdev` feature (default), touchscreens, mice and keyboards are read from
`/dev/input/event*` directly.

- **Touch**: multi-touch (protocol B) and single-touch panels. 
- **Mouse**: motion, buttons, wheel. A cursor is drawn by eboard (there is no compositor).
- **Keyboard**: US layout (other layouts are planned via xkbcommon). Copy/Paste works via an internal clipboard.
- **On-screen keyboard** (`osk` feature, default): on touch-only devices an on-screen-keyboard is shown (US Layout only) when no physical keyboard is detected.

## Persistence

With the `persistence` feature, eboard saves state like eframe does. 

## Fonts

egui's `default_fonts` are enabled by default. When using your own font you might want to disable `default-features`.

```toml
eboard = { version = "0.1", default-features = false, features = ["kms", "viv-fb", "linuxfb", "input-evdev", "osk"] }
```

## Options

Eboard behavior can be changed via `eboard::Options` aswell as through environment variables: 

| Environment variable | Example | Meaning |
|---|---|---|
| `EBOARD_BACKEND` | `kms`, `viv-fb`, `linuxfb`  | Backend (default: `auto`) |
| `EBOARD_KMS_DEVICE` | `/dev/dri/card1` | DRM device (default: first one with a connected display) |
| `EBOARD_KMS_CONNECTOR` | `DSI-1`, `HDMI-A-1` | Output (default: first connected) |
| `EBOARD_KMS_MODE` | `1280x720@60` | Display mode (default: preferred mode) |
| `EBOARD_FB_DEVICE` | `/dev/fb1` | Framebuffer for `viv-fb` and `linuxfb` (default: `/dev/fb0`) |
| `EBOARD_ROTATION` | `90` | Rotate the UI clockwise: `0`, `90`, `180`, `270` |
| `EBOARD_PHYSICAL_SIZE` | `155x88` | Panel size in mm, used for the scale factor |
| `EBOARD_PPP` | `1.5` | Fixed scale factor (default: from the panel's pixel density, 160 dpi = 1.0) |
| `EBOARD_BACKLIGHT` | `panel_backlight@1` | Device in `/sys/class/backlight` for `Board::set_brightness` (default: picked automatically) |
| `EBOARD_VSYNC` | `0` | Wait for vblank (default: on) |
| `EBOARD_INPUT` | `0` | Read input devices (default: on) |
| `EBOARD_INPUT_GRAB` | `none`, `keyboards`, `all` | Exclusive access (default: `keyboards`) |
| `EBOARD_TOUCH_TRANSFORM` | `swapxy,invx,invy` | Fix touch axes that don't match the panel |
| `EBOARD_OSK` | `off`, `auto`, `always` | On-screen keyboard (requires feature `osk`, default: `auto` - only without a hardware keyboard) |
| `EBOARD_BLANK_AFTER` | `300` | Power the display down after n seconds without input (default: off, `0` = off) |
| `EBOARD_PERSISTENCE_PATH` | `/var/lib/myapp/state.ron` | State file (requires feature `persistence`) |

Set `RUST_LOG=info` (or `debug`) with a logger such as `env_logger` to see the chosen device,
mode, pixel format and GPU.

## Building

Cross-compiling with [cross](https://github.com/cross-rs/cross) (Docker or Podman).

```sh
# Raspberry Pi 4/5, Jetson, i.MX8
cross build --release --target aarch64-unknown-linux-gnu --examples
# 32-bit ARM (i.MX6, 32-bit Raspberry Pi OS)
cross build --release --target armv7-unknown-linux-gnueabihf --examples
# Intel/AMD boards and industrial PCs
cross build --release --target x86_64-unknown-linux-gnu --examples
```

See GitHub release for already built examples for these targets attached

## Running

Depending on the backend used libgbm, libEGL, libGLESv2 are required.

- Nothing else may own the display: stop the desktop / display manager
  (`sudo systemctl isolate multi-user.target`) and run from the console or over SSH.
- The user needs the `video` group (`/dev/dri/card*`, `/dev/fb*`, Vivante's `/dev/galcore`),
  `render` for the GPU and `input` for `/dev/input/event*`.
- With `viv-fb` and `linuxfb`, the console is switched to graphics mode. 
- On exit, `viv-fb` and `linuxfb` fill the framebuffer with `App::clear_color`.

```sh
EBOARD_ROTATION=90 RUST_LOG=info ./hello
EBOARD_BACKEND=viv-fb RUST_LOG=info ./hello      # i.MX6 with the Vivante fbdev driver
EBOARD_BACKEND=linuxfb RUST_LOG=info ./hello
```

## Roadmap

- Keyboard layouts via xkbcommon (also for the on-screen keyboard), touchpads, hardware cursor
- wgpu + Vulkan `VK_KHR_display`, EGLDevice for old Jetson releases, multiple outputs

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md): Conventional Commits, CI checks, releases with release-plz.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.
