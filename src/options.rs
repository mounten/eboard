use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use crate::{Error, Rotation, TouchTransform, geometry::parse_size};

/// Display backend. Like `QT_QPA_EGLFS_INTEGRATION`, it can be overridden with
/// `EBOARD_BACKEND`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backend {
    /// Try every compiled-in backend in order: `kms`, `viv-fb`, then `linuxfb`.
    #[default]
    Auto,
    /// DRM/KMS + GBM + EGL, rendered with OpenGL ES (feature `kms`).
    Kms,
    /// Vivante GPU on the Linux framebuffer: NXP's EGL for fbdev (`fbGetDisplayByIndex`),
    /// rendered with OpenGL ES (feature `viv-fb`). Like Qt's `eglfs_viv`, for i.MX6 BSPs that
    /// ship the Vivante driver without KMS/GBM.
    VivFb,
    /// Linux framebuffer (`/dev/fbN`), rendered on the CPU (feature `linuxfb`). Like Qt's
    /// `linuxfb`: works without GPU drivers, e.g. on i.MX6 BSPs without KMS/GBM.
    LinuxFb,
}

impl FromStr for Backend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => Ok(Self::Auto),
            "kms" | "drm" | "gbm" => Ok(Self::Kms),
            "viv-fb" | "vivfb" | "viv" => Ok(Self::VivFb),
            "linuxfb" | "fb" | "fbdev" => Ok(Self::LinuxFb),
            other => Err(format!(
                "unknown backend {other:?} (expected auto, kms, viv-fb or linuxfb)"
            )),
        }
    }
}

/// The `N` of a `/dev/fbN` path, e.g. for Vivante's `fbGetDisplayByIndex`.
#[cfg_attr(not(feature = "viv-fb"), allow(dead_code))]
pub(crate) fn fb_index(path: &Path) -> Option<i32> {
    let digits = path.file_name()?.to_str()?.strip_prefix("fb")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Which input devices to open exclusively (`EVIOCGRAB`). Grabbed devices deliver events only to
/// this app: nothing reaches the text console (e.g. a login prompt) or other programs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputGrab {
    /// Share all devices.
    None,
    /// Grab keyboards, so typing doesn't also go to the console. Ctrl+C on such a keyboard then
    /// no longer stops the app (use SSH, systemd or an exit button).
    #[default]
    Keyboards,
    /// Grab every device we use.
    All,
}

impl FromStr for InputGrab {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "0" | "off" | "false" => Ok(Self::None),
            "keyboards" | "keyboard" => Ok(Self::Keyboards),
            "all" | "1" | "on" | "true" => Ok(Self::All),
            other => Err(format!(
                "unknown grab mode {other:?} (expected none, keyboards or all)"
            )),
        }
    }
}

/// A requested display mode. Unset fields match anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeRequest {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: Option<u32>,
}

impl FromStr for ModeRequest {
    type Err = String;

    /// Parses `"1280x720"` or `"1280x720@60"`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (size, refresh) = match s.split_once('@') {
            Some((size, refresh)) => (size, Some(refresh)),
            None => (s, None),
        };
        let [width, height] =
            parse_size(size).ok_or_else(|| format!("mode {s:?} is not WIDTHxHEIGHT[@HZ]"))?;
        let refresh_hz = refresh
            .map(|r| {
                r.trim()
                    .parse()
                    .map_err(|_| format!("refresh rate in {s:?} is not a number"))
            })
            .transpose()?;
        Ok(Self {
            width,
            height,
            refresh_hz,
        })
    }
}

#[cfg_attr(not(feature = "kms"), allow(dead_code))]
/// A display mode as offered by the connector, reduced to what mode selection needs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ModeCandidate {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: f32,
    pub preferred: bool,
}

#[cfg_attr(not(feature = "kms"), allow(dead_code))]
/// Picks a mode. Without a request: the preferred mode, else the first one (the kernel lists the
/// largest first). With a request: matching size, then the closest refresh rate if one was asked
/// for, else the preferred and then the fastest.
pub(crate) fn select_mode(
    candidates: &[ModeCandidate],
    request: Option<ModeRequest>,
) -> Option<usize> {
    let Some(request) = request else {
        return candidates
            .iter()
            .position(|m| m.preferred)
            .or((!candidates.is_empty()).then_some(0));
    };
    let matching = candidates
        .iter()
        .enumerate()
        .filter(|(_, m)| m.width == request.width && m.height == request.height);
    match request.refresh_hz {
        Some(hz) => matching
            .min_by(|(_, a), (_, b)| {
                (a.refresh_hz - hz as f32)
                    .abs()
                    .total_cmp(&(b.refresh_hz - hz as f32).abs())
            })
            .map(|(i, _)| i),
        None => matching
            .max_by(|(_, a), (_, b)| {
                a.preferred
                    .cmp(&b.preferred)
                    .then(a.refresh_hz.total_cmp(&b.refresh_hz))
            })
            .map(|(i, _)| i),
    }
}

