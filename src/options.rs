use std::path::PathBuf;
use std::str::FromStr;

use crate::{Error, Rotation, geometry::parse_size};

/// Display backend. Like `QT_QPA_EGLFS_INTEGRATION`, it can be overridden with
/// `EGUI_EMBED_BACKEND`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Backend {
    /// Try every compiled-in backend in order: `kms`, then `linuxfb`.
    #[default]
    Auto,
    /// DRM/KMS + GBM + EGL, rendered with OpenGL ES (feature `kms`).
    Kms,
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
            "linuxfb" | "fb" | "fbdev" => Ok(Self::LinuxFb),
            other => Err(format!(
                "unknown backend {other:?} (expected auto, kms or linuxfb)"
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
    /// devices (Raspberry Pi 4/5, i.MX8, Jetson). Env: `EGUI_EMBED_KMS_DEVICE`.
    pub device: Option<PathBuf>,

    /// Framebuffer device for the `linuxfb` backend. Default: `/dev/fb0`.
    /// Env: `EGUI_EMBED_FB_DEVICE`.
    pub fb_device: Option<PathBuf>,

    /// Connector name as the kernel prints it, e.g. `DSI-1`, `HDMI-A-1`, `DP-1`.
    /// Default: the first connected connector. Env: `EGUI_EMBED_KMS_CONNECTOR`.
    pub connector: Option<String>,

    /// Display mode. Default: the connector's preferred mode. Env: `EGUI_EMBED_KMS_MODE`
    /// (`1280x720` or `1280x720@60`).
    pub mode: Option<ModeRequest>,

    /// Rotate the UI on the panel (clockwise). Env: `EGUI_EMBED_ROTATION` (`0`, `90`, `180`,
    /// `270`, `-90`).
    pub rotation: Rotation,

    /// Physical size of the panel in mm (native orientation, before rotation). Overrides what the
    /// connector reports; used to derive `pixels_per_point`. Env: `EGUI_EMBED_PHYSICAL_SIZE`
    /// (`155x88`).
    pub physical_size_mm: Option<[u32; 2]>,

    /// Fixed `native_pixels_per_point`. Default: derived from the physical size
    /// (see [`crate::pixels_per_point_from_size`]). Env: `EGUI_EMBED_PPP`.
    pub pixels_per_point: Option<f32>,
}

/// Options for [`crate::run`].
///
/// Every `EGUI_EMBED_*` environment variable that is set overrides the corresponding field, so a
/// deployed binary can be reconfigured without rebuilding (like Qt's `QT_QPA_EGLFS_*`).
#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    /// Env: `EGUI_EMBED_BACKEND` (`auto`, `kms`, `linuxfb`).
    pub backend: Backend,

    pub output: OutputConfig,

    /// Wait for vertical blank before showing a frame (no tearing). If `false`, page flips are
    /// asynchronous where the driver supports it. With `linuxfb`, the copy to the framebuffer
    /// waits for vblank if the driver supports `FBIO_WAITFORVSYNC`. Env: `EGUI_EMBED_VSYNC`
    /// (`0`/`1`).
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
        }
    }
}

impl Options {
    /// Applies the `EGUI_EMBED_*` environment variables. Called by [`crate::run`].
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

        let var = |key: &str| var(key).filter(|v| !v.trim().is_empty());

        if let Some(v) = var("EGUI_EMBED_BACKEND") {
            self.backend = parse("EGUI_EMBED_BACKEND", &v)?;
        }
        if let Some(v) = var("EGUI_EMBED_KMS_DEVICE") {
            self.output.device = Some(PathBuf::from(v));
        }
        if let Some(v) = var("EGUI_EMBED_FB_DEVICE") {
            self.output.fb_device = Some(PathBuf::from(v));
        }
        if let Some(v) = var("EGUI_EMBED_KMS_CONNECTOR") {
            self.output.connector = Some(v.trim().to_owned());
        }
        if let Some(v) = var("EGUI_EMBED_KMS_MODE") {
            self.output.mode = Some(parse("EGUI_EMBED_KMS_MODE", &v)?);
        }
        if let Some(v) = var("EGUI_EMBED_ROTATION") {
            self.output.rotation = parse("EGUI_EMBED_ROTATION", &v)?;
        }
        if let Some(v) = var("EGUI_EMBED_PHYSICAL_SIZE") {
            let size = parse_size(&v).ok_or_else(|| {
                Error::Config(format!(
                    "EGUI_EMBED_PHYSICAL_SIZE={v:?}: expected WIDTHxHEIGHT in mm"
                ))
            })?;
            self.output.physical_size_mm = Some(size);
        }
        if let Some(v) = var("EGUI_EMBED_PPP") {
            let ppp: f32 = parse("EGUI_EMBED_PPP", &v)?;
            if !(ppp.is_finite() && ppp > 0.0) {
                return Err(Error::Config(format!("EGUI_EMBED_PPP={v:?}: must be > 0")));
            }
            self.output.pixels_per_point = Some(ppp);
        }
        if let Some(v) = var("EGUI_EMBED_VSYNC") {
            self.vsync = match v.trim() {
                "1" | "true" | "on" | "yes" => true,
                "0" | "false" | "off" | "no" => false,
                _ => {
                    return Err(Error::Config(format!(
                        "EGUI_EMBED_VSYNC={v:?}: expected 0 or 1"
                    )));
                }
            };
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
            ("EGUI_EMBED_BACKEND", "KMS"),
            ("EGUI_EMBED_KMS_DEVICE", "/dev/dri/card1"),
            ("EGUI_EMBED_FB_DEVICE", "/dev/fb1"),
            ("EGUI_EMBED_KMS_CONNECTOR", "DSI-1"),
            ("EGUI_EMBED_KMS_MODE", "720x1280@60"),
            ("EGUI_EMBED_ROTATION", "-90"),
            ("EGUI_EMBED_PHYSICAL_SIZE", "88x155"),
            ("EGUI_EMBED_PPP", "1.5"),
            ("EGUI_EMBED_VSYNC", "0"),
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
    }

    #[test]
    fn empty_env_values_are_ignored() {
        assert_eq!(
            with_env(&[("EGUI_EMBED_ROTATION", "")]).unwrap(),
            Options::default()
        );
    }

    #[test]
    fn invalid_env_values_are_errors() {
        assert!(with_env(&[("EGUI_EMBED_BACKEND", "wayland")]).is_err());
        assert_eq!(
            with_env(&[("EGUI_EMBED_BACKEND", "fbdev")])
                .unwrap()
                .backend,
            Backend::LinuxFb
        );
        assert!(with_env(&[("EGUI_EMBED_ROTATION", "45")]).is_err());
        assert!(with_env(&[("EGUI_EMBED_KMS_MODE", "big")]).is_err());
        assert!(with_env(&[("EGUI_EMBED_PPP", "-1")]).is_err());
        assert!(with_env(&[("EGUI_EMBED_VSYNC", "maybe")]).is_err());
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
