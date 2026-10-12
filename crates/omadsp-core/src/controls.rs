//! Preset → filter-chain control values (`"node:port"` → value), and diffs between two sets.
//!
//! Stage layout (ADR-3, ADR-4):
//! - `pre` = LSP `para_equalizer_x16_lr`: bands 0–13 device correction (Phase 3; Off for now),
//!   band 14 the bass shelf, band 15 the treble shelf.
//! - `eq` = LSP `para_equalizer_x32_lr`: the user EQ. Graphic band `i` or parametric band `i`
//!   always sits in slot `i`, so editing one band never touches another.
//! - `lim` = LSP `limiter_stereo`.
//!
//! Every band is written in full on both sides (type, mode, frequency, Q, gain), so a control set
//! describes the whole stage. Flat peak/shelf bands are switched Off, never left on at 0 dB
//! (ADR-3: −95 dB residual otherwise).

use std::collections::BTreeMap;

use crate::dsp_math::{SHELF_Q, db_to_lin};
use crate::model::{EqMode, FilterKind, ParamBand, Preset};

pub const PRE: &str = "pre";
pub const EQ: &str = "eq";
pub const LIM: &str = "lim";
pub const PRE_BANDS: usize = 16;
pub const EQ_BANDS: usize = 32;
pub const BASS_BAND: usize = 14;
pub const TREBLE_BAND: usize = 15;

/// LSP filter type codes (`ft*_N` scale points in the plugin TTL).
pub fn lsp_filter_type(kind: FilterKind) -> u32 {
    match kind {
        FilterKind::Peak => 1,
        FilterKind::HighPass => 2,
        FilterKind::HighShelf => 3,
        FilterKind::LowPass => 4,
        FilterKind::LowShelf => 5,
        FilterKind::Notch => 6,
        FilterKind::BandPass => 9,
    }
}
pub const LSP_OFF: u32 = 0;
/// `fm*_N`: APO (DR), the Equalizer APO / AutoEq designs.
pub const LSP_MODE_APO: u32 = 6;

/// A full or partial set of control values, ordered by name.
pub type Controls = BTreeMap<String, f32>;

#[derive(Clone, Copy)]
struct Band {
    kind: Option<FilterKind>,
    f: f64,
    g: f64,
    q: f64,
}

const OFF: Band = Band { kind: None, f: 1000.0, g: 0.0, q: 1.0 };

impl Band {
    fn new(kind: FilterKind, f: f64, g: f64, q: f64) -> Band {
        let active = !kind.uses_gain() || g != 0.0;
        Band { kind: active.then_some(kind), f, g: if kind.uses_gain() { g } else { 0.0 }, q }
    }
}

fn put_band(c: &mut Controls, stage: &str, i: usize, side: char, b: Band) {
    let t = b.kind.map_or(LSP_OFF, lsp_filter_type);
    c.insert(format!("{stage}:ft{side}_{i}"), t as f32);
    c.insert(format!("{stage}:fm{side}_{i}"), LSP_MODE_APO as f32);
    c.insert(format!("{stage}:f{side}_{i}"), b.f as f32);
    c.insert(format!("{stage}:q{side}_{i}"), b.q as f32);
    c.insert(format!("{stage}:g{side}_{i}"), db_to_lin(b.g) as f32);
}

fn param_band(b: &ParamBand, side: char) -> Band {
    let applies = if side == 'l' { b.ch.left() } else { b.ch.right() };
    if b.on && applies {
        Band::new(b.kind, b.f, b.g, b.q)
    } else {
        Band { kind: None, ..Band::new(b.kind, b.f, b.g, b.q) }
    }
}

/// Every control omadsp manages, for `p`.
pub fn controls(p: &Preset) -> Controls {
    let mut c = Controls::new();
    for side in ['l', 'r'] {
        for i in 0..BASS_BAND {
            put_band(&mut c, PRE, i, side, OFF);
        }
        let t = &p.tone;
        put_band(&mut c, PRE, BASS_BAND, side, Band::new(FilterKind::LowShelf, t.bass.f, t.bass.db, SHELF_Q));
        put_band(&mut c, PRE, TREBLE_BAND, side, Band::new(FilterKind::HighShelf, t.treble.f, t.treble.db, SHELF_Q));

        match p.eq.mode {
            EqMode::Graphic => {
                let q = p.graphic_q();
                let freqs = p.graphic_freqs();
                for i in 0..EQ_BANDS {
                    let b = match (freqs.get(i), p.eq.gains.get(i)) {
                        (Some(&f), Some(&g)) => Band::new(FilterKind::Peak, f, g, q),
                        _ => OFF,
                    };
                    put_band(&mut c, EQ, i, side, b);
                }
            }
            EqMode::Parametric => {
                for i in 0..EQ_BANDS {
                    let b = p.parametric.get(i).map_or(OFF, |b| param_band(b, side));
                    put_band(&mut c, EQ, i, side, b);
                }
            }
        }
    }
    c.insert(format!("{LIM}:th"), db_to_lin(p.limiter.ceiling_dbtp) as f32);
    c.insert(format!("{LIM}:rt"), p.limiter.release_ms as f32);
    c
}