/// Which output to use and how to drive it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OutputConfig {
    /// DRM device, e.g. `/dev/dri/card1`. Default: the first `/dev/dri/card*` with a connected
    /// connector. Needed on SoCs where the render GPU and the display controller are separate
    /// devices (Raspberry Pi 4/5, i.MX8, Jetson). Env: `EBOARD_KMS_DEVICE`.
    pub device: Option<PathBuf>,

    /// Framebuffer device for the `linuxfb` and `viv-fb` backends. Default: `/dev/fb0`.
    /// Env: `EBOARD_FB_DEVICE`.
    pub fb_device: Option<PathBuf>,

    /// Connector name as the kernel prints it, e.g. `DSI-1`, `HDMI-A-1`, `DP-1`.
    /// Default: the first connected connector. Env: `EBOARD_KMS_CONNECTOR`.
    pub connector: Option<String>,

    /// Display mode. Default: the connector's preferred mode. Env: `EBOARD_KMS_MODE`
    /// (`1280x720` or `1280x720@60`).
    pub mode: Option<ModeRequest>,

    /// Rotate the UI on the panel (clockwise). Env: `EBOARD_ROTATION` (`0`, `90`, `180`,
    /// `270`, `-90`).
    pub rotation: Rotation,

    /// Physical size of the panel in mm (native orientation, before rotation). Overrides what the
    /// connector reports; used to derive `pixels_per_point`. Env: `EBOARD_PHYSICAL_SIZE`
    /// (`155x88`).
    pub physical_size_mm: Option<[u32; 2]>,

    /// Fixed `native_pixels_per_point`. Default: derived from the physical size
    /// (see [`crate::pixels_per_point_from_size`]). Env: `EBOARD_PPP`.
    pub pixels_per_point: Option<f32>,

    /// Backlight device in `/sys/class/backlight` for [`crate::Frame::set_brightness`], e.g.
    /// `panel_backlight@1`. Default: picked automatically (`firmware` before `platform` before
    /// `raw`, like systemd). Env: `EBOARD_BACKLIGHT`.
    pub backlight: Option<String>,
}

/// Options for [`crate::run`].
///
/// Every `EBOARD_*` environment variable that is set overrides the corresponding field, so a
/// deployed binary can be reconfigured without rebuilding (like Qt's `QT_QPA_EGLFS_*`).
#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    /// Env: `EBOARD_BACKEND` (`auto`, `kms`, `viv-fb`, `linuxfb`).
    pub backend: Backend,

    pub output: OutputConfig,

    /// Wait for vertical blank before showing a frame (no tearing). If `false`, page flips are
    /// asynchronous where the driver supports it. With `viv-fb`, `FB_MULTI_BUFFER=2` is set
    /// (unless already set) for double buffering, and off means `eglSwapInterval(0)`. With
    /// `linuxfb`, the copy to the framebuffer waits for vblank if the driver supports
    /// `FBIO_WAITFORVSYNC`. Env: `EBOARD_VSYNC` (`0`/`1`).
    pub vsync: bool,

    /// Number of MSAA samples requested from EGL (0 = off). egui does its own anti-aliasing
    /// (feathering), so this is rarely useful.
    pub multisampling: u16,

    /// Dithering in the egui fragment shader. Reduces banding on 16-bit (RGB565) panels.
    pub dithering: bool,

    /// Force a GLSL version instead of detecting it from the context.
    #[cfg(feature = "glow")]
    pub shader_version: Option<egui_glow::ShaderVersion>,

    /// Install SIGINT/SIGTERM handlers that exit the run loop cleanly (restoring the display and
    /// calling [`crate::App::on_exit`]).
    pub handle_signals: bool,

    /// Touch, mouse and keyboard (feature `input-evdev`).
    pub input: InputOptions,

    /// Power the display down after this long without input (burn-in, power saving). The touch,
    /// click or key press that wakes it is swallowed, so it can't press anything. Needs input
    /// (feature `input-evdev`), otherwise nothing could wake the display. Apps can change it at
    /// runtime with [`crate::Frame::set_blank_after`]. Default: off. Env: `EBOARD_BLANK_AFTER`
    /// (seconds, `0` = off).
    pub blank_after: Option<Duration>,

    /// File the app state is saved to (feature `persistence`). Default:
    /// `$STATE_DIRECTORY/app.ron` (systemd `StateDirectory=`), else
    /// `$XDG_DATA_HOME/<app>/app.ron`, else `~/.local/share/<app>/app.ron`, where `<app>` is the
    /// executable's name. Env: `EBOARD_PERSISTENCE_PATH`.
    #[cfg(feature = "persistence")]
    pub persistence_path: Option<PathBuf>,
}

