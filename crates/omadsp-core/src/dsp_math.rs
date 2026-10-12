//! RBJ Audio-EQ-Cookbook biquads, the combined response of a preset, and auto headroom.
//!
//! The EQ stages run LSP `para_equalizer` in APO (DR) mode, which implements the cookbook designs
//! Equalizer APO uses (Spike A: LSP vs these formulas ≤ 0.026 dB). Bandpass is the constant 0 dB
//! peak gain variant, as in Equalizer APO.

use crate::model::{Channel, EqMode, FilterKind, Preset};

/// Points in the response curve sent to the UI.
pub const CURVE_POINTS: usize = 256;
/// Curve span (Hz).
pub const CURVE_RANGE: [f64; 2] = [20.0, 20000.0];

/// One filter of a channel's chain, in user units (Hz, dB, Q).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Filter {
    pub kind: FilterKind,
    pub f: f64,
    pub g: f64,
    pub q: f64,
}

/// Normalised biquad coefficients (a0 = 1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Biquad {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Filter {
    pub fn biquad(&self, fs: f64) -> Biquad {
        let a = 10f64.powf(self.g / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * self.f / fs;
        let (sw, cw) = w0.sin_cos();
        let alpha = sw / (2.0 * self.q);
        let (b, a) = match self.kind {
            FilterKind::Peak => {
                ([1.0 + alpha * a, -2.0 * cw, 1.0 - alpha * a], [1.0 + alpha / a, -2.0 * cw, 1.0 - alpha / a])
            }
            FilterKind::LowShelf => {
                let sa = 2.0 * a.sqrt() * alpha;
                (
                    [
                        a * ((a + 1.0) - (a - 1.0) * cw + sa),
                        2.0 * a * ((a - 1.0) - (a + 1.0) * cw),
                        a * ((a + 1.0) - (a - 1.0) * cw - sa),
                    ],
                    [
                        (a + 1.0) + (a - 1.0) * cw + sa,
                        -2.0 * ((a - 1.0) + (a + 1.0) * cw),
                        (a + 1.0) + (a - 1.0) * cw - sa,
                    ],
                )
            }
            FilterKind::HighShelf => {
                let sa = 2.0 * a.sqrt() * alpha;
                (
                    [
                        a * ((a + 1.0) + (a - 1.0) * cw + sa),
                        -2.0 * a * ((a - 1.0) + (a + 1.0) * cw),
                        a * ((a + 1.0) + (a - 1.0) * cw - sa),
                    ],
                    [
                        (a + 1.0) - (a - 1.0) * cw + sa,
                        2.0 * ((a - 1.0) - (a + 1.0) * cw),
                        (a + 1.0) - (a - 1.0) * cw - sa,
                    ],
                )
            }
            FilterKind::LowPass => {
                ([(1.0 - cw) / 2.0, 1.0 - cw, (1.0 - cw) / 2.0], [1.0 + alpha, -2.0 * cw, 1.0 - alpha])
            }
            FilterKind::HighPass => {
                ([(1.0 + cw) / 2.0, -(1.0 + cw), (1.0 + cw) / 2.0], [1.0 + alpha, -2.0 * cw, 1.0 - alpha])
            }
            FilterKind::BandPass => ([alpha, 0.0, -alpha], [1.0 + alpha, -2.0 * cw, 1.0 - alpha]),
            FilterKind::Notch => ([1.0, -2.0 * cw, 1.0], [1.0 + alpha, -2.0 * cw, 1.0 - alpha]),
        };
        Biquad { b0: b[0] / a[0], b1: b[1] / a[0], b2: b[2] / a[0], a1: a[1] / a[0], a2: a[2] / a[0] }
    }

    /// Whether the filter changes the signal at all. Peak and shelf filters at 0 dB are exact
    /// identities and are switched off in the EQ stage (ADR-3).
    pub fn is_active(&self) -> bool {
        !self.kind.uses_gain() || self.g != 0.0
    }
}