/// Entries of `new` that differ from `old` (or are missing there).
pub fn diff(old: &Controls, new: &Controls) -> Controls {
    new.iter().filter(|(k, v)| old.get(*k) != Some(v)).map(|(k, v)| (k.clone(), *v)).collect()
}

/// Linear gain for the capture node's `volume`: the preamp including auto headroom.
pub fn preamp_volume(p: &Preset, fs: f64) -> f32 {
    db_to_lin(crate::dsp_math::preamp_db(p, fs).0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Channel, builtin_presets};

    #[test]
    fn flat_switches_every_band_off() {
        let c = controls(&Preset::default());
        assert_eq!(c.len(), 2 * (PRE_BANDS + EQ_BANDS) * 5 + 2);
        assert!(c.iter().filter(|(k, _)| k.contains(":ft")).all(|(_, &v)| v == 0.0));
        assert!(c.iter().filter(|(k, _)| k.contains(":fm")).all(|(_, &v)| v == 6.0));
        assert!((c["lim:th"] - 0.891_251).abs() < 1e-6);
        assert_eq!(c["lim:rt"], 5.0);
    }

    #[test]
    fn graphic_bands_land_in_their_slots() {
        let p = &builtin_presets()[2]; // Warm V: 3,2,1,0,-1,-1,0,1,2,3
        let c = controls(p);
        assert_eq!(c["eq:ftl_0"], 1.0);
        assert!((c["eq:gl_0"] - 1.412_538).abs() < 1e-6);
        assert_eq!(c["eq:ftl_3"], 0.0, "0 dB band is Off");
        assert_eq!(c["eq:ftr_9"], 1.0);
        assert_eq!(c["eq:ftl_10"], 0.0, "beyond the band count");
        assert!((c["eq:fl_9"] - 16000.0).abs() < 0.01);
    }

    #[test]
    fn tone_uses_shelf_bands() {
        let mut p = Preset::default();
        p.tone.bass.db = 4.0;
        p.tone.treble.db = -2.0;
        p.tone.treble.f = 6000.0;
        let c = controls(&p);
        assert_eq!((c["pre:ftl_14"], c["pre:ftr_15"]), (5.0, 3.0));
        assert_eq!(c["pre:fr_15"], 6000.0);
        assert!((c["pre:ql_14"] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    }

    #[test]
    fn parametric_channels_and_types() {
        let mut p = Preset::default();
        p.eq.mode = EqMode::Parametric;
        p.parametric = vec![
            ParamBand { kind: FilterKind::HighPass, f: 40.0, q: 0.7, ch: Channel::Right, ..ParamBand::default() },
            ParamBand { kind: FilterKind::Peak, g: 0.0, ..ParamBand::default() },
            ParamBand { kind: FilterKind::Notch, f: 3000.0, on: false, ..ParamBand::default() },
        ];
        let c = controls(&p);
        assert_eq!((c["eq:ftl_0"], c["eq:ftr_0"]), (0.0, 2.0));
        assert_eq!(c["eq:gr_0"], 1.0, "gain is unity for gainless types");
        assert_eq!(c["eq:ftl_1"], 0.0, "flat bell is Off");
        assert_eq!((c["eq:ftl_2"], c["eq:fl_2"]), (0.0, 3000.0), "disabled band is Off but keeps its values");
    }

    #[test]
    fn diff_sends_only_changes() {
        let a = controls(&Preset::default());
        let mut p = Preset::default();
        p.eq.gains[4] = -3.0;
        let d = diff(&a, &controls(&p));
        let keys: Vec<_> = d.keys().map(String::as_str).collect();
        assert_eq!(keys, ["eq:ftl_4", "eq:ftr_4", "eq:gl_4", "eq:gr_4"]);
        assert!(diff(&a, &a).is_empty());
    }

    #[test]
    fn preamp_volume_follows_headroom() {
        let mut p = Preset::default();
        assert_eq!(preamp_volume(&p, 48000.0), 1.0);
        p.eq.gains[5] = 6.0;
        assert!((preamp_volume(&p, 48000.0) - 0.5012).abs() < 0.002);
    }
}