/// Input devices from `/dev/input/event*` (feature `input-evdev`). Touchscreens, mice and
/// keyboards are detected automatically, including ones plugged in later.
#[derive(Clone, Debug, PartialEq)]
pub struct InputOptions {
    /// Read input devices at all. Env: `EBOARD_INPUT` (`0`/`1`).
    pub enabled: bool,

    /// Which devices to open exclusively. Env: `EBOARD_INPUT_GRAB` (`none`, `keyboards`,
    /// `all`).
    pub grab: InputGrab,

    /// Fix touch controllers whose axes don't match the panel (applied before
    /// [`OutputConfig::rotation`], which touch follows automatically).
    /// Env: `EBOARD_TOUCH_TRANSFORM` (`swapxy,invx,invy`).
    pub touch_transform: TouchTransform,

    /// Draw a mouse cursor when a mouse is used (there is no compositor to draw one).
    pub show_cursor: bool,
}

impl Default for InputOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            grab: InputGrab::Keyboards,
            touch_transform: TouchTransform::default(),
            show_cursor: true,
        }
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            backend: Backend::Auto,
            output: OutputConfig::default(),
            vsync: true,
            multisampling: 0,
            dithering: true,
            #[cfg(feature = "glow")]
            shader_version: None,
            handle_signals: true,
            input: InputOptions::default(),
            blank_after: None,
            #[cfg(feature = "persistence")]
            persistence_path: None,
        }
    }
}

impl Options {
    /// Applies the `EBOARD_*` environment variables. Called by [`crate::run`].
    pub fn apply_env_overrides(&mut self) -> Result<(), Error> {
        self.apply_overrides(|key| std::env::var(key).ok())
    }

    fn apply_overrides(&mut self, var: impl Fn(&str) -> Option<String>) -> Result<(), Error> {
        fn parse<T: FromStr>(key: &str, value: &str) -> Result<T, Error>
        where
            T::Err: std::fmt::Display,
        {
            value
                .parse()
                .map_err(|e| Error::Config(format!("{key}={value:?}: {e}")))
        }

        fn parse_bool(key: &str, value: &str) -> Result<bool, Error> {
            match value.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "on" | "yes" => Ok(true),
                "0" | "false" | "off" | "no" => Ok(false),
                _ => Err(Error::Config(format!("{key}={value:?}: expected 0 or 1"))),
            }
        }

        let var = |key: &str| var(key).filter(|v| !v.trim().is_empty());