impl Biquad {
    /// |H(e^jw)|² at `f`, in the cookbook's φ = sin²(w/2) form, which stays accurate near DC
    /// where the plain cos(w) form cancels.
    pub fn power(&self, f: f64, fs: f64) -> f64 {
        let phi = (std::f64::consts::PI * f / fs).sin().powi(2);
        let Biquad { b0, b1, b2, a1, a2 } = *self;
        let num = (b0 + b1 + b2).powi(2) - 4.0 * (b0 * b1 + 4.0 * b0 * b2 + b1 * b2) * phi + 16.0 * b0 * b2 * phi * phi;
        let den = (1.0 + a1 + a2).powi(2) - 4.0 * (a1 + 4.0 * a2 + a1 * a2) * phi + 16.0 * a2 * phi * phi;
        num / den
    }
}

fn power_to_db(p: f64) -> f64 {
    // −300 dB stands in for a perfect notch, so JSON never sees −inf.
    (10.0 * p.max(0.0).log10()).max(-300.0)
}

/// Combined magnitude (dB) of a chain at each frequency. Frequencies at or above Nyquist are
/// evaluated just below it.
pub fn response_db(filters: &[Filter], freqs: &[f64], fs: f64) -> Vec<f64> {
    let bq: Vec<Biquad> = filters.iter().filter(|f| f.is_active()).map(|f| f.biquad(fs)).collect();
    let top = fs / 2.0 * 0.9999;
    freqs.iter().map(|&f| power_to_db(bq.iter().map(|b| b.power(f.min(top), fs)).product())).collect()
}

/// `n` log-spaced frequencies across [`CURVE_RANGE`], both ends included.
pub fn curve_freqs(n: usize) -> Vec<f64> {
    crate::model::graphic_freqs(n, CURVE_RANGE)
}

