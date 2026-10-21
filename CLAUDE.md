# eboard

Runs egui apps full-screen on embedded Linux **without a compositor, winit or eframe**, the way Qt's
`eglfs` and `linuxfb` do. The public API mirrors eframe 0.37 (`App`, `Frame`, `CreationContext`,
`run`). Backends are cargo features, selected at runtime (`EBOARD_BACKEND`, `Backend::Auto`
tries them in this order):
- `kms`: DRM/KMS + GBM + EGL, painted with OpenGL ES through `egui_glow`.
- `viv-fb`: Vivante GPU on `/dev/fbN` through NXP's fbdev EGL (`fbGetDisplayByIndex`,
  `fbCreateWindow`), painted with `egui_glow`, like Qt's `eglfs_viv`. For i.MX6 BSPs that ship the
  Vivante blob without KMS/GBM.
- `linuxfb`: `/dev/fbN`, painted on the CPU by our own rasterizer (`soft` feature). For boards
  without KMS/GBM/GPU drivers (e.g. i.MX6 BSPs).

Targets: Raspberry Pi 5 (primary dev board, Touch Display 2), NVIDIA Jetson (NVGBM), NXP i.MX8
(aarch64) and i.MX6 (armv7).

## Layout
- `src/lib.rs`: `run()`, backend selection (`backends::run`), and `run_on`, which wires any
  `Platform` + its renderer to the runner.
- `src/app.rs`: `App` trait, `Frame`, `CreationContext`, `ScreenInfo`.
- `src/options.rs`: `Options`, `EBOARD_*` env overrides, mode selection (pure, tested).
- `src/geometry.rs`: rotation math (continuous and pixel-exact), pixels-per-point (pure, tested).
- `src/runner.rs`: run loop, generic over `Platform`. It sleeps in `poll()` on the platform fd (DRM
  page flips) plus an eventfd that egui's repaint callback and the signal handler write to.
  Platforms without vblank events are paced by `Platform::frame_interval`. No busy loop: idle CPU
  must stay ~0.
- `src/signals.rs`: SIGINT/SIGTERM → clean exit (the second signal `_exit`s).
- `src/idle.rs`: `IdleTimer` (blank deadline, part of the poll timeout) and `WakeFilter`, which
  swallows the gesture that wakes the display (until every touch/button/key pressed after waking
  is released; motion-only batches end it). Pure, tested. While blanked the runner renders nothing
  and polls without timeout; input calls `Platform::set_blanked(false)` (kms: connector `DPMS`
  property; fbdev: `FBIOBLANK`). Without input, blanking stays off.
- `src/storage.rs` (feature `persistence`): eframe-like `Storage`, `get_value`/`set_value` (RON),
  `FileStorage` (one RON map, written only when changed, atomically: tmp + fsync + rename + dir
  fsync; a corrupt file is moved to `*.bad`), state path resolution. The runner arms an auto-save
  deadline after each frame (part of the poll timeout) and saves before `on_exit`.
