//! What the user sees: the preset list, the working copy, on/off and A/B. Turns IPC commands into
//! state changes, persisting presets right away and leaving `state.json` to [`Session::flush`].
//!
//! Rules:
//! - Built-in presets are read-only. A locked preset can be applied and edited (the working copy
//!   becomes dirty) but not overwritten, renamed or deleted.
//! - Preset names are unique, case-insensitively, across built-in and user presets.
//! - Explicit values out of range are an `invalid_value` error; a preamp `delta` clamps instead
//!   (the bar's scroll wheel).
//! - `band.set` addresses the graphic bands in graphic mode and the parametric bands otherwise;
//!   `band.add|remove|reorder` always work on the parametric list.

use std::ops::RangeInclusive;

use omadsp_core::model::{self, EqMode, ParamBand, Preset, State};
use omadsp_core::protocol::{BandPatch, Command, ErrorBody, ErrorCode, PresetInfo, ShelfPatch};
use tracing::warn;

use crate::engine::Desired;
use crate::store::{Store, UserPreset};

/// Longest preset name.
pub const NAME_MAX: usize = 64;

/// What a handled command asks of the engine.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// Apply the new preset inside a dip (a preset switch or a structural change) rather than live.
    pub smooth: bool,
}

pub struct Session {
    store: Store,
    builtins: Vec<Preset>,
    user: Vec<UserPreset>,
    state: State,
    ab: bool,
    /// `state` differs from `state.json`.
    unsaved: bool,
}

fn err(code: ErrorCode, msg: impl Into<String>) -> ErrorBody {
    ErrorBody::new(code, msg)
}

fn check(name: &str, v: f64, r: &RangeInclusive<f64>) -> Result<f64, ErrorBody> {
    if r.contains(&v) {
        Ok(v)
    } else {
        Err(err(ErrorCode::InvalidValue, format!("{name} must be within {}…{}", r.start(), r.end())))
    }
}

fn check_sym(name: &str, v: f64, limit: f64) -> Result<f64, ErrorBody> {
    check(name, v, &(-limit..=limit))
}

fn io_err(what: &str, e: std::io::Error) -> ErrorBody {
    let code = if e.kind() == std::io::ErrorKind::PermissionDenied { ErrorCode::Locked } else { ErrorCode::Internal };
    err(code, format!("{what}: {e}"))
}

fn valid_name(name: &str) -> Result<String, ErrorBody> {
    let n = name.trim();
    if n.is_empty() || n.chars().count() > NAME_MAX || n.chars().any(char::is_control) {
        return Err(err(ErrorCode::InvalidValue, format!("preset names are 1–{NAME_MAX} printable characters")));
    }
    Ok(n.to_string())
}

/// Whether two presets sound the same (name and lock don't count).
fn same_sound(a: &Preset, b: &Preset) -> bool {
    let strip = |p: &Preset| Preset { name: String::new(), locked: false, ..p.clone() };
    strip(a) == strip(b)
}

/// Graphic gains for a new layout: the old gains interpolated linearly over log frequency.
fn resample(old_f: &[f64], old_g: &[f64], new_f: &[f64]) -> Vec<f64> {
    new_f
        .iter()
        .map(|&f| {
            let x = f.ln();
            let i = old_f.iter().rposition(|&o| o.ln() <= x);
            match i {
                None => old_g[0],
                Some(i) if i + 1 >= old_f.len() => old_g[old_f.len() - 1],
                Some(i) => {
                    let (x0, x1) = (old_f[i].ln(), old_f[i + 1].ln());
                    let t = (x - x0) / (x1 - x0);
                    let g = old_g[i] + t * (old_g[i + 1] - old_g[i]);
                    (g * 100.0).round() / 100.0
                }
            }
        })
        .collect()
}

fn check_band(b: &ParamBand) -> Result<(), ErrorBody> {
    check("f", b.f, &model::FREQ_HZ)?;
    check_sym("g", b.g, model::EQ_GAIN_DB)?;
    check("q", b.q, &model::Q_RANGE)?;
    Ok(())
}

fn patch_shelf(s: &mut model::Shelf, p: &ShelfPatch, freqs: &RangeInclusive<f64>) -> Result<(), ErrorBody> {
    if let Some(db) = p.db {
        s.db = check_sym("db", db, model::TONE_GAIN_DB)?;
    }
    if let Some(f) = p.f {
        s.f = check("f", f, freqs)?;
    }
    Ok(())
}