/// Largest boost (dB, ≥ 0) of a chain within [`CURVE_RANGE`]. Checks a dense grid and every
/// filter's own frequency, so a narrow peak between grid points isn't missed.
pub fn max_boost_db(filters: &[Filter], fs: f64) -> f64 {
    let mut freqs = curve_freqs(2048);
    let (lo, hi) = (CURVE_RANGE[0], CURVE_RANGE[1].min(fs / 2.0));
    freqs.extend(filters.iter().map(|f| f.f).filter(|&f| f >= lo && f <= hi));
    response_db(filters, &freqs, fs).into_iter().fold(0.0, f64::max)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// Every active filter of one channel, in chain order: tone shelves, then the user EQ.
/// (Device correction joins in Phase 3.)
pub fn preset_filters(p: &Preset, side: Side) -> Vec<Filter> {
    let mut v = Vec::new();
    let t = &p.tone;
    v.push(Filter { kind: FilterKind::LowShelf, f: t.bass.f, g: t.bass.db, q: SHELF_Q });
    v.push(Filter { kind: FilterKind::HighShelf, f: t.treble.f, g: t.treble.db, q: SHELF_Q });
    match p.eq.mode {
        EqMode::Graphic => {
            let q = p.graphic_q();
            for (f, &g) in p.graphic_freqs().into_iter().zip(&p.eq.gains) {
                v.push(Filter { kind: FilterKind::Peak, f, g, q });
            }
        }
        EqMode::Parametric => {
            for b in p.parametric.iter().filter(|b| b.on && applies(b.ch, side)) {
                v.push(Filter { kind: b.kind, f: b.f, g: b.g, q: b.q });
            }
        }
    }
    v.retain(Filter::is_active);
    v
}

/// Q of the bass and treble shelves: the cookbook's S = 1 slope (no overshoot).
pub const SHELF_Q: f64 = std::f64::consts::FRAC_1_SQRT_2;

fn applies(ch: Channel, side: Side) -> bool {
    match side {
        Side::Left => ch.left(),
        Side::Right => ch.right(),
    }
}

/// The auto-headroom part of the preamp (≤ 0 dB): minus the larger channel's biggest boost.
pub fn auto_headroom_db(p: &Preset, fs: f64) -> f64 {
    let l = max_boost_db(&preset_filters(p, Side::Left), fs);
    let r = max_boost_db(&preset_filters(p, Side::Right), fs);
    -l.max(r)
}

/// The gain applied at the head of the chain, in dB: the user value plus auto headroom.
pub fn preamp_db(p: &Preset, fs: f64) -> (f64, f64) {
    let auto = if p.preamp.auto { auto_headroom_db(p, fs) } else { 0.0 };
    (p.preamp.db + auto, auto)
}

/// The response curves of both channels at [`CURVE_POINTS`] points. `right` is `None` when both
/// channels are identical.
pub struct Curve {
    pub freqs: Vec<f64>,
    pub left: Vec<f64>,
    pub right: Option<Vec<f64>>,
}

pub fn curve(p: &Preset, fs: f64) -> Curve {
    let freqs = curve_freqs(CURVE_POINTS);
    let (fl, fr) = (preset_filters(p, Side::Left), preset_filters(p, Side::Right));
    let left = response_db(&fl, &freqs, fs);
    let right = (fl != fr).then(|| response_db(&fr, &freqs, fs));
    Curve { freqs, left, right }
}

pub fn db_to_lin(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ParamBand;

    const FREQS: [f64; 6] = [20.0, 100.0, 1000.0, 3100.0, 10000.0, 19000.0];

    fn check(kind: FilterKind, f: f64, g: f64, q: f64, fs: f64, want: [f64; 6]) {
        let got = response_db(&[Filter { kind, f, g, q }], &FREQS, fs);
        for (i, (a, b)) in got.iter().zip(want).enumerate() {
            assert!((a - b).abs() < 1e-4, "{kind:?} {f} Hz @ {fs}: point {i} got {a}, want {b}");
        }
    }

    #[test]
    fn matches_reference_implementation() {
        // spikes/lib/rbj.py, the reference used in Spike A/B (LSP measured within 0.026 dB of it).
        // It evaluates H(z) as complex numbers; the two forms agree to ~1e-4 dB.
        use FilterKind::*;
        check(Peak, 1000.0, 6.0, 1.414, 48000.0, [0.001295, 0.032808, 6.0, 0.378587, 0.023917, 0.001608]);
        check(LowShelf, 105.0, 4.0, 0.707, 48000.0, [3.994511, 2.191157, 0.000514, 7e-06, 0.0, 0.0]);
        check(HighShelf, 8000.0, -3.0, 0.707, 48000.0, [0.0, 0.0, -0.00052, -0.048612, -2.26606, -2.995458]);
        check(LowPass, 5000.0, 0.0, 0.707, 48000.0, [0.0, -2e-06, -0.00613, -0.550441, -14.331693, -37.538299]);
        check(HighPass, 80.0, 0.0, 2.0, 48000.0, [-23.598512, 5.381968, 0.048597, 0.004924, 0.000354, 2.4e-05]);
        check(BandPass, 1000.0, 0.0, 1.0, 48000.0, [-33.990078, -19.969203, 0.0, -9.519844, -21.337451, -33.051638]);
        check(Notch, 3000.0, 0.0, 4.0, 48000.0, [-1.2e-05, -0.000295, -0.03693, -11.694242, -0.020912, -0.001249]);
        check(Peak, 1000.0, 6.0, 1.414, 44100.0, [0.001294, 0.032791, 6.0, 0.37659, 0.022363, 0.000806]);
        check(HighShelf, 8000.0, -3.0, 0.707, 44100.0, [0.0, 0.0, -0.000482, -0.045472, -2.296356, -2.998757]);
        check(LowPass, 5000.0, 0.0, 0.707, 44100.0, [0.0, -2e-06, -0.005969, -0.541454, -14.777051, -43.418886]);
    }

    #[test]
    fn closed_forms() {
        let fs = 48000.0;
        let at = |kind, f, g, q, x| response_db(&[Filter { kind, f, g, q }], &[x], fs)[0];
        // Peak: exactly its gain at f0.
        assert!((at(FilterKind::Peak, 440.0, -7.5, 3.0, 440.0) + 7.5).abs() < 1e-9);
        // Low/high pass: |H(f0)| = Q.
        assert!((at(FilterKind::LowPass, 2000.0, 0.0, 2.0, 2000.0) - 20.0 * 2f64.log10()).abs() < 1e-9);
        assert!((at(FilterKind::HighPass, 2000.0, 0.0, 0.5, 2000.0) - 20.0 * 0.5f64.log10()).abs() < 1e-9);
        // Shelves: full gain at the far end, half (in dB) at f0 for the S = 1 shape.
        assert!((at(FilterKind::LowShelf, 100.0, 6.0, SHELF_Q, 100.0) - 3.0).abs() < 1e-9);
        assert!((at(FilterKind::HighShelf, 4000.0, 6.0, SHELF_Q, 23990.0) - 6.0).abs() < 0.01);
        // Notch: a zero at f0 (rounding leaves a tiny remainder; −300 dB is the JSON-safe floor).
        let n = at(FilterKind::Notch, 1000.0, 0.0, 1.0, 1000.0);
        assert!((-300.0..-120.0).contains(&n), "{n}");
    }

    #[test]
    fn flat_preset_is_flat_and_needs_no_headroom() {
        let p = Preset::default();
        assert!(preset_filters(&p, Side::Left).is_empty());
        let c = curve(&p, 48000.0);
        assert_eq!(c.freqs.len(), CURVE_POINTS);
        assert!(c.left.iter().all(|&d| d == 0.0));
        assert!(c.right.is_none());
        assert_eq!(preamp_db(&p, 48000.0), (0.0, 0.0));
    }

    #[test]
    fn auto_headroom_follows_the_combined_peak() {
        let mut p = Preset::default();
        p.eq.gains[4] = 6.0; // 500 Hz-ish band
        let (pre, auto) = preamp_db(&p, 48000.0);
        assert!((auto + 6.0).abs() < 0.02, "{auto}");
        assert_eq!(pre, auto);
        // Two neighbouring boosts overlap: the combined peak exceeds either band.
        p.eq.gains[5] = 6.0;
        assert!(auto_headroom_db(&p, 48000.0) < -6.5);
        // Cuts need no headroom; the user offset adds on top.
        let mut q = Preset::default();
        q.eq.gains = vec![-6.0; 10];
        q.preamp.db = 2.0;
        assert_eq!(preamp_db(&q, 48000.0), (2.0, 0.0));
        // Auto off: only the user value.
        p.preamp = crate::model::Preamp { db: -1.5, auto: false };
        assert_eq!(preamp_db(&p, 48000.0), (-1.5, 0.0));
    }

    #[test]
    fn narrow_peaks_are_not_missed() {
        // A Q 30 bell between grid points still counts fully.
        let f = [Filter { kind: FilterKind::Peak, f: 1234.5, g: 9.0, q: 30.0 }];
        assert!((max_boost_db(&f, 48000.0) - 9.0).abs() < 1e-9);
        // A resonant low-pass peaks above its corner: ~+6 dB at Q 2.
        let lp = [Filter { kind: FilterKind::LowPass, f: 3000.0, g: 0.0, q: 2.0 }];
        let m = max_boost_db(&lp, 48000.0);
        assert!(m > 6.0 && m < 6.4, "{m}");
    }

    #[test]
    fn parametric_channels() {
        let mut p = Preset::default();
        p.eq.mode = EqMode::Parametric;
        p.parametric = vec![
            ParamBand { f: 100.0, g: 3.0, ch: Channel::Left, ..ParamBand::default() },
            ParamBand { f: 5000.0, g: -4.0, ..ParamBand::default() },
            ParamBand { f: 300.0, g: 8.0, on: false, ..ParamBand::default() },
        ];
        assert_eq!(preset_filters(&p, Side::Left).len(), 2);
        assert_eq!(preset_filters(&p, Side::Right).len(), 1);
        let c = curve(&p, 48000.0);
        assert!(c.right.is_some());
        assert!((auto_headroom_db(&p, 48000.0) + 3.0).abs() < 0.05);
    }
}
