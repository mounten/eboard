# egui-embed

Runs egui apps full-screen on embedded Linux **without a compositor, winit or eframe**, the way Qt's
`eglfs` and `linuxfb` do. The public API mirrors eframe 0.37 (`App`, `Frame`, `CreationContext`,
`run`). Backends are cargo features, selected at runtime (`EGUI_EMBED_BACKEND`, `Backend::Auto`
tries them in this order):
- `kms`: DRM/KMS + GBM + EGL, painted with OpenGL ES through `egui_glow`.
- `linuxfb`: `/dev/fbN`, painted on the CPU by our own rasterizer (`soft` feature). For boards
  without KMS/GBM/GPU drivers (e.g. i.MX6 BSPs).

Targets: Raspberry Pi 5 (primary dev board, Touch Display 2), NVIDIA Jetson (NVGBM), NXP i.MX8
(aarch64) and i.MX6 (armv7).

## Layout
- `src/lib.rs`: `run()`, backend selection (`backends::run`), and `run_on`, which wires any
  `Platform` + its renderer to the runner.
- `src/app.rs`: `App` trait, `Frame`, `CreationContext`, `ScreenInfo`.
- `src/options.rs`: `Options`, `EGUI_EMBED_*` env overrides, mode selection (pure, tested).
- `src/geometry.rs`: rotation math (continuous and pixel-exact), pixels-per-point (pure, tested).
- `src/runner.rs`: run loop, generic over `Platform`. It sleeps in `poll()` on the platform fd (DRM
  page flips) plus an eventfd that egui's repaint callback and the signal handler write to.
  Platforms without vblank events are paced by `Platform::frame_interval`. No busy loop: idle CPU
  must stay ~0.
- `src/signals.rs`: SIGINT/SIGTERM → clean exit (the second signal `_exit`s).
- `src/platform/`: `Platform` trait (with an associated `Renderer` type) and `Started<P>`.
  - `kms.rs`: DRM/GBM, page flips, CRTC restore, `kms::open` also creates the `GlowRenderer`.
  - `egl.rs`: EGL setup. Must stay GBM-free so a future Vivante fbdev EGL backend can reuse it.
  - `linuxfb.rs`: fbdev ioctls + mmap. `present` copies only the damaged area of the canvas,
    converting pixel format and rotating on the way.
  - `vt.rs`: `KD_GRAPHICS` on the console, so fbcon does not draw over the UI (needs root or
    running on the VT).
- `src/render/`: `Renderer` trait.
  - `glow.rs`: wraps `egui_glow::Painter`. Software rotation renders into an FBO and blits it
    rotated.
  - `soft/raster.rs`: triangle rasterizer for epaint meshes. It must match egui_glow (gamma-space
    multiply, premultiplied blending). Integer edge functions + top-left rule, so shared edges are
    never blended twice.
  - `soft/blit.rs`: damage detection, pixel formats (16/24/32 bpp), rotated copy (pure, tested).
