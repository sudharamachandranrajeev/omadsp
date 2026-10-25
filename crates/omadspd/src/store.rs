//! Presets and state on disk.
//!
//! - User presets: `<config>/presets/*.json`, one per file. The file name is a slug of the preset
//!   name; the name inside the file is what counts. Files without the managed marker (hand-written
//!   ones) are loaded but never rewritten or deleted.
//! - State: `<state>/state.json`. A file that can't be read is moved aside to `state.json.bad`
//!   (unless it isn't ours) and the defaults are used.
//!
//! Nothing is created until the first save: a daemon that only starts and stops writes nothing.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::warn;

use omadsp_core::managed_file;
use omadsp_core::model::{Preset, State};

/// A user preset and the file it lives in.
#[derive(Clone, Debug, PartialEq)]
pub struct UserPreset {
    pub path: PathBuf,
    pub preset: Preset,
}

#[derive(Clone, Debug)]
pub struct Store {
    config_dir: PathBuf,
    state_dir: PathBuf,
}

/// `$XDG_CONFIG_HOME/omadsp` and `$XDG_STATE_HOME/omadsp`, with the usual `~` fallbacks.
pub fn default_dirs() -> Result<(PathBuf, PathBuf)> {
    let home = || std::env::var_os("HOME").map(PathBuf::from).context("HOME is not set");
    let xdg = |var: &str, fallback: &str| -> Result<PathBuf> {
        match std::env::var_os(var).filter(|v| !v.is_empty()) {
            Some(v) => Ok(PathBuf::from(v)),
            None => Ok(home()?.join(fallback)),
        }
    };
    Ok((xdg("XDG_CONFIG_HOME", ".config")?.join("omadsp"), xdg("XDG_STATE_HOME", ".local/state")?.join("omadsp")))
}

/// A file name for `name`: lowercase ASCII letters and digits, runs of anything else as one `-`.
pub fn slug(name: &str) -> String {
    let mut s = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.ends_with('-') {
            s.push('-');
        }
    }
    let s = s.trim_matches('-');
    if s.is_empty() { "preset".to_string() } else { s.to_string() }
}

fn to_json<T: serde::Serialize>(v: &T) -> String {
    managed_file::json_with_marker(&serde_json::to_value(v).expect("model types serialize"))
}

impl Store {
    pub fn new(config_dir: PathBuf, state_dir: PathBuf) -> Store {
        Store { config_dir, state_dir }
    }

    fn presets_dir(&self) -> PathBuf {
        self.config_dir.join("presets")
    }

    fn state_path(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }

    /// Every readable preset file, in file-name order. Broken files are skipped with a warning.
    pub fn load_presets(&self) -> Vec<UserPreset> {
        let dir = self.presets_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                warn!("can't read {}: {e}", dir.display());
                return Vec::new();
            }
        };
        let mut paths: Vec<PathBuf> =
            entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        paths.sort();
        let mut out = Vec::new();
        for path in paths {
            match fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|t| Preset::from_json(&t).map_err(|e| e.to_string()))
            {
                Ok(mut preset) => {
                    if preset.name.trim().is_empty() {
                        preset.name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                    }
                    out.push(UserPreset { path, preset });
                }
                Err(e) => warn!("skipping preset {}: {e}", path.display()),
            }
        }
        out
    }

    /// A free path for a new preset called `name`.
    pub fn new_preset_path(&self, name: &str, taken: &[UserPreset]) -> PathBuf {
        let base = slug(name);
        let dir = self.presets_dir();
        (1..)
            .map(|i| dir.join(if i == 1 { format!("{base}.json") } else { format!("{base}-{i}.json") }))
            .find(|p| !p.exists() && !taken.iter().any(|u| &u.path == p))
            .expect("some suffix is free")
    }

    pub fn write_preset(&self, path: &Path, p: &Preset) -> io::Result<()> {
        managed_file::write(path, &to_json(p), 0o644)
    }

    /// Deletes a preset file omadsp wrote; refuses anything else.
    pub fn delete_preset(&self, path: &Path) -> io::Result<()> {
        managed_file::check_writable(path)?;
        match fs::remove_file(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    }

    /// The saved state, or the first-run defaults.
    pub fn load_state(&self) -> State {
        let path = self.state_path();
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return State::default(),
            Err(e) => {
                warn!("can't read {}: {e}; starting from defaults", path.display());
                return State::default();
            }
        };
        match State::from_json(&text) {
            Ok(s) => s,
            Err(e) => {
                warn!("{} is unusable ({e}); starting from defaults", path.display());
                if managed_file::is_managed(&text) {
                    let bad = path.with_extension("json.bad");
                    if let Err(e) = fs::rename(&path, &bad) {
                        warn!("can't move it to {}: {e}", bad.display());
                    }
                }
                State::default()
            }
        }
    }

    pub fn save_state(&self, s: &State) -> io::Result<()> {
        managed_file::write(&self.state_path(), &to_json(s), 0o600)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn tmp_store(tag: &str) -> Store {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp")
            .join(format!("store-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        Store::new(d.join("config"), d.join("state"))
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("Warm V"), "warm-v");
        assert_eq!(slug("  Rock & Roll!! "), "rock-roll");
        assert_eq!(slug("ശബ്ദം"), "preset");
    }

    #[test]
    fn nothing_on_disk_means_defaults_and_no_files() {
        let st = tmp_store("empty");
        assert_eq!(st.load_state(), State::default());
        assert!(st.load_presets().is_empty());
        assert!(!st.config_dir.exists() && !st.state_dir.exists());
    }

    #[test]
    fn presets_and_state_round_trip() {
        let st = tmp_store("rt");
        let mut p = Preset { name: "Mine".into(), ..Preset::default() };
        p.eq.gains[3] = 4.5;
        let path = st.new_preset_path(&p.name, &[]);
        assert!(path.ends_with("presets/mine.json"));
        st.write_preset(&path, &p).unwrap();
        let loaded = st.load_presets();
        assert_eq!(loaded, vec![UserPreset { path: path.clone(), preset: p.clone() }]);
        assert!(st.new_preset_path("mine", &loaded).ends_with("mine-2.json"));

        let s = State { enabled: true, preset: "Mine".into(), working: p, ..State::default() };
        st.save_state(&s).unwrap();
        assert_eq!(st.load_state(), s);

        st.delete_preset(&path).unwrap();
        assert!(st.load_presets().is_empty());
    }

    #[test]
    fn foreign_files_are_loaded_but_never_touched() {
        let st = tmp_store("foreign");
        fs::create_dir_all(st.presets_dir()).unwrap();
        let path = st.presets_dir().join("hand.json");
        fs::write(&path, r#"{"name":"Hand","eq":{"gains":[1,2,3]}}"#).unwrap();
        fs::write(st.presets_dir().join("broken.json"), "{nope").unwrap();
        let loaded = st.load_presets();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].preset.name, "Hand");
        assert!(st.write_preset(&path, &loaded[0].preset).is_err());
        assert!(st.delete_preset(&path).is_err());
        assert!(path.exists());
    }

    #[test]
    fn unusable_state_is_moved_aside() {
        let st = tmp_store("bad");
        fs::create_dir_all(&st.state_dir).unwrap();
        let text = format!("{{\n  {},\n  \"schema\": 99\n}}\n", managed_file::JSON_MARKER);
        fs::write(st.state_path(), text).unwrap();
        assert_eq!(st.load_state(), State::default());
        assert!(!st.state_path().exists());
        assert!(st.state_dir.join("state.json.bad").exists());
    }
}
