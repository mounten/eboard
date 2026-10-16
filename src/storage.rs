//! Persistence (feature `persistence`): a key-value store saved as one RON file, like eframe's.
//!
//! On a board the power can be cut at any moment and flash wears out, so the file is only written
//! when its content changed, and always atomically: write a temporary file, fsync it, rename it
//! over the old one, fsync the directory. After a power cut there is either the old or the new
//! file, never a torn one.

use std::collections::BTreeMap;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// Key under which [`crate::App::save`] usually stores the app state. Mirrors `eframe::APP_KEY`.
pub const APP_KEY: &str = "app";

/// Key of egui's [`egui::Memory`] (window positions, collapsed headers, ...), as in eframe.
pub(crate) const EGUI_MEMORY_KEY: &str = "egui";

/// A place to store the app state between runs. Mirrors `eframe::Storage`.
///
/// See [`crate::CreationContext::storage`] and [`crate::App::save`].
pub trait Storage {
    /// The value stored under `key`.
    fn get_string(&self, key: &str) -> Option<String>;

    /// Stores `value` under `key`.
    fn set_string(&mut self, key: &str, value: String);

    /// Removes `key`.
    fn remove_string(&mut self, key: &str);

    /// Writes the store to disk (if anything changed).
    fn flush(&mut self);
}

/// Reads the [RON](https://github.com/ron-rs/ron) value stored under `key`. `None` if there is
/// none or it doesn't decode (e.g. after the type changed).
pub fn get_value<T: serde::de::DeserializeOwned>(storage: &dyn Storage, key: &str) -> Option<T> {
    let value = storage.get_string(key)?;
    ron::from_str(&value)
        .inspect_err(|e| log::debug!("Failed to decode the stored {key:?}: {e}"))
        .ok()
}

/// Stores `value` under `key` as [RON](https://github.com/ron-rs/ron).
pub fn set_value<T: serde::Serialize>(storage: &mut dyn Storage, key: &str, value: &T) {
    match ron::to_string(value) {
        Ok(string) => storage.set_string(key, string),
        Err(e) => log::error!("Failed to encode {key:?} as RON: {e}"),
    }
}

/// The store eboard uses: a map of RON strings in one file.
pub(crate) struct FileStorage {
    path: PathBuf,
    kv: BTreeMap<String, String>,
    /// What the file contains, so unchanged state is never rewritten.
    last_written: Option<String>,
}

impl FileStorage {
    /// Loads `path`. A missing file gives an empty store. A file that doesn't parse is moved
    /// aside to `*.bad` and the store starts empty: a broken file must never stop the app.
    pub(crate) fn load(path: PathBuf) -> Self {
        let mut kv = BTreeMap::new();
        let mut last_written = None;
        match std::fs::read_to_string(&path) {
            Ok(text) => match ron::from_str::<BTreeMap<String, String>>(&text) {
                Ok(map) => {
                    log::info!("Loaded state from {}", path.display());
                    kv = map;
                    last_written = Some(text);
                }
                Err(e) => {
                    let bad = with_suffix(&path, "bad");
                    log::warn!(
                        "{}: {e}; moving it to {} and starting with empty state",
                        path.display(),
                        bad.display()
                    );
                    if let Err(e) = std::fs::rename(&path, &bad) {
                        log::warn!("Could not move {}: {e}", path.display());
                    }
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                log::info!("No saved state yet ({})", path.display());
            }
            Err(e) => log::warn!("{}: {e}; starting with empty state", path.display()),
        }
        Self {
            path,
            kv,
            last_written,
        }
    }

    /// Writes the store if it differs from the file. Returns whether it wrote.
    fn write_if_changed(&mut self) -> io::Result<bool> {
        let text = ron::ser::to_string_pretty(&self.kv, ron::ser::PrettyConfig::default())
            .map_err(io::Error::other)?;
        if self.last_written.as_ref() == Some(&text) {
            return Ok(false);
        }
        write_atomic(&self.path, text.as_bytes())?;
        self.last_written = Some(text);
        Ok(true)
    }
}

impl Storage for FileStorage {
    fn get_string(&self, key: &str) -> Option<String> {
        self.kv.get(key).cloned()
    }

    fn set_string(&mut self, key: &str, value: String) {
        self.kv.insert(key.to_owned(), value);
    }

    fn remove_string(&mut self, key: &str) {
        self.kv.remove(key);
    }

    fn flush(&mut self) {
        match self.write_if_changed() {
            Ok(true) => log::debug!("Saved state to {}", self.path.display()),
            Ok(false) => {}
            // Keep running; the next save tries again.
            Err(e) => log::warn!("Saving state to {} failed: {e}", self.path.display()),
        }
    }
}

/// `path` with `.suffix` appended to the file name (`app.ron` → `app.ron.tmp`).
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    path.with_file_name(name)
}