- `src/input/` (feature `input-evdev`): evdev input.
  - `touch.rs`, `pointer.rs`, `keyboard.rs`: pure decoders `(type, code, value)` → `egui::Event`,
    with their own constants in `codes.rs` so they test on the host. Touch also emulates the
    egui pointer for the first finger (like egui-winit).
  - `devices.rs` (Linux): opens and classifies `/dev/input/event*`, hotplug via inotify,
    `EVIOCGRAB` (keyboards by default, so typing doesn't reach the console/login prompt).
  - `mod.rs`: `Shared` state (rotation, pixels per point, modifiers, mouse cursor, internal
    clipboard) and `Input`, which the runner polls; draws the software mouse cursor.
- `examples/hello.rs`: interactive demo (button, slider, text field, touch markers). It is the
  hardware smoke test; `--animate` repaints every frame.
- `tools/`: stdlib-only Python helpers for testing on a board over SSH: `uinput_touch.py`
  (virtual touchscreen), `uinput_kbdmouse.py` (virtual keyboard + mouse), both need root;
  `fb2png.py` turns a `/dev/fb0` dump into a PNG to see what the linuxfb backend drew.
- `README.md`: user-facing overview and the options table. Keep it in sync when adding options.

## Rules
- Vendor graphics libs are **dlopen'ed** (`khronos-egl` dynamic, `libloading` for libGLESv2). The
  only build-time native dependency is `libgbm` (via `gbm-sys`, `kms` feature only). Keep it that
  way: new link-time C dependencies have to be added to the cross image as well.
- A binary built with `kms` does not start on boards without `libgbm.so.1`. Build those boards with
  `--no-default-features --features linuxfb,input-evdev`.
- Binaries must keep running on **glibc 2.31** (JetPack 5, Pi OS Bullseye, older Yocto). Don't
  switch the cross image to a newer base.
- `drm` must stay on the version `gbm` uses (currently 0.14).
- Linux platform code is gated by `target_os = "linux"` + its feature. The crate must still
  `cargo check`/`cargo test` on the Windows host; the software renderer is fully testable there.
- Keep math, parsing and pixel conversion in pure functions with unit tests, so they can be tested
  without hardware.
- Rotation convention: clockwise. `Rotation::panel_to_logical[_px]` maps panel pixels to egui pixels
  and is shared by the GL blit, the framebuffer copy and touch input.
- Input decoders must stay free of `evdev` crate types; only `devices.rs` may use the crate.
- Teardown order (kms) matters: GBM buffers → DRM framebuffers → EGL → GBM surface → GBM device.
  Restore the CRTC before destroying our framebuffers.

## Commands
Host (Windows):
```sh
cargo test                                   # unit tests, including the software rasterizer
cargo test soft_snapshot -- --ignored        # renders target/soft_snapshot.png for a visual check
cargo clippy --target aarch64-unknown-linux-gnu --all-targets
cargo clippy --target armv7-unknown-linux-gnueabihf --all-targets
cargo clippy --target armv7-unknown-linux-gnueabihf --no-default-features --features linuxfb,input-evdev --all-targets
cargo fmt --check
```

Cross builds use cross-rs with Podman (`podman machine start` if it is stopped). The image comes from
`docker/Dockerfile.cross-focal` (Ubuntu 20.04 + `libgbm-dev` for the target arch), see `Cross.toml`:
```sh
cross build --release --target aarch64-unknown-linux-gnu --example hello
cross build --release --target armv7-unknown-linux-gnueabihf --example hello
# i.MX6 without KMS/GBM: framebuffer only, no libgbm needed on the board
cross build --release --target armv7-unknown-linux-gnueabihf --example hello --no-default-features --features linuxfb,input-evdev
```
If cross build scripts fail with `GLIBC_x.y not found` after an image change, delete
`target/release/{build,deps,.fingerprint}` and `target/<triple>/release` (stale host artifacts).

## Status
- Phase 1 (KMS + EGL + glow, rotation, event-driven repaint, signals) is done and verified on a
  Raspberry Pi 5 + Touch Display 2, including `EGUI_EMBED_ROTATION=90`.
- `linuxfb` + software renderer is done and verified on the Pi 5 through DRM's fbdev emulation (0° and
  90°, 60 fps animated at ~2.6 ms CPU per frame, ~0% idle). Not yet tried on the i.MX6.
- Input (evdev touch/mouse/keyboard, hotplug, keyboard grab) is done and verified on the Pi 5 with
  virtual devices from `tools/` (taps, drag, multi-touch, rotation 90°, typing, mouse cursor).
  Still to try: real finger touches on the Touch Display 2.
- Input not done yet: keyboard layouts other than US (xkbcommon), touchpads, tslib calibration,
  hardware cursor plane, key repeat settings.
- Later: `viv-fb` (Vivante fbdev EGL, GPU path for i.MX6 BSPs that ship the Vivante blob), GLES2
  32-bit index workaround, phase 4 extras (wgpu + `VK_KHR_display`, EGLDevice, multi-output).
- Software renderer speedups not done yet: multithreaded bands, skipping unchanged regions
  before rasterizing, fast path for axis-aligned quads (text).
- Not done yet: clipboard/IME/cursor from `PlatformOutput`, hardware rotation other than 180°,
  page flipping on fbdev (`FBIOPAN_DISPLAY`), dlopen'ing libgbm (one binary for all boards).