impl Session {
    /// Loads presets and state. A state whose preset no longer exists keeps its working copy,
    /// marked dirty.
    pub fn load(store: Store) -> Session {
        let builtins = model::builtin_presets();
        let mut user = store.load_presets();
        // Built-in names win; later duplicates of a user name are dropped.
        let mut seen: Vec<String> = builtins.iter().map(|p| p.name.to_lowercase()).collect();
        user.retain(|u| {
            let k = u.preset.name.to_lowercase();
            if seen.contains(&k) {
                warn!("ignoring {}: a preset called {:?} already exists", u.path.display(), u.preset.name);
                false
            } else {
                seen.push(k);
                true
            }
        });
        user.sort_by_key(|u| u.preset.name.to_lowercase());
        let state = store.load_state();
        let mut s = Session { store, builtins, user, state, ab: false, unsaved: false };
        s.state.dirty = s.compute_dirty();
        s
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn ab(&self) -> bool {
        self.ab
    }

    pub fn desired(&self) -> Desired {
        Desired { enabled: self.state.enabled, ab: self.ab, preset: self.state.working.clone() }
    }

    /// Built-ins first, then user presets by name: the `preset.step` order.
    pub fn presets(&self) -> Vec<PresetInfo> {
        let b = self.builtins.iter().map(|p| PresetInfo { name: p.name.clone(), builtin: true, locked: true });
        let u = self.user.iter().map(|u| PresetInfo {
            name: u.preset.name.clone(),
            builtin: false,
            locked: u.preset.locked,
        });
        b.chain(u).collect()
    }

    fn find(&self, name: &str) -> Option<&Preset> {
        let k = name.to_lowercase();
        self.builtins.iter().chain(self.user.iter().map(|u| &u.preset)).find(|p| p.name.to_lowercase() == k)
    }

    fn is_builtin(&self, name: &str) -> bool {
        self.builtins.iter().any(|p| p.name.eq_ignore_ascii_case(name))
    }

    fn user_index(&self, name: &str) -> Option<usize> {
        let k = name.to_lowercase();
        self.user.iter().position(|u| u.preset.name.to_lowercase() == k)
    }

    fn compute_dirty(&self) -> bool {
        self.find(&self.state.preset).is_none_or(|p| !same_sound(p, &self.state.working))
    }

    /// A user preset that may be changed on disk, by index.
    fn writable(&self, name: &str) -> Result<usize, ErrorBody> {
        if self.is_builtin(name) {
            return Err(err(ErrorCode::Locked, format!("{name:?} is built in")));
        }
        let i = self.user_index(name).ok_or_else(|| err(ErrorCode::NotFound, format!("no preset {name:?}")))?;
        if self.user[i].preset.locked {
            return Err(err(ErrorCode::Locked, format!("{name:?} is locked")));
        }
        Ok(i)
    }

    fn sort_user(&mut self) {
        self.user.sort_by_key(|u| u.preset.name.to_lowercase());
    }

    fn load_preset(&mut self, p: Preset) {
        self.state.preset = p.name.clone();
        self.state.working = Preset { locked: false, ..p };
    }

    /// Handles everything except `state.get` and `meter.subscribe`, which belong to the server.
    pub fn handle(&mut self, cmd: Command) -> Result<Outcome, ErrorBody> {
        let before = self.state.clone();
        let mut smooth = false;
        let w = &mut self.state.working;
        match cmd {
            Command::StateGet | Command::MeterSubscribe { .. } => {}
            Command::DspSetEnabled { enabled } => self.state.enabled = enabled,
            Command::DspToggle => self.state.enabled = !self.state.enabled,
            Command::DspAbHold { hold } => self.ab = hold,
            Command::PresetApply { name } => {
                let p =
                    self.find(&name).cloned().ok_or_else(|| err(ErrorCode::NotFound, format!("no preset {name:?}")))?;
                self.load_preset(p);
                smooth = true;
            }
            Command::PresetStep { delta } => {
                let list = self.presets();
                let n = list.len() as i64;
                let cur = list.iter().position(|p| p.name.eq_ignore_ascii_case(&self.state.preset));
                let next = match cur {
                    Some(i) => (i as i64 + i64::from(delta)).rem_euclid(n),
                    None if delta >= 0 => 0,
                    None => n - 1,
                };
                let p = self.find(&list[next as usize].name).cloned().expect("listed presets exist");
                self.load_preset(p);
                smooth = true;
            }
            Command::PresetSave { name } => {
                let name = valid_name(name.as_deref().unwrap_or(&self.state.preset))?;
                let mut p = Preset { name: name.clone(), locked: false, ..self.state.working.clone() };
                let i = match self.user_index(&name) {
                    Some(_) if !name.eq_ignore_ascii_case(&self.state.preset) => {
                        return Err(err(ErrorCode::Exists, format!("a preset called {name:?} already exists")));
                    }
                    Some(_) => Some(self.writable(&name)?),
                    None if self.is_builtin(&name) => {
                        return Err(err(ErrorCode::Locked, format!("{name:?} is built in")));
                    }
                    None => None,
                };
                let path = match i {
                    Some(i) => {
                        p.name = self.user[i].preset.name.clone();
                        self.user[i].path.clone()
                    }
                    None => self.store.new_preset_path(&name, &self.user),
                };
                self.store.write_preset(&path, &p).map_err(|e| io_err("saving the preset", e))?;
                match i {
                    Some(i) => self.user[i].preset = p.clone(),
                    None => self.user.push(UserPreset { path, preset: p.clone() }),
                }
                self.sort_user();
                self.state.preset = p.name;
            }
            Command::PresetRename { from, to } => {
                let to = valid_name(&to)?;
                let i = self.writable(&from)?;
                if self.find(&to).is_some_and(|p| !p.name.eq_ignore_ascii_case(&from)) {
                    return Err(err(ErrorCode::Exists, format!("a preset called {to:?} already exists")));
                }
                let old = self.user[i].clone();
                let p = Preset { name: to.clone(), ..old.preset.clone() };
                let others: Vec<UserPreset> = self.user.iter().filter(|u| u.path != old.path).cloned().collect();
                let path = self.store.new_preset_path(&to, &others);
                let path = if path == old.path { old.path.clone() } else { path };
                self.store.write_preset(&path, &p).map_err(|e| io_err("renaming the preset", e))?;
                if path != old.path
                    && let Err(e) = self.store.delete_preset(&old.path)
                {
                    warn!("renamed, but can't remove {}: {e}", old.path.display());
                }
                self.user[i] = UserPreset { path, preset: p };
                self.sort_user();
                if self.state.preset.eq_ignore_ascii_case(&from) {
                    self.state.preset = to.clone();
                    self.state.working.name = to;
                }
            }
            Command::PresetDelete { name } => {
                let i = self.writable(&name)?;
                self.store.delete_preset(&self.user[i].path).map_err(|e| io_err("deleting the preset", e))?;
                self.user.remove(i);
            }
            Command::PresetLock { name, locked } => {
                if self.is_builtin(&name) {
                    return Err(err(ErrorCode::Locked, format!("{name:?} is built in")));
                }
                let i =
                    self.user_index(&name).ok_or_else(|| err(ErrorCode::NotFound, format!("no preset {name:?}")))?;
                let p = Preset { locked, ..self.user[i].preset.clone() };
                self.store.write_preset(&self.user[i].path, &p).map_err(|e| io_err("locking the preset", e))?;
                self.user[i].preset = p;
            }
            Command::EqSetMode { mode } => {
                smooth = w.eq.mode != mode;
                w.eq.mode = mode;
            }
            Command::EqSetLayout { bands, range } => {
                let bands = bands.unwrap_or(w.eq.bands);
                if !model::GRAPHIC_BANDS.contains(&bands) {
                    let r = &model::GRAPHIC_BANDS;
                    return Err(err(
                        ErrorCode::InvalidValue,
                        format!("bands must be within {}…{}", r.start(), r.end()),
                    ));
                }
                let range = range.unwrap_or(w.eq.range);
                check("range", range[0], &model::FREQ_HZ)?;
                check("range", range[1], &model::FREQ_HZ)?;
                if range[1] / range[0] < 2.0 {
                    return Err(err(ErrorCode::InvalidValue, "the range must span at least an octave"));
                }
                if bands != w.eq.bands || range != w.eq.range {
                    let new_f = model::graphic_freqs(bands, range);
                    w.eq.gains = resample(&w.graphic_freqs(), &w.eq.gains, &new_f);
                    w.eq.bands = bands;
                    w.eq.range = range;
                    smooth = true;
                }
            }
            Command::EqReset => {
                match w.eq.mode {
                    EqMode::Graphic => w.eq.gains.iter_mut().for_each(|g| *g = 0.0),
                    EqMode::Parametric => w.parametric.clear(),
                }
                smooth = true;
            }
            Command::BandSet { index, patch } => match w.eq.mode {
                EqMode::Graphic => {
                    let BandPatch { g: Some(g), kind: None, ch: None, f: None, q: None, color: None, on: None } = patch
                    else {
                        return Err(err(ErrorCode::InvalidValue, "graphic mode: a band only has a gain (g)"));
                    };
                    let n = w.eq.gains.len();
                    let slot =
                        w.eq.gains
                            .get_mut(index)
                            .ok_or_else(|| err(ErrorCode::NotFound, format!("band {index} of {n}")))?;
                    *slot = check_sym("g", g, model::EQ_GAIN_DB)?;
                }
                EqMode::Parametric => {
                    let n = w.parametric.len();
                    let b = w
                        .parametric
                        .get_mut(index)
                        .ok_or_else(|| err(ErrorCode::NotFound, format!("band {index} of {n}")))?;
                    let mut nb = b.clone();
                    if let Some(k) = patch.kind {
                        nb.kind = k;
                    }
                    if let Some(c) = patch.ch {
                        nb.ch = c;
                    }
                    nb.f = patch.f.unwrap_or(nb.f);
                    nb.g = patch.g.unwrap_or(nb.g);
                    nb.q = patch.q.unwrap_or(nb.q);
                    if let Some(c) = patch.color {
                        nb.color = Some(c).filter(|c| !c.is_empty());
                    }
                    nb.on = patch.on.unwrap_or(nb.on);
                    check_band(&nb)?;
                    *b = nb;
                }
            },
            Command::BandAdd { band, index } => {
                if w.parametric.len() >= model::MAX_PARAMETRIC_BANDS {
                    return Err(err(ErrorCode::InvalidValue, format!("at most {} bands", model::MAX_PARAMETRIC_BANDS)));
                }
                check_band(&band)?;
                let i = index.unwrap_or(w.parametric.len());
                if i > w.parametric.len() {
                    return Err(err(ErrorCode::InvalidValue, format!("index {i} is past the end")));
                }
                w.parametric.insert(i, band);
                smooth = true;
            }
            Command::BandRemove { index } => {
                if index >= w.parametric.len() {
                    return Err(err(ErrorCode::NotFound, format!("band {index} of {}", w.parametric.len())));
                }
                w.parametric.remove(index);
                smooth = true;
            }
            Command::BandReorder { from, to } => {
                let n = w.parametric.len();
                if from >= n || to >= n {
                    return Err(err(ErrorCode::NotFound, format!("bands {from} and {to} of {n}")));
                }
                let b = w.parametric.remove(from);
                w.parametric.insert(to, b);
                smooth = true;
            }
            Command::ToneSet { bass, treble } => {
                let mut t = w.tone.clone();
                if let Some(p) = &bass {
                    patch_shelf(&mut t.bass, p, &model::BASS_FREQ_HZ)?;
                }
                if let Some(p) = &treble {
                    patch_shelf(&mut t.treble, p, &model::TREBLE_FREQ_HZ)?;
                }
                w.tone = t;
            }
            Command::PreampSet { db, delta, auto } => {
                let mut p = w.preamp.clone();
                if let Some(db) = db {
                    p.db = check("db", db, &model::PREAMP_DB)?;
                }
                if let Some(d) = delta {
                    p.db = model::clamp(((p.db + d) * 100.0).round() / 100.0, &model::PREAMP_DB, p.db);
                }
                p.auto = auto.unwrap_or(p.auto);
                w.preamp = p;
            }
            Command::LimiterSet { ceiling_dbtp, release_ms } => {
                let mut l = w.limiter.clone();
                if let Some(c) = ceiling_dbtp {
                    l.ceiling_dbtp = check("ceiling_dbtp", c, &model::CEILING_DBTP)?;
                }
                if let Some(r) = release_ms {
                    l.release_ms = check("release_ms", r, &model::RELEASE_MS)?;
                }
                w.limiter = l;
            }
        }
        self.state.working.sanitize();
        self.state.dirty = self.compute_dirty();
        if self.state != before {
            self.unsaved = true;
        }
        Ok(Outcome { smooth })
    }

    /// Whether `state.json` is behind.
    pub fn unsaved(&self) -> bool {
        self.unsaved
    }

    /// Writes `state.json` if it's behind.
    pub fn flush(&mut self) {
        if !self.unsaved {
            return;
        }
        match self.store.save_state(&self.state) {
            Ok(()) => self.unsaved = false,
            Err(e) => {
                warn!("can't save the state: {e}");
                // Don't retry in a loop; the next change tries again.
                self.unsaved = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::tmp_store;
    use omadsp_core::model::{Channel, FLAT, FilterKind};
    use omadsp_core::protocol::Request;
    use serde_json::{Value, json};

    fn run(s: &mut Session, kind: &str, payload: Value) -> Result<Outcome, ErrorBody> {
        s.handle(Command::parse(&Request::new("t", kind, payload))?)
    }

    fn code(r: Result<Outcome, ErrorBody>) -> ErrorCode {
        r.expect_err("expected an error").code
    }

    #[test]
    fn first_run_is_off_and_flat_and_writes_nothing() {
        let mut s = Session::load(tmp_store("s-first"));
        assert!(!s.state().enabled);
        assert_eq!(s.state().preset, FLAT);
        assert!(!s.state().dirty);
        assert!(!s.unsaved());
        s.flush();
        assert!(Session::load(s.store.clone()).state() == s.state());
    }

    #[test]
    fn edits_mark_dirty_and_persist() {
        let st = tmp_store("s-dirty");
        let mut s = Session::load(st.clone());
        run(&mut s, "dsp.toggle", Value::Null).unwrap();
        run(&mut s, "band.set", json!({"index": 2, "g": 3.5})).unwrap();
        assert!(s.state().enabled && s.state().dirty);
        run(&mut s, "band.set", json!({"index": 2, "g": 0.0})).unwrap();
        assert!(!s.state().dirty, "back to the saved sound");
        run(&mut s, "band.set", json!({"index": 2, "g": 3.5})).unwrap();
        assert!(s.unsaved());
        s.flush();
        let again = Session::load(st);
        assert_eq!(again.state(), s.state());
        assert!(again.state().dirty);
    }

    #[test]
    fn range_and_mode_errors() {
        let mut s = Session::load(tmp_store("s-err"));
        assert_eq!(code(run(&mut s, "band.set", json!({"index": 0, "g": 20.0}))), ErrorCode::InvalidValue);
        assert_eq!(code(run(&mut s, "band.set", json!({"index": 99, "g": 1.0}))), ErrorCode::NotFound);
        assert_eq!(code(run(&mut s, "band.set", json!({"index": 0, "f": 100.0}))), ErrorCode::InvalidValue);
        assert_eq!(code(run(&mut s, "preamp.set", json!({"db": 30.0}))), ErrorCode::InvalidValue);
        assert_eq!(code(run(&mut s, "eq.setLayout", json!({"bands": 64}))), ErrorCode::InvalidValue);
        assert_eq!(code(run(&mut s, "preset.apply", json!({"name": "Nope"}))), ErrorCode::NotFound);
        assert_eq!(s.state(), &State::default(), "failed commands change nothing");
        run(&mut s, "preamp.set", json!({"delta": -100.0})).unwrap();
        assert_eq!(s.state().working.preamp.db, *model::PREAMP_DB.start(), "delta clamps");
    }

    #[test]
    fn presets_save_step_rename_lock_delete() {
        let st = tmp_store("s-presets");
        let mut s = Session::load(st.clone());
        assert_eq!(code(run(&mut s, "preset.save", Value::Null)), ErrorCode::Locked, "Flat is built in");
        run(&mut s, "band.set", json!({"index": 0, "g": 2.0})).unwrap();
        run(&mut s, "preset.save", json!({"name": "Mine"})).unwrap();
        assert_eq!((s.state().preset.as_str(), s.state().dirty), ("Mine", false));
        run(&mut s, "preset.save", json!({"name": "MINE "})).expect("saving under the current name overwrites");
        assert_eq!(s.state().preset, "Mine", "the stored name keeps its case");
        run(&mut s, "preset.apply", json!({"name": "Vocal"})).unwrap();
        assert_eq!(code(run(&mut s, "preset.save", json!({"name": "mine"}))), ErrorCode::Exists);
        run(&mut s, "preset.apply", json!({"name": "mine"})).unwrap();

        // step: Flat, Bass Boost, Warm V, Vocal, Bright, Mine → wraps to Flat
        run(&mut s, "preset.step", json!({"delta": 1})).unwrap();
        assert_eq!(s.state().preset, FLAT);
        run(&mut s, "preset.step", json!({"delta": -1})).unwrap();
        assert_eq!(s.state().preset, "Mine");
        assert_eq!(s.state().working.eq.gains[0], 2.0);

        assert_eq!(code(run(&mut s, "preset.rename", json!({"from": "Mine", "to": "vocal"}))), ErrorCode::Exists);
        run(&mut s, "preset.rename", json!({"from": "Mine", "to": "Ours"})).unwrap();
        assert_eq!(s.state().preset, "Ours");
        run(&mut s, "preset.lock", json!({"name": "Ours", "locked": true})).unwrap();
        assert_eq!(code(run(&mut s, "preset.delete", json!({"name": "Ours"}))), ErrorCode::Locked);
        run(&mut s, "band.set", json!({"index": 0, "g": 5.0})).unwrap();
        assert_eq!(code(run(&mut s, "preset.save", Value::Null)), ErrorCode::Locked);

        let disk = Session::load(st.clone());
        let info = disk.presets().into_iter().find(|p| p.name == "Ours").unwrap();
        assert!(info.locked && !info.builtin);
        assert_eq!(st.load_presets().len(), 1, "the rename removed the old file");

        run(&mut s, "preset.lock", json!({"name": "Ours", "locked": false})).unwrap();
        run(&mut s, "preset.delete", json!({"name": "Ours"})).unwrap();
        assert!(st.load_presets().is_empty());
        assert!(s.state().dirty, "the working copy survives, unsaved");
    }

    #[test]
    fn layout_change_resamples_gains() {
        let mut s = Session::load(tmp_store("s-layout"));
        for i in 0..10 {
            run(&mut s, "band.set", json!({"index": i, "g": i as f64})).unwrap();
        }
        let out = run(&mut s, "eq.setLayout", json!({"bands": 19})).unwrap();
        assert!(out.smooth);
        let g = &s.state().working.eq.gains;
        assert_eq!(g.len(), 19);
        assert_eq!((g[0], g[2], g[18]), (0.0, 1.0, 9.0), "old band centres keep their gains");
        assert!((g[1] - 0.5).abs() < 0.01);
    }

    #[test]
    fn parametric_bands() {
        let mut s = Session::load(tmp_store("s-param"));
        run(&mut s, "eq.setMode", json!({"mode": "parametric"})).unwrap();
        let band = json!({"type": "lowshelf", "f": 100.0, "g": 3.0, "q": 0.7, "ch": "l", "on": true});
        run(&mut s, "band.add", json!({"band": band})).unwrap();
        run(&mut s, "band.add", json!({"band": {"f": 3000.0, "g": -2.0, "q": 2.0, "on": true}, "index": 0})).unwrap();
        run(&mut s, "band.set", json!({"index": 1, "f": 120.0, "color": "#ff0000"})).unwrap();
        assert_eq!(code(run(&mut s, "band.set", json!({"index": 1, "q": 50.0}))), ErrorCode::InvalidValue);
        let p = &s.state().working.parametric;
        assert_eq!((p[0].f, p[1].kind, p[1].ch, p[1].f), (3000.0, FilterKind::LowShelf, Channel::Left, 120.0));
        run(&mut s, "band.reorder", json!({"from": 1, "to": 0})).unwrap();
        assert_eq!(s.state().working.parametric[0].f, 120.0);
        run(&mut s, "band.remove", json!({"index": 0})).unwrap();
        assert_eq!(s.state().working.parametric.len(), 1);
        run(&mut s, "eq.reset", Value::Null).unwrap();
        assert!(s.state().working.parametric.is_empty());
    }

    #[test]
    fn missing_preset_keeps_working_copy_dirty() {
        let st = tmp_store("s-missing");
        let mut working = Preset::default();
        working.eq.gains[1] = 1.0;
        st.save_state(&State { preset: "Gone".into(), working: working.clone(), ..State::default() }).unwrap();
        let s = Session::load(st);
        assert_eq!(s.state().working, working);
        assert!(s.state().dirty);
    }
}
