//! Panel backlight through `/sys/class/backlight/<device>/{brightness,max_brightness}`.
//!
//! Idle blanking doesn't touch it: powering the display down (DPMS, `FBIOBLANK`) switches the
//! panel's backlight off in the kernel, and nothing stays dark if the app gets killed.
//!
//! Writing needs the `video` group on most distributions (or a udev rule granting it).

use std::io;
use std::path::{Path, PathBuf};

const SYSFS_BACKLIGHT: &str = "/sys/class/backlight";

/// Picks a device from `(name, type)` pairs: the one named `wanted`, else like systemd-backlight
/// the best type (`firmware` before `platform` before `raw`), then the first by name.
pub(crate) fn pick_device(entries: &[(String, String)], wanted: Option<&str>) -> Option<usize> {
    if let Some(wanted) = wanted {
        return entries.iter().position(|(name, _)| name == wanted);
    }
    let rank = |ty: &str| match ty.trim() {
        "firmware" => 0,
        "platform" => 1,
        "raw" => 2,
        _ => 3,
    };
    entries
        .iter()
        .enumerate()
        .min_by(|(_, (a_name, a_ty)), (_, (b_name, b_ty))| {
            rank(a_ty).cmp(&rank(b_ty)).then(a_name.cmp(b_name))
        })
        .map(|(i, _)| i)
}

/// Brightness `0.0..=1.0` → raw sysfs value.
pub(crate) fn to_raw(brightness: f32, max: u32) -> u32 {
    (brightness.clamp(0.0, 1.0) * max as f32).round() as u32
}

/// Raw sysfs value → brightness `0.0..=1.0`.
pub(crate) fn from_raw(raw: u32, max: u32) -> f32 {
    if max == 0 {
        return 0.0;
    }
    (raw.min(max) as f32) / max as f32
}

#[derive(Debug)]
pub(crate) struct Backlight {
    dir: PathBuf,
    max: u32,
}

impl Backlight {
    /// Opens the device named `wanted`, or the best one in `/sys/class/backlight`.
    pub(crate) fn open(wanted: Option<&str>) -> io::Result<Self> {
        Self::open_in(Path::new(SYSFS_BACKLIGHT), wanted)
    }

    fn open_in(root: &Path, wanted: Option<&str>) -> io::Result<Self> {
        let mut entries: Vec<(String, String)> = std::fs::read_dir(root)?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name().into_string().ok()?;
                let ty = std::fs::read_to_string(entry.path().join("type")).unwrap_or_default();
                Some((name, ty))
            })
            .collect();
        entries.sort();
        let index = pick_device(&entries, wanted).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                match wanted {
                    Some(name) => format!("no backlight {name:?} in {}", root.display()),
                    None => format!("no backlight in {}", root.display()),
                },
            )
        })?;
        let dir = root.join(&entries[index].0);
        let max = read_u32(&dir.join("max_brightness"))?;
        if max == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: max_brightness is 0", dir.display()),
            ));
        }
        Ok(Self { dir, max })
    }

    /// Device name, e.g. `panel_backlight@1`.
    pub(crate) fn name(&self) -> String {
        self.dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    }

    pub(crate) fn max(&self) -> u32 {
        self.max
    }

    /// Current brightness, `0.0..=1.0`.
    pub(crate) fn get(&self) -> io::Result<f32> {
        Ok(from_raw(read_u32(&self.dir.join("brightness"))?, self.max))
    }

    /// Sets the brightness, `0.0..=1.0`. On many panels 0 turns the backlight off completely.
    pub(crate) fn set(&self, brightness: f32) -> io::Result<()> {
        if !brightness.is_finite() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("brightness {brightness} is not a number"),
            ));
        }
        write_u32(&self.dir.join("brightness"), to_raw(brightness, self.max))
    }
}

fn read_u32(path: &Path) -> io::Result<u32> {
    let text = std::fs::read_to_string(path)?;
    text.trim().parse().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {text:?}: {e}", path.display()),
        )
    })
}

fn write_u32(path: &Path, value: u32) -> io::Result<()> {
    std::fs::write(path, value.to_string()).map_err(|e| {
        let hint = if e.kind() == io::ErrorKind::PermissionDenied {
            " (add the user to the `video` group)"
        } else {
            ""
        };
        io::Error::new(e.kind(), format!("{}: {e}{hint}", path.display()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(n, t)| (n.to_string(), format!("{t}\n")))
            .collect()
    }

    #[test]
    fn device_choice() {
        let list = entries(&[
            ("panel_backlight@1", "raw"),
            ("pwm-backlight", "platform"),
            ("acpi_video0", "firmware"),
            ("intel_backlight", "raw"),
        ]);
        assert_eq!(pick_device(&list, None), Some(2));
        assert_eq!(pick_device(&list[..2], None), Some(1));
        // Same type: first by name.
        assert_eq!(pick_device(&list[2..], None), Some(0));
        assert_eq!(
            pick_device(&entries(&[("b", "raw"), ("a", "raw")]), None),
            Some(1)
        );
        assert_eq!(pick_device(&list, Some("intel_backlight")), Some(3));
        assert_eq!(pick_device(&list, Some("missing")), None);
        assert_eq!(pick_device(&[], None), None);
    }

    #[test]
    fn raw_conversion() {
        assert_eq!(to_raw(0.0, 31), 0);
        assert_eq!(to_raw(1.0, 31), 31);
        assert_eq!(to_raw(0.5, 31), 16);
        assert_eq!(to_raw(2.0, 255), 255);
        assert_eq!(to_raw(-1.0, 255), 0);
        assert_eq!(from_raw(31, 31), 1.0);
        assert_eq!(from_raw(0, 31), 0.0);
        assert_eq!(from_raw(100, 31), 1.0);
        assert_eq!(from_raw(5, 0), 0.0);
        for raw in 0..=31 {
            assert_eq!(to_raw(from_raw(raw, 31), 31), raw);
        }
    }

    #[test]
    fn sysfs_device() {
        let root = std::env::temp_dir().join(format!("eboard-backlight-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (name, ty, max) in [("raw0", "raw", "31\n"), ("plat0", "platform", "255\n")] {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("type"), format!("{ty}\n")).unwrap();
            std::fs::write(dir.join("max_brightness"), max).unwrap();
            std::fs::write(dir.join("brightness"), "0\n").unwrap();
        }

        let backlight = Backlight::open_in(&root, None).unwrap();
        assert_eq!(backlight.name(), "plat0");
        assert_eq!(backlight.max(), 255);
        backlight.set(0.5).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("plat0/brightness")).unwrap(),
            "128"
        );
        assert!((backlight.get().unwrap() - 128.0 / 255.0).abs() < 1e-6);
        assert!(backlight.set(f32::NAN).is_err());

        let raw = Backlight::open_in(&root, Some("raw0")).unwrap();
        assert_eq!(raw.max(), 31);
        assert!(Backlight::open_in(&root, Some("nope")).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
