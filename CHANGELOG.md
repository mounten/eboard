# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1](https://github.com/mounten/eboard/compare/v0.2.0...v0.2.1) - 2026-10-21

### Added

- *(linuxfb)* page flipping with FBIOPAN_DISPLAY ([#17](https://github.com/mounten/eboard/pull/17))
- *(osk)* repeat backspace while held ([#16](https://github.com/mounten/eboard/pull/16))
- *(kms)* load libgbm at runtime, one binary for all boards ([#14](https://github.com/mounten/eboard/pull/14))
- *(examples)* stress_test and particles, a shared header, all examples in the board archives ([#11](https://github.com/mounten/eboard/pull/11))

### Fixed

- *(input)* ignore BTN_TOUCH on multi-touch screens ([#18](https://github.com/mounten/eboard/pull/18))
- clear the framebuffer on exit (linuxfb, viv-fb) ([#10](https://github.com/mounten/eboard/pull/10))

### Performance

- *(soft)* redraw only the shapes that changed ([#19](https://github.com/mounten/eboard/pull/19))

## [0.2.0](https://github.com/mounten/eboard/compare/v0.1.0...v0.2.0) - 2026-10-16

### Added

- *(input)* on-screen keyboard for touch-only devices (osk feature) ([#9](https://github.com/mounten/eboard/pull/9))
- idle screen blanking and backlight control ([#8](https://github.com/mounten/eboard/pull/8))
- persist app state and egui memory (persistence feature) ([#7](https://github.com/mounten/eboard/pull/7))
- *(viv-fb)* add Vivante fbdev EGL backend for i.MX6 BSPs ([#5](https://github.com/mounten/eboard/pull/5))
- *(examples)* add a chroma-key HUD overlay example ([#4](https://github.com/mounten/eboard/pull/4))
- add default_fonts feature ([#2](https://github.com/mounten/eboard/pull/2))

### Documentation

- add crates.io, docs.rs and license badges to README ([#6](https://github.com/mounten/eboard/pull/6))

## [0.1.0](https://github.com/mounten/eboard/releases/tag/v0.1.0) - 2026-10-15

### Added

- *(input)* read touch, mouse and keyboard from evdev
- add kms and linuxfb backends

### Changed

- [**breaking**] rename the crate to eboard

### Documentation

- add README and contributor notes