- `src/platform/`: `Platform` trait (with an associated `Renderer` type) and `Started<P>`.
  - `kms.rs`: DRM/GBM, page flips, CRTC restore, `kms::open` also creates the `GlowRenderer`.
  - `mod.rs` also has `glow_started`, which wraps any GL platform with a `GlowRenderer` (software
    rotation = requested minus hardware).
  - `egl.rs`: EGL setup (`NativeDisplay::Platform` for GBM, `Legacy` for plain `eglGetDisplay`).
    Must stay GBM-free: `viv-fb` uses it on boards without libgbm.
  - `vivfb.rs`: dlopens the fb* functions from `libEGL.so.1` (a missing `fbGetDisplayByIndex`
    means "not Vivante", so `Auto` falls through), sets `FB_MULTI_BUFFER=2` when vsync is on,
    presents with `eglSwapBuffers` and paces with `frame_interval`.
  - `fbdev.rs`: `<linux/fb.h>` structs and ioctls, refresh rate, shared by `linuxfb` and `viv-fb`.
  - `linuxfb.rs`: mmap of `/dev/fbN`. `present` copies only the damaged area of the canvas,
    converting pixel format and rotating on the way.
  - `backlight.rs`: `/sys/class/backlight` device choice (firmware > platform > raw) and
    brightness `0..=1` ↔ raw (pure, tested; std only, so it builds on every host). Blanking does
    not write `bl_power`: DPMS/FBIOBLANK switch the backlight off in the kernel (checked on the Pi).
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
  - `osk.rs` (feature `osk`): on-screen keyboard. Not egui widgets (they'd steal the
    `TextEdit`'s focus): `Osk::filter` consumes pointer/touch gestures that start on it and emits
    `Event::Text`/`Event::Key` on release over the same key. Shown while `PlatformOutput::ime` is
    set (`Auto`: only without a typing keyboard, `devices::has_keyboard`, which ignores CEC
    keypads). Reserves its height as the bottom `safe_area_insets` (always sent, they are sticky).
    The runner runs `App::ui` in a child `Ui` limited to `ctx.content_rect()` (the root `Ui` is
    the whole viewport), always, so widget ids don't change. Painted on `Order::Tooltip` with
    `Painter::new(.., viewport_rect)`; icons are shapes (the arrows aren't in egui's fonts).
    Layout/hit-test/filter are pure and tested; `osk_snapshot` renders `target/osk_snapshot.png`.
- `examples/common/mod.rs`: `Header`, shown at the top of every example (backend, output, size,
  rotation, Hz, renderer, fps over the last 120 frames, CPU per frame, Quit button). Examples
  include it with `mod common;`.
- `examples/hello.rs`: egui's hello world (name, age slider, Increment, Ferris from
  `examples/assets/ferris.png` through `egui_extras`' image loaders). It is the hardware smoke
  test; idle, it never repaints. With `persistence`, name and age survive a restart.
- `examples/stress_test.rs`, `examples/particles.rs`: benchmarks that repaint every frame (grid of
  animated circles; bouncing particles with per-particle CPU work). Use them for "CPU per frame"
  and tearing checks.
- `examples/hud.rs`: overlay HUD on a key color (chroma key for an FPGA/video mixer): key from
  `App::clear_color`, frameless central panel, feathering off, text on opaque plates.
- `tools/`: stdlib-only Python helpers for testing on a board over SSH: `uinput_touch.py`
  (virtual touchscreen), `uinput_kbdmouse.py` (virtual keyboard + mouse), both need root;
  `fb2png.py` turns a `/dev/fb0` dump into a PNG to see what the linuxfb backend drew.
- `README.md`: user-facing overview and the options table. Keep it in sync when adding options.

## Rules
- Vendor graphics libs are **dlopen'ed** (`khronos-egl` dynamic, `libloading` for libGLESv2). The
  only build-time native dependency is `libgbm` (via `gbm-sys`, `kms` feature only). Keep it that
  way: new link-time C dependencies have to be added to the cross image as well.
- A binary built with `kms` does not start on boards without `libgbm.so.1`. Build those boards with
  `--no-default-features --features viv-fb,linuxfb,input-evdev,osk,default_fonts`.
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
cargo test --features persistence
cargo test soft_snapshot -- --ignored        # renders target/soft_snapshot.png for a visual check
cargo test osk_snapshot -- --ignored         # renders target/osk_snapshot.png (on-screen keyboard)
cargo clippy --target aarch64-unknown-linux-gnu --all-targets
cargo clippy --target armv7-unknown-linux-gnueabihf --all-targets
cargo clippy --target armv7-unknown-linux-gnueabihf --no-default-features --features viv-fb,linuxfb,input-evdev --all-targets
cargo clippy --target armv7-unknown-linux-gnueabihf --no-default-features --features viv-fb --all-targets
cargo fmt --check
```

Cross builds use cross-rs with Podman (`podman machine start` if it is stopped). The image comes from
`docker/Dockerfile.cross-focal` (Ubuntu 20.04 + `libgbm-dev` for the target arch), see `Cross.toml`:
```sh
cross build --release --target aarch64-unknown-linux-gnu --examples
cross build --release --target armv7-unknown-linux-gnueabihf --examples
# i.MX6 without KMS/GBM: Vivante fbdev EGL or CPU framebuffer, no libgbm needed on the board
cross build --release --target armv7-unknown-linux-gnueabihf --examples --no-default-features --features viv-fb,linuxfb,input-evdev,osk,default_fonts
cross build --release --target x86_64-unknown-linux-gnu --examples
cross test --target x86_64-unknown-linux-gnu   # run the tests on Linux (the image has libgbm)
```
If cross build scripts fail with `GLIBC_x.y not found` after an image change, delete
`target/release/{build,deps,.fingerprint}` and `target/<triple>/release` (stale host artifacts).

## Workflow
- `main` is protected (ruleset "Protect main"): no direct pushes. Work on a branch, open a PR whose
  **title** is a Conventional Commit (it becomes the squash commit and the changelog entry), and
  wait for all required checks: `check`, `check (windows-latest)`, `check (macos-latest)`,
  `boards / aarch64|armv7|armv7-linuxfb|x86_64` and `pr-title`. The owner merges.
- Changes that affect what users see (new options, features, behavior) need a README update.

## CI and releases
Modeled on the owner's egui-kit crate (see `CONTRIBUTING.md`):
- `.github/workflows/ci.yml`: fmt, clippy for the host and both ARM targets plus feature combos,
  tests (the runner installs `libgbm-dev`), docs, a Windows/macOS build check, the board builds and
  a Conventional Commits check of PR titles (PRs are squash-merged).
- `.github/workflows/boards.yml` (reusable): `cross build --examples` for aarch64, armv7,
  armv7-linuxfb (`viv-fb` + `linuxfb`, must not link libgbm) and x86_64; fails if an example is
  missing or needs glibc > 2.31. Packs all examples into `eboard-examples-<board>.tar.gz` and
  uploads it as an artifact in CI, or attaches it to the GitHub release. New examples are picked
  up automatically (`cargo metadata`).
- `.github/workflows/release-plz.yml` + `release-plz.toml`: release PR on every push to `main`;
  merging it publishes to crates.io, tags `vX.Y.Z` and creates the GitHub release. Needs the
  secrets `CARGO_REGISTRY_TOKEN` and `RELEASE_PLZ_TOKEN`.
- Commit messages and PR titles must be Conventional Commits; they become the changelog.
- Workflows can be linted locally with actionlint:
  `MSYS_NO_PATHCONV=1 podman run --rm -v "$(cygpath -w "$PWD"):/repo" -w /repo docker.io/rhysd/actionlint:latest`

## Status
- Phase 1 (KMS + EGL + glow, rotation, event-driven repaint, signals) is done and verified on a
  Raspberry Pi 5 + Touch Display 2, including `EBOARD_ROTATION=90`.
- `viv-fb` (Vivante fbdev EGL) is verified on an i.MX6QP (GC2000+, GLES 3.1, driver 6.4.11,
  `/dev/fb0` 1024x768x32, `FB_MULTI_BUFFER=2`, `Auto` picks it). That fb runs at 24 Hz (65 MHz
  pixel clock, wide horizontal blanking) into an FPGA video mixer that chroma-keys the UI over a
  24 Hz camera feed (see `examples/hud.rs`); eboard paces to it. Touch comes over CAN (apps feed
  it through `App::raw_input_hook`), so there is no evdev input on that board. On the Pi
  `viv-fb` must fail with "not the Vivante fbdev EGL" and let `Auto` fall through.
- `linuxfb` + software renderer is done and verified on the Pi 5 through DRM's fbdev emulation (0° and
  90°, 60 fps animated at ~2.6 ms CPU per frame, ~0% idle). Not yet tried on the i.MX6.
- Input (evdev touch/mouse/keyboard, hotplug, keyboard grab) is done and verified on the Pi 5 with
  virtual devices from `tools/` (taps, drag, multi-touch, rotation 90°, typing, mouse cursor).
  Still to try: real finger touches on the Touch Display 2.
- Persistence (`persistence` feature) is done and verified on the Pi 5: restore after restart,
  auto-save survives `SIGKILL` with an intact file.
- Idle blanking + backlight are done and verified on the Pi 5 with `kms` and `linuxfb`: DPMS goes
  off (backlight too), ~0% CPU while dark, the waking tap doesn't click, brightness slider writes
  sysfs. linuxfb copies the whole canvas after unblanking (fbcon may have drawn over it).
- On-screen keyboard is done and verified on the Pi 5 (linuxfb captures; kms sanity run): shows
  on focus, types with shift, Enter hides it, `Auto` hides it while `uinput_kbdmouse.py` is
  attached and shows it again on unplug, works at `EBOARD_ROTATION=90`. Not done: backspace
  auto-repeat (needs a timer), other layouts.
- Input not done yet: keyboard layouts other than US (xkbcommon), touchpads, tslib calibration,
  hardware cursor plane, key repeat settings.
- Later: GLES2 32-bit index workaround (i.MX6DL/S GC880 without `GL_OES_element_index_uint`), phase 4 extras (wgpu + `VK_KHR_display`, EGLDevice, multi-output).
- Software renderer speedups not done yet: multithreaded bands, skipping unchanged regions
  before rasterizing, fast path for axis-aligned quads (text).
- Not done yet: clipboard/IME/cursor from `PlatformOutput`, hardware rotation other than 180°,
  page flipping on fbdev (`FBIOPAN_DISPLAY`), dlopen'ing libgbm (one binary for all boards).