/// Replaces `path` with `bytes` so that a crash or power cut leaves either the old or the new
/// content.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = with_suffix(path, "tmp");
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result?;
    // Make the rename itself durable. Directories can't be opened as files on Windows.
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// Where the state file goes, in this order: `explicit` (from [`crate::Options::persistence_path`]
/// or `EBOARD_PERSISTENCE_PATH`), `$STATE_DIRECTORY/app.ron` (systemd `StateDirectory=`),
/// `$XDG_DATA_HOME/<app>/app.ron`, `$HOME/.local/share/<app>/app.ron`.
pub(crate) fn resolve_path(
    explicit: Option<&Path>,
    env: impl Fn(&str) -> Option<String>,
    app_name: &str,
) -> Option<PathBuf> {
    const FILE: &str = "app.ron";
    if let Some(path) = explicit {
        return Some(path.to_owned());
    }
    let env = |key: &str| env(key).filter(|v| !v.trim().is_empty());
    // systemd passes a colon-separated list if the unit names several directories.
    if let Some(dirs) = env("STATE_DIRECTORY")
        && let Some(first) = dirs.split(':').find(|d| !d.is_empty())
    {
        return Some(Path::new(first).join(FILE));
    }
    if let Some(data) = env("XDG_DATA_HOME") {
        return Some(Path::new(&data).join(app_name).join(FILE));
    }
    env("HOME").map(|home| {
        Path::new(&home)
            .join(".local/share")
            .join(app_name)
            .join(FILE)
    })
}

/// The executable's file name without extension, used as the state directory name.
pub(crate) fn app_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.file_stem()?.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "eboard".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A fresh, empty directory for one test.
    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("eboard-storage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn values_round_trip_through_the_file() {
        type State = (f32, String, Vec<bool>);
        let path = test_dir("round-trip").join("sub/app.ron");
        let state: State = (42.5, "a \"quoted\"\nname".to_owned(), vec![true, false]);

        let mut storage = FileStorage::load(path.clone());
        assert_eq!(get_value::<State>(&storage, APP_KEY), None);
        set_value(&mut storage, APP_KEY, &state);
        storage.set_string("other", "1".to_owned());
        storage.flush();

        let mut storage = FileStorage::load(path);
        assert_eq!(get_value::<State>(&storage, APP_KEY), Some(state));
        assert_eq!(storage.get_string("other").as_deref(), Some("1"));
        storage.remove_string("other");
        assert_eq!(storage.get_string("other"), None);
        // A value of another type doesn't decode.
        assert_eq!(get_value::<Vec<u8>>(&storage, APP_KEY), None);
    }

    #[test]
    fn writes_only_when_something_changed() {
        let path = test_dir("changes").join("app.ron");
        let mut storage = FileStorage::load(path.clone());
        // Nothing stored and no file: an empty map is still worth writing once.
        assert!(storage.write_if_changed().unwrap());
        assert!(!storage.write_if_changed().unwrap());

        storage.set_string("a", "1".to_owned());
        assert!(storage.write_if_changed().unwrap());
        storage.set_string("a", "1".to_owned());
        assert!(!storage.write_if_changed().unwrap());

        // A freshly loaded store matches its file.
        let mut storage = FileStorage::load(path);
        assert!(!storage.write_if_changed().unwrap());
        storage.set_string("a", "2".to_owned());
        assert!(storage.write_if_changed().unwrap());
    }

    #[test]
    fn corrupt_file_is_moved_aside() {
        let dir = test_dir("corrupt");
        let path = dir.join("app.ron");
        std::fs::write(&path, "{ this is not ron").unwrap();

        let mut storage = FileStorage::load(path.clone());
        assert_eq!(storage.get_string("a"), None);
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("app.ron.bad")).unwrap(),
            "{ this is not ron"
        );

        storage.set_string("a", "1".to_owned());
        storage.flush();
        assert_eq!(
            FileStorage::load(path).get_string("a").as_deref(),
            Some("1")
        );
    }

    #[test]
    fn atomic_write_leaves_no_temporary_file() {
        let dir = test_dir("atomic");
        let path = dir.join("app.ron");
        write_atomic(&path, b"old").unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["app.ron"]);
    }

    #[test]
    fn path_resolution_order() {
        let resolve = |explicit: Option<&str>, vars: &[(&str, &str)]| {
            let vars: HashMap<&str, &str> = vars.iter().copied().collect();
            resolve_path(
                explicit.map(Path::new),
                |k| vars.get(k).map(|v| v.to_string()),
                "hello",
            )
        };
        let all = [
            ("STATE_DIRECTORY", "/var/lib/hello:/var/lib/other"),
            ("XDG_DATA_HOME", "/data"),
            ("HOME", "/home/jens"),
        ];
        assert_eq!(
            resolve(Some("/etc/hello.ron"), &all),
            Some(PathBuf::from("/etc/hello.ron"))
        );
        assert_eq!(
            resolve(None, &all),
            Some(Path::new("/var/lib/hello").join("app.ron"))
        );
        assert_eq!(
            resolve(None, &all[1..]),
            Some(Path::new("/data").join("hello").join("app.ron"))
        );
        assert_eq!(
            resolve(None, &[("XDG_DATA_HOME", ""), ("HOME", "/home/jens")]),
            Some(
                Path::new("/home/jens")
                    .join(".local/share")
                    .join("hello")
                    .join("app.ron")
            )
        );
        assert_eq!(resolve(None, &[]), None);
    }
}
