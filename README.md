# egui-embed

Run [egui](https://github.com/emilk/egui) apps full-screen on embedded Linux **without a compositor,
X11/Wayland, winit or eframe**, similar to Qt's `eglfs` and `linuxfb` platform plugins. The API
mirrors eframe.

```rust
struct Hello;

impl egui_embed::App for Hello {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut egui_embed::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Hello from egui-embed");
        });
    }
}

fn main() {
    egui_embed::run(egui_embed::Options::default(), |_cc| Ok(Box::new(Hello))).unwrap();
}
```

Status: early. Rendering works; **input (touch, mouse, keyboard) is not implemented yet**.
Tested on a Raspberry Pi 5 with the Touch Display 2.

## Backends

| Backend | Cargo feature | Renders with | Use for |
|---|---|---|---|
| `kms` | `kms` (default) | OpenGL ES 2/3 (`egui_glow`) via DRM/KMS + GBM + EGL | Mesa GPUs (Raspberry Pi, i.MX8 etnaviv, ...), NVIDIA Jetson (NVGBM) |
| `linuxfb` | `linuxfb` (default) | CPU (built-in software rasterizer) | `/dev/fbN` without GPU/KMS drivers, e.g. i.MX6 BSPs |

The backend is picked at runtime. `auto` (default) tries `kms` first, then `linuxfb`.

- The UI only repaints when something changes (input, animation, `request_repaint`), so idle CPU
  stays near 0%.
- `kms` uses vsync via page flips. It picks the DRM device that has a connected display, which
  matters on SoCs where the GPU and the display controller are separate devices.
- `linuxfb` copies only the changed area to the framebuffer and supports 16/24/32 bpp formats.
- SIGINT/SIGTERM exit cleanly: `App::on_exit` is called and the display/console is restored.

## Options

Every option can be set in code (`egui_embed::Options`) or overridden at runtime with an environment
variable, so a deployed binary can be reconfigured without rebuilding.

| `Options` field | Environment variable | Example | Meaning |
|---|---|---|---|
| `backend` | `EGUI_EMBED_BACKEND` | `kms`, `linuxfb` | Backend (`auto` by default) |
| `output.device` | `EGUI_EMBED_KMS_DEVICE` | `/dev/dri/card1` | DRM device (default: first one with a connected display) |
| `output.connector` | `EGUI_EMBED_KMS_CONNECTOR` | `DSI-1`, `HDMI-A-1` | Output (default: first connected) |
| `output.mode` | `EGUI_EMBED_KMS_MODE` | `1280x720@60` | Display mode (default: preferred mode) |
| `output.fb_device` | `EGUI_EMBED_FB_DEVICE` | `/dev/fb1` | Framebuffer for `linuxfb` (default `/dev/fb0`) |
| `output.rotation` | `EGUI_EMBED_ROTATION` | `90` | Rotate the UI clockwise: `0`, `90`, `180`, `270` |
| `output.physical_size_mm` | `EGUI_EMBED_PHYSICAL_SIZE` | `155x88` | Panel size in mm, used for the scale factor |
| `output.pixels_per_point` | `EGUI_EMBED_PPP` | `1.5` | Fixed scale factor (default: from the panel's pixel density, 160 dpi = 1.0) |
| `vsync` | `EGUI_EMBED_VSYNC` | `0` | Wait for vblank (default on) |
| `multisampling` | – | `4` | MSAA samples (`kms` only, default 0) |
| `dithering` | – | `false` | Dithering in the GL shader (`kms` only, default on) |
| `shader_version` | – | | Force a GLSL version (`kms` only) |
| `handle_signals` | – | `false` | Install SIGINT/SIGTERM handlers (default on) |

Rotation: `90` turns a native portrait panel (e.g. 720×1280) into a landscape UI (1280×720).
`kms` lets the display controller do 180° if it can and does everything else on the GPU; `linuxfb`
rotates while copying.

Set `RUST_LOG=info` (or `debug`) with a logger such as `env_logger` to see the chosen device,
mode, pixel format and GPU.

## Building

The crate targets `*-linux-gnu` (vendor GPU libraries are glibc-linked). EGL and GLES are loaded at
runtime with `dlopen`; the only library linked at build time is `libgbm` (`kms` feature).

Cross-compiling with [cross](https://github.com/cross-rs/cross) (Docker or Podman). The image in
`docker/` is based on Ubuntu 20.04 so the binaries run on glibc 2.31 and newer:

```sh
# Raspberry Pi 4/5, Jetson, i.MX8
cross build --release --target aarch64-unknown-linux-gnu --example hello
# 32-bit ARM
cross build --release --target armv7-unknown-linux-gnueabihf --example hello
# Boards without libgbm (e.g. i.MX6 with the Vivante fbdev BSP): framebuffer only
cross build --release --target armv7-unknown-linux-gnueabihf --example hello --no-default-features --features linuxfb
```

## Running

- Nothing else may own the display: stop the desktop / display manager
  (`sudo systemctl isolate multi-user.target`) and run from the console or over SSH.
- The user needs the `video` group (`/dev/dri/card*`, `/dev/fb*`) and `render` for the GPU.
- With `linuxfb`, the console is switched to graphics mode so its text and cursor don't draw over
  the UI. That needs root, or the app running on the console itself (e.g. a systemd service with
  `TTYPath=`).

```sh
RUST_LOG=info ./hello                              # animated demo; Ctrl+C to quit
EGUI_EMBED_ROTATION=90 RUST_LOG=info ./hello
EGUI_EMBED_BACKEND=linuxfb RUST_LOG=info ./hello
```

## Roadmap

- Input: touch, mouse and keyboard via evdev/libinput (next)
- `viv-fb`: Vivante GPU on fbdev for i.MX6 BSPs (like Qt's `eglfs_viv`)
- wgpu + Vulkan `VK_KHR_display`, EGLDevice for old Jetson releases, multiple outputs