        if let Some(v) = var("EBOARD_BACKEND") {
            self.backend = parse("EBOARD_BACKEND", &v)?;
        }
        if let Some(v) = var("EBOARD_KMS_DEVICE") {
            self.output.device = Some(PathBuf::from(v));
        }
        if let Some(v) = var("EBOARD_FB_DEVICE") {
            self.output.fb_device = Some(PathBuf::from(v));
        }
        if let Some(v) = var("EBOARD_KMS_CONNECTOR") {
            self.output.connector = Some(v.trim().to_owned());
        }
        if let Some(v) = var("EBOARD_KMS_MODE") {
            self.output.mode = Some(parse("EBOARD_KMS_MODE", &v)?);
        }
        if let Some(v) = var("EBOARD_ROTATION") {
            self.output.rotation = parse("EBOARD_ROTATION", &v)?;
        }
        if let Some(v) = var("EBOARD_PHYSICAL_SIZE") {
            let size = parse_size(&v).ok_or_else(|| {
                Error::Config(format!(
                    "EBOARD_PHYSICAL_SIZE={v:?}: expected WIDTHxHEIGHT in mm"
                ))
            })?;
            self.output.physical_size_mm = Some(size);
        }
        if let Some(v) = var("EBOARD_PPP") {
            let ppp: f32 = parse("EBOARD_PPP", &v)?;
            if !(ppp.is_finite() && ppp > 0.0) {
                return Err(Error::Config(format!("EBOARD_PPP={v:?}: must be > 0")));
            }
            self.output.pixels_per_point = Some(ppp);
        }
        if let Some(v) = var("EBOARD_VSYNC") {
            self.vsync = parse_bool("EBOARD_VSYNC", &v)?;
        }
        if let Some(v) = var("EBOARD_BACKLIGHT") {
            self.output.backlight = Some(v.trim().to_owned());
        }
        if let Some(v) = var("EBOARD_BLANK_AFTER") {
            let seconds: f64 = parse("EBOARD_BLANK_AFTER", &v)?;
            if !(seconds.is_finite() && seconds >= 0.0) {
                return Err(Error::Config(format!(
                    "EBOARD_BLANK_AFTER={v:?}: expected seconds >= 0"
                )));
            }
            self.blank_after = (seconds > 0.0).then(|| Duration::from_secs_f64(seconds));
        }
        if let Some(v) = var("EBOARD_INPUT") {
            self.input.enabled = parse_bool("EBOARD_INPUT", &v)?;
        }
        if let Some(v) = var("EBOARD_INPUT_GRAB") {
            self.input.grab = parse("EBOARD_INPUT_GRAB", &v)?;
        }
        if let Some(v) = var("EBOARD_TOUCH_TRANSFORM") {
            self.input.touch_transform = parse("EBOARD_TOUCH_TRANSFORM", &v)?;
        }
        #[cfg(feature = "persistence")]
        if let Some(v) = var("EBOARD_PERSISTENCE_PATH") {
            self.persistence_path = Some(PathBuf::from(v));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn with_env(vars: &[(&str, &str)]) -> Result<Options, Error> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut options = Options::default();
        options.apply_overrides(|k| vars.get(k).cloned())?;
        Ok(options)
    }

    #[test]
    fn no_env_keeps_defaults() {
        assert_eq!(with_env(&[]).unwrap(), Options::default());
    }

    #[test]
    fn env_overrides_all_fields() {
        let o = with_env(&[
            ("EBOARD_BACKEND", "KMS"),
            ("EBOARD_KMS_DEVICE", "/dev/dri/card1"),
            ("EBOARD_FB_DEVICE", "/dev/fb1"),
            ("EBOARD_KMS_CONNECTOR", "DSI-1"),
            ("EBOARD_KMS_MODE", "720x1280@60"),
            ("EBOARD_ROTATION", "-90"),
            ("EBOARD_PHYSICAL_SIZE", "88x155"),
            ("EBOARD_PPP", "1.5"),
            ("EBOARD_VSYNC", "0"),
            ("EBOARD_INPUT_GRAB", "all"),
            ("EBOARD_TOUCH_TRANSFORM", "invy"),
            ("EBOARD_BACKLIGHT", "panel_backlight@1"),
            ("EBOARD_BLANK_AFTER", "300"),
        ])
        .unwrap();
        assert_eq!(o.backend, Backend::Kms);
        assert_eq!(o.output.device, Some(PathBuf::from("/dev/dri/card1")));
        assert_eq!(o.output.fb_device, Some(PathBuf::from("/dev/fb1")));
        assert_eq!(o.output.connector.as_deref(), Some("DSI-1"));
        assert_eq!(
            o.output.mode,
            Some(ModeRequest {
                width: 720,
                height: 1280,
                refresh_hz: Some(60)
            })
        );
        assert_eq!(o.output.rotation, Rotation::Deg270);
        assert_eq!(o.output.physical_size_mm, Some([88, 155]));
        assert_eq!(o.output.pixels_per_point, Some(1.5));
        assert!(!o.vsync);
        assert!(o.input.enabled);
        assert_eq!(o.input.grab, InputGrab::All);
        assert!(o.input.touch_transform.invert_y);
        assert_eq!(o.output.backlight.as_deref(), Some("panel_backlight@1"));
        assert_eq!(o.blank_after, Some(Duration::from_secs(300)));
    }

