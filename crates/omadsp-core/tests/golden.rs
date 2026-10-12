//! Graph generator golden files (`tests/golden/*.conf` at the repo root).
//! `UPDATE_GOLDEN=1 cargo test -p omadsp-core --test golden` rewrites them; review the diff.

use std::path::PathBuf;

use omadsp_core::graph::{Options, host_conf};
use omadsp_core::model::{Channel, EqMode, FilterKind, ParamBand, Preset, builtin_presets};

fn golden(name: &str, text: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden").join(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, text).unwrap();
        return;
    }
    let want =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e} (run with UPDATE_GOLDEN=1)", path.display()));
    if want != text {
        let line = want.lines().zip(text.lines()).position(|(a, b)| a != b).map_or(0, |i| i + 1);
        panic!("{name} differs from the golden file at line {line}; rerun with UPDATE_GOLDEN=1 and review the diff");
    }
}

#[test]
fn flat() {
    golden("flat.conf", &host_conf(&Preset::default(), &Options::default()));
}

#[test]
fn graphic_with_tone_and_limiter() {
    let mut p = builtin_presets().into_iter().find(|p| p.name == "Warm V").unwrap();
    p.tone.bass.db = 3.0;
    p.tone.treble.db = -1.5;
    p.tone.treble.f = 6000.0;
    p.limiter.ceiling_dbtp = -2.0;
    p.limiter.release_ms = 10.0;
    golden("warm-v-tone.conf", &host_conf(&p, &Options::default()));
}

#[test]
fn graphic_31_band() {
    let mut p = Preset::default();
    p.eq.bands = 31;
    p.eq.range = [20.0, 20000.0];
    p.eq.gains = (0..31).map(|i| ((i % 7) as f64 - 3.0) * 1.5).collect();
    golden("graphic-31.conf", &host_conf(&p, &Options::default()));
}

#[test]
fn parametric() {
    let mut p = Preset::default();
    p.eq.mode = EqMode::Parametric;
    p.parametric = vec![
        ParamBand { kind: FilterKind::HighPass, f: 30.0, q: 0.71, ..ParamBand::default() },
        ParamBand { kind: FilterKind::LowShelf, f: 105.0, g: 4.5, q: 0.7, ch: Channel::Left, ..ParamBand::default() },
        ParamBand { kind: FilterKind::Peak, f: 2500.0, g: -3.0, q: 2.0, ch: Channel::Right, ..ParamBand::default() },
        ParamBand { kind: FilterKind::Notch, f: 6000.0, q: 8.0, on: false, ..ParamBand::default() },
        ParamBand { kind: FilterKind::BandPass, f: 1000.0, q: 0.5, ..ParamBand::default() },
        ParamBand { kind: FilterKind::HighShelf, f: 10000.0, g: -2.0, q: 0.7, ..ParamBand::default() },
        ParamBand { kind: FilterKind::LowPass, f: 18000.0, q: 0.71, ..ParamBand::default() },
    ];
    golden("parametric.conf", &host_conf(&p, &Options::default()));
}

#[test]
fn test_target() {
    let o = Options {
        name: "omadsp_test".into(),
        description: "omadsp test".into(),
        target: Some("omadsp_spike_out".into()),
        start_disabled: false,
    };
    golden("test-target.conf", &host_conf(&Preset::default(), &o));
}
