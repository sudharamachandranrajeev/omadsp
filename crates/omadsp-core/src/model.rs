//! Presets, device profiles and daemon state: everything omadsp stores on disk.
//!
//! Field names follow PROMPT §8.5 so preset files stay readable and hand-editable. Every
//! struct reads with defaults for missing fields, and unknown fields are ignored (the
//! managed-file marker `"//"` among them). After loading, [`Preset::sanitize`] clamps every
//! value into the range the DSP engine supports.

use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};

/// Current preset file schema. Bump it and add a migration in [`Preset::from_json`] on any
/// incompatible change.
pub const PRESET_SCHEMA: u32 = 1;
/// Current `state.json` schema.
pub const STATE_SCHEMA: u32 = 1;
/// Current `profiles.json` schema.
pub const PROFILES_SCHEMA: u32 = 1;

/// Name of the preset a first run starts with.
pub const FLAT: &str = "Flat";

/// Graphic EQ band counts the user can choose.
pub const GRAPHIC_BANDS: RangeInclusive<usize> = 5..=32;
/// Parametric bands per preset. Band `i` always uses slot `i` of the 32-band EQ stage on the
/// channels it applies to, so editing one band never moves another.
pub const MAX_PARAMETRIC_BANDS: usize = 32;
/// User EQ gain limit, both modes (±dB).
pub const EQ_GAIN_DB: f64 = 15.0;
/// Frequencies the user can set (Hz). The EQ stage itself accepts 10 Hz…24 kHz.
pub const FREQ_HZ: RangeInclusive<f64> = 20.0..=20000.0;
/// Filter Q the user can set.
pub const Q_RANGE: RangeInclusive<f64> = 0.1..=30.0;
/// Bass/treble shelf gain limit (±dB).
pub const TONE_GAIN_DB: f64 = 12.0;
/// Bass shelf corner frequency (Hz).
pub const BASS_FREQ_HZ: RangeInclusive<f64> = 20.0..=500.0;
/// Treble shelf corner frequency (Hz).
pub const TREBLE_FREQ_HZ: RangeInclusive<f64> = 1000.0..=20000.0;
/// Preamp (manual value, or the offset added to auto headroom) in dB.
pub const PREAMP_DB: RangeInclusive<f64> = -24.0..=12.0;
/// Limiter ceiling (dBTP).
pub const CEILING_DBTP: RangeInclusive<f64> = -12.0..=0.0;
/// Limiter release (ms). This is the LSP limiter's own `rt` range, so PROMPT §8.5's 50 ms
/// example can't be honoured.
pub const RELEASE_MS: RangeInclusive<f64> = 0.25..=20.0;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preset {
    pub schema: u32,
    pub name: String,
    pub locked: bool,
    pub eq: Eq,
    pub parametric: Vec<ParamBand>,
    pub preamp: Preamp,
    pub tone: Tone,
    // Stored so presets round-trip; applied from Phase 4 on.
    pub stereo: Stereo,
    pub crossfeed: Crossfeed,
    pub spatial: Spatial,
    pub reverb: Reverb,
    pub leveler: Leveler,
    pub limiter: Limiter,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EqMode {
    #[default]
    Graphic,
    Parametric,
}

/// The graphic EQ: `bands` log-spaced peaking filters from `range[0]` to `range[1]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Eq {
    pub mode: EqMode,
    pub bands: usize,
    pub range: [f64; 2],
    pub gains: Vec<f64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FilterKind {
    #[default]
    Peak,
    LowShelf,
    HighShelf,
    LowPass,
    HighPass,
    BandPass,
    Notch,
}

impl FilterKind {
    /// Whether the band's gain affects its response.
    pub fn uses_gain(self) -> bool {
        matches!(self, FilterKind::Peak | FilterKind::LowShelf | FilterKind::HighShelf)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    #[default]
    Both,
    #[serde(alias = "l")]
    Left,
    #[serde(alias = "r")]
    Right,
}

impl Channel {
    pub fn left(self) -> bool {
        self != Channel::Right
    }
    pub fn right(self) -> bool {
        self != Channel::Left
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ParamBand {
    #[serde(rename = "type")]
    pub kind: FilterKind,
    pub ch: Channel,
    pub f: f64,
    pub g: f64,
    pub q: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub on: bool,
}

/// Input gain. The applied value is `db`, plus −(largest boost of the combined curve) when
/// `auto` is on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preamp {
    pub db: f64,
    pub auto: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Shelf {
    pub db: f64,
    pub f: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tone {
    pub bass: Shelf,
    pub treble: Shelf,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stereo {
    pub balance: f64,
    pub width: f64,
    pub mono: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CrossfeedMode {
    #[default]
    Off,
    Low,
    High,
    Custom,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Crossfeed {
    pub mode: CrossfeedMode,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpatialMode {
    #[default]
    Off,
    Externalise,
    #[serde(rename = "virtual71")]
    Virtual71,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Spatial {
    pub mode: SpatialMode,
    pub hrtf: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Reverb {
    pub ir: Option<String>,
    pub wet: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Leveler {
    pub on: bool,
    pub target_lufs: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limiter {
    pub ceiling_dbtp: f64,
    pub release_ms: f64,
}

impl Default for Preset {
    fn default() -> Self {
        Preset {
            schema: PRESET_SCHEMA,
            name: FLAT.to_string(),
            locked: false,
            eq: Eq::default(),
            parametric: Vec::new(),
            preamp: Preamp::default(),
            tone: Tone::default(),
            stereo: Stereo::default(),
            crossfeed: Crossfeed::default(),
            spatial: Spatial::default(),
            reverb: Reverb::default(),
            leveler: Leveler::default(),
            limiter: Limiter::default(),
        }
    }
}

impl Default for Eq {
    fn default() -> Self {
        Eq { mode: EqMode::Graphic, bands: 10, range: [31.0, 16000.0], gains: vec![0.0; 10] }
    }
}

impl Default for ParamBand {
    fn default() -> Self {
        ParamBand { kind: FilterKind::Peak, ch: Channel::Both, f: 1000.0, g: 0.0, q: 1.0, color: None, on: true }
    }
}

impl Default for Preamp {
    fn default() -> Self {
        Preamp { db: 0.0, auto: true }
    }
}

impl Default for Tone {
    fn default() -> Self {
        Tone { bass: Shelf { db: 0.0, f: 105.0 }, treble: Shelf { db: 0.0, f: 8000.0 } }
    }
}

impl Default for Shelf {
    fn default() -> Self {
        Shelf { db: 0.0, f: 1000.0 }
    }
}

impl Default for Stereo {
    fn default() -> Self {
        Stereo { balance: 0.0, width: 1.0, mono: false }
    }
}

impl Default for Leveler {
    fn default() -> Self {
        Leveler { on: false, target_lufs: -18.0 }
    }
}

impl Default for Limiter {
    fn default() -> Self {
        Limiter { ceiling_dbtp: -1.0, release_ms: 5.0 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModelError {
    Json(String),
    NewerSchema { found: u32, supported: u32 },
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModelError::Json(e) => write!(f, "invalid JSON: {e}"),
            ModelError::NewerSchema { found, supported } => {
                write!(f, "schema {found} is newer than this omadsp supports ({supported})")
            }
        }
    }
}

impl std::error::Error for ModelError {}

/// Clamps `v` into `r`; NaN becomes `fallback`.
pub fn clamp(v: f64, r: &RangeInclusive<f64>, fallback: f64) -> f64 {
    if v.is_nan() { fallback } else { v.clamp(*r.start(), *r.end()) }
}

fn clamp_sym(v: f64, limit: f64) -> f64 {
    clamp(v, &(-limit..=limit), 0.0)
}

fn schema_of(v: &serde_json::Value) -> u32 {
    v.get("schema").and_then(|s| s.as_u64()).map_or(1, |s| s as u32)
}

impl Preset {
    /// Parses a preset file, migrating older schemas, and sanitizes it.
    pub fn from_json(text: &str) -> Result<Preset, ModelError> {
        let v: serde_json::Value = serde_json::from_str(text).map_err(|e| ModelError::Json(e.to_string()))?;
        let schema = schema_of(&v);
        if schema > PRESET_SCHEMA {
            return Err(ModelError::NewerSchema { found: schema, supported: PRESET_SCHEMA });
        }
        // Schema 1 is the first; migrations of older layouts go here.
        let mut p: Preset = serde_json::from_value(v).map_err(|e| ModelError::Json(e.to_string()))?;
        p.schema = PRESET_SCHEMA;
        p.sanitize();
        Ok(p)
    }

    /// Clamps every value into the range the engine supports and repairs structural problems
    /// (gain list length, too many parametric bands). Idempotent.
    pub fn sanitize(&mut self) {
        let eq = &mut self.eq;
        eq.bands = eq.bands.clamp(*GRAPHIC_BANDS.start(), *GRAPHIC_BANDS.end());
        let lo = clamp(eq.range[0], &FREQ_HZ, 31.0);
        let hi = clamp(eq.range[1], &FREQ_HZ, 16000.0);
        eq.range = if hi / lo >= 2.0 { [lo, hi] } else { [31.0, 16000.0] };
        eq.gains.resize(eq.bands, 0.0);
        for g in &mut eq.gains {
            *g = clamp_sym(*g, EQ_GAIN_DB);
        }

        self.parametric.truncate(MAX_PARAMETRIC_BANDS);
        for b in &mut self.parametric {
            b.f = clamp(b.f, &FREQ_HZ, 1000.0);
            b.g = clamp_sym(b.g, EQ_GAIN_DB);
            b.q = clamp(b.q, &Q_RANGE, 1.0);
        }

        self.preamp.db = clamp(self.preamp.db, &PREAMP_DB, 0.0);
        let t = &mut self.tone;
        t.bass.db = clamp_sym(t.bass.db, TONE_GAIN_DB);
        t.bass.f = clamp(t.bass.f, &BASS_FREQ_HZ, 105.0);
        t.treble.db = clamp_sym(t.treble.db, TONE_GAIN_DB);
        t.treble.f = clamp(t.treble.f, &TREBLE_FREQ_HZ, 8000.0);

        self.stereo.balance = clamp(self.stereo.balance, &(-1.0..=1.0), 0.0);
        self.stereo.width = clamp(self.stereo.width, &(0.0..=2.0), 1.0);
        self.reverb.wet = clamp(self.reverb.wet, &(0.0..=1.0), 0.0);
        self.leveler.target_lufs = clamp(self.leveler.target_lufs, &(-40.0..=-6.0), -18.0);

        self.limiter.ceiling_dbtp = clamp(self.limiter.ceiling_dbtp, &CEILING_DBTP, -1.0);
        self.limiter.release_ms = clamp(self.limiter.release_ms, &RELEASE_MS, 5.0);
    }

    /// Centre frequencies of the graphic EQ bands: log-spaced, both ends included.
    pub fn graphic_freqs(&self) -> Vec<f64> {
        graphic_freqs(self.eq.bands, self.eq.range)
    }

    /// Q of every graphic band: each band spans the distance to its neighbours.
    pub fn graphic_q(&self) -> f64 {
        graphic_q(self.eq.bands, self.eq.range)
    }

    /// Whether every stage is neutral (the EQ stages could be bypassed bit-exactly).
    pub fn is_flat(&self) -> bool {
        let user_eq_flat = match self.eq.mode {
            EqMode::Graphic => self.eq.gains.iter().all(|&g| g == 0.0),
            EqMode::Parametric => self.parametric.iter().all(|b| !b.on || (b.kind.uses_gain() && b.g == 0.0)),
        };
        user_eq_flat && self.tone.bass.db == 0.0 && self.tone.treble.db == 0.0
    }
}

pub fn graphic_freqs(bands: usize, range: [f64; 2]) -> Vec<f64> {
    let n = bands.max(2);
    let (lo, hi) = (range[0].ln(), range[1].ln());
    (0..n).map(|i| (lo + (hi - lo) * i as f64 / (n - 1) as f64).exp()).collect()
}

/// Q for a band `b` octaves wide, where `b` is the spacing between neighbouring bands.
pub fn graphic_q(bands: usize, range: [f64; 2]) -> f64 {
    let octaves = (range[1] / range[0]).log2() / (bands.max(2) - 1) as f64;
    let k = 2f64.powf(octaves);
    k.sqrt() / (k - 1.0)
}

fn graphic(name: &str, gains: [f64; 10]) -> Preset {
    Preset {
        name: name.to_string(),
        locked: true,
        eq: Eq { gains: gains.to_vec(), ..Eq::default() },
        ..Preset::default()
    }
}

/// Presets compiled into omadsp. They are read-only; saving one under its own name is refused.
pub fn builtin_presets() -> Vec<Preset> {
    vec![
        Preset { locked: true, ..Preset::default() },
        graphic("Bass Boost", [6.0, 5.0, 3.5, 1.5, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
        graphic("Warm V", [3.0, 2.0, 1.0, 0.0, -1.0, -1.0, 0.0, 1.0, 2.0, 3.0]),
        graphic("Vocal", [-2.0, -1.0, 0.0, 1.0, 3.0, 3.0, 2.0, 1.0, 0.0, -1.0]),
        graphic("Bright", [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 4.0]),
    ]
}

/// The daemon's persistent state (`~/.local/state/omadsp/state.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub schema: u32,
    /// DSP on/off as the user last set it. A first run starts off.
    pub enabled: bool,
    /// The preset the working copy was loaded from.
    pub preset: String,
    /// Live values, including edits not yet saved to a preset.
    pub working: Preset,
    /// Whether `working` differs from the saved preset.
    pub dirty: bool,
}

impl Default for State {
    fn default() -> Self {
        State {
            schema: STATE_SCHEMA,
            enabled: false,
            preset: FLAT.to_string(),
            working: Preset::default(),
            dirty: false,
        }
    }
}

impl State {
    pub fn from_json(text: &str) -> Result<State, ModelError> {
        let v: serde_json::Value = serde_json::from_str(text).map_err(|e| ModelError::Json(e.to_string()))?;
        let schema = schema_of(&v);
        if schema > STATE_SCHEMA {
            return Err(ModelError::NewerSchema { found: schema, supported: STATE_SCHEMA });
        }
        let mut s: State = serde_json::from_value(v).map_err(|e| ModelError::Json(e.to_string()))?;
        s.schema = STATE_SCHEMA;
        s.working.sanitize();
        Ok(s)
    }
}

/// A device → preset assignment (`~/.config/omadsp/profiles.json`). Used from Phase 3 on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub device_key: String,
    pub label: String,
    /// `autoeq:<id>`, or none.
    pub correction: Option<String>,
    pub preset: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profiles {
    pub schema: u32,
    pub profiles: Vec<Profile>,
}

impl Default for Profiles {
    fn default() -> Self {
        Profiles { schema: PROFILES_SCHEMA, profiles: Vec::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_example_parses() {
        // PROMPT §8.5, verbatim.
        let text = r##"{
          "schema": 1,
          "name": "Warm V",
          "locked": false,
          "eq": { "mode": "graphic", "bands": 10, "range": [31, 16000], "gains": [3,2,1,0,-1,-1,0,1,2,3] },
          "parametric": [ { "type": "peak", "ch": "both", "f": 120, "g": 2.5, "q": 0.9, "color": "#e07a5f", "on": true } ],
          "preamp": { "db": -3.0, "auto": true },
          "tone": { "bass": { "db": 2, "f": 105 }, "treble": { "db": 1, "f": 8000 } },
          "stereo": { "balance": 0, "width": 1.0, "mono": false },
          "crossfeed": { "mode": "off" },
          "spatial": { "mode": "off", "hrtf": null },
          "reverb": { "ir": null, "wet": 0 },
          "leveler": { "on": false, "target_lufs": -18 },
          "limiter": { "ceiling_dbtp": -1.0, "release_ms": 50 }
        }"##;
        let p = Preset::from_json(text).unwrap();
        assert_eq!(p.name, "Warm V");
        assert_eq!(p.eq.gains, vec![3.0, 2.0, 1.0, 0.0, -1.0, -1.0, 0.0, 1.0, 2.0, 3.0]);
        assert_eq!(p.parametric[0].kind, FilterKind::Peak);
        assert_eq!(p.parametric[0].color.as_deref(), Some("#e07a5f"));
        assert_eq!(p.tone.bass, Shelf { db: 2.0, f: 105.0 });
        // 50 ms is beyond the LSP limiter's release range.
        assert_eq!(p.limiter.release_ms, 20.0);
    }

    #[test]
    fn missing_fields_take_defaults_and_marker_is_ignored() {
        let p = Preset::from_json(r#"{ "//": "Managed by omadsp — do not edit", "name": "X" }"#).unwrap();
        assert_eq!(p.name, "X");
        assert_eq!(p.eq, Eq::default());
        assert_eq!(p.limiter, Limiter::default());
        assert!(p.preamp.auto);
    }

    #[test]
    fn newer_schema_is_refused() {
        let e = Preset::from_json(r#"{ "schema": 2 }"#).unwrap_err();
        assert_eq!(e, ModelError::NewerSchema { found: 2, supported: 1 });
    }

    #[test]
    fn sanitize_clamps_and_repairs() {
        let mut p = Preset::default();
        p.eq.bands = 40;
        p.eq.gains = vec![99.0, f64::NAN, -99.0];
        p.eq.range = [1000.0, 1200.0];
        p.parametric = vec![ParamBand { f: 5.0, q: 0.0, g: -40.0, ..ParamBand::default() }; 40];
        p.tone.treble.f = 200.0;
        p.limiter.ceiling_dbtp = 3.0;
        p.sanitize();
        assert_eq!(p.eq.bands, 32);
        assert_eq!(p.eq.gains.len(), 32);
        assert_eq!(&p.eq.gains[..3], &[15.0, 0.0, -15.0]);
        assert_eq!(p.eq.range, [31.0, 16000.0]);
        assert_eq!(p.parametric.len(), 32);
        assert_eq!((p.parametric[0].f, p.parametric[0].q, p.parametric[0].g), (20.0, 0.1, -15.0));
        assert_eq!(p.tone.treble.f, 1000.0);
        assert_eq!(p.limiter.ceiling_dbtp, 0.0);
        let once = p.clone();
        p.sanitize();
        assert_eq!(p, once);
    }

    #[test]
    fn graphic_layout() {
        let p = Preset::default();
        let f = p.graphic_freqs();
        assert_eq!(f.len(), 10);
        assert!((f[0] - 31.0).abs() < 1e-9 && (f[9] - 16000.0).abs() < 1e-6);
        // 31 Hz…16 kHz in 9 steps is almost exactly one octave per band: Q ≈ √2.
        assert!((p.graphic_q() - std::f64::consts::SQRT_2).abs() < 0.01, "{}", p.graphic_q());
        // One-third octave: Q ≈ 4.32.
        assert!((graphic_q(31, [20.0, 20000.0]) - 4.32).abs() < 0.05);
    }

    #[test]
    fn builtins_are_locked_and_flat_is_flat() {
        let b = builtin_presets();
        assert_eq!(b[0].name, FLAT);
        assert!(b.iter().all(|p| p.locked));
        assert!(b[0].is_flat());
        assert!(!b[1].is_flat());
        let mut names: Vec<_> = b.iter().map(|p| p.name.as_str()).collect();
        names.dedup();
        assert_eq!(names.len(), b.len());
    }

    #[test]
    fn preset_round_trips() {
        let p = &builtin_presets()[2];
        let text = serde_json::to_string_pretty(p).unwrap();
        assert_eq!(&Preset::from_json(&text).unwrap(), p);
    }

    #[test]
    fn first_run_state_is_off_and_flat() {
        let s = State::default();
        assert!(!s.enabled);
        assert_eq!(s.preset, FLAT);
        let back = State::from_json(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