    #[test]
    fn blank_after_values() {
        let blank_after = |v: &str| with_env(&[("EBOARD_BLANK_AFTER", v)]).map(|o| o.blank_after);
        assert_eq!(blank_after("0").unwrap(), None);
        assert_eq!(
            blank_after("2.5").unwrap(),
            Some(Duration::from_millis(2500))
        );
        assert!(blank_after("-1").is_err());
        assert!(blank_after("inf").is_err());
        assert!(blank_after("5min").is_err());
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn env_sets_persistence_path() {
        let o = with_env(&[("EBOARD_PERSISTENCE_PATH", "/var/lib/hello/state.ron")]).unwrap();
        assert_eq!(
            o.persistence_path,
            Some(PathBuf::from("/var/lib/hello/state.ron"))
        );
    }

    #[test]
    fn empty_env_values_are_ignored() {
        assert_eq!(
            with_env(&[("EBOARD_ROTATION", "")]).unwrap(),
            Options::default()
        );
    }

    #[test]
    fn invalid_env_values_are_errors() {
        assert!(with_env(&[("EBOARD_BACKEND", "wayland")]).is_err());
        assert_eq!(
            with_env(&[("EBOARD_BACKEND", "fbdev")]).unwrap().backend,
            Backend::LinuxFb
        );
        assert_eq!(
            with_env(&[("EBOARD_BACKEND", "viv-fb")]).unwrap().backend,
            Backend::VivFb
        );
        assert!(with_env(&[("EBOARD_ROTATION", "45")]).is_err());
        assert!(with_env(&[("EBOARD_KMS_MODE", "big")]).is_err());
        assert!(with_env(&[("EBOARD_PPP", "-1")]).is_err());
        assert!(with_env(&[("EBOARD_VSYNC", "maybe")]).is_err());
    }

    #[test]
    fn backend_names() {
        for (name, backend) in [
            ("auto", Backend::Auto),
            ("", Backend::Auto),
            ("kms", Backend::Kms),
            ("viv-fb", Backend::VivFb),
            ("VivFB", Backend::VivFb),
            ("viv", Backend::VivFb),
            ("linuxfb", Backend::LinuxFb),
        ] {
            assert_eq!(name.parse(), Ok(backend), "{name:?}");
        }
        assert!("vivante".parse::<Backend>().is_err());
    }

    #[test]
    fn fb_indices() {
        let index = |p: &str| fb_index(Path::new(p));
        assert_eq!(index("/dev/fb0"), Some(0));
        assert_eq!(index("/dev/fb1"), Some(1));
        assert_eq!(index("/dev/graphics/fb12"), Some(12));
        assert_eq!(index("fb2"), Some(2));
        assert_eq!(index("/dev/fb"), None);
        assert_eq!(index("/dev/fb+1"), None);
        assert_eq!(index("/dev/fbx"), None);
        assert_eq!(index("/dev/dri/card0"), None);
        assert_eq!(index("/dev/fb0/"), Some(0));
        assert_eq!(index("/"), None);
    }

    fn mode(width: u32, height: u32, refresh_hz: f32, preferred: bool) -> ModeCandidate {
        ModeCandidate {
            width,
            height,
            refresh_hz,
            preferred,
        }
    }

    #[test]
    fn mode_selection() {
        let modes = [
            mode(3840, 2160, 30.0, false),
            mode(1920, 1080, 60.0, true),
            mode(1920, 1080, 50.0, false),
            mode(1920, 1080, 74.97, false),
            mode(1280, 720, 60.0, false),
        ];
        assert_eq!(select_mode(&modes, None), Some(1));
        assert_eq!(select_mode(&modes[2..], None), Some(0));
        assert_eq!(select_mode(&[], None), None);

        let req = |s: &str| Some(s.parse::<ModeRequest>().unwrap());
        assert_eq!(select_mode(&modes, req("1920x1080")), Some(1));
        assert_eq!(select_mode(&modes, req("1920x1080@50")), Some(2));
        assert_eq!(select_mode(&modes, req("1920x1080@75")), Some(3));
        assert_eq!(select_mode(&modes, req("1280x720")), Some(4));
        assert_eq!(select_mode(&modes, req("800x600")), None);
    }

    #[test]
    fn mode_without_refresh() {
        assert_eq!(
            "1920x1080".parse(),
            Ok(ModeRequest {
                width: 1920,
                height: 1080,
                refresh_hz: None
            })
        );
    }
}
