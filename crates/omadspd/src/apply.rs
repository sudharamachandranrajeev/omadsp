//! Getting the filter from what it has to what the user wants, without clicks (ADR-4).
//!
//! Sans-IO: [`Applier`] takes the time and the filter's state as inputs and returns [`Action`]s, so
//! every sequence is unit-tested without PipeWire. The engine performs the actions and polls again at
//! [`Applier::next_wakeup`] or when something changes.
//!
//! Rules (Phase 0, step 2):
//! - Control values go out as one Props `params` update, at most every 16 ms; quick drags coalesce.
//!   A band that changes type does so at 0 dB, in steps one quantum apart
//!   ([`controls::live_step`]); a pass filter or notch switching on or off waits for a dip.
//! - Volumes ramp in audioconvert. A ramp that arrives while another one is still running is
//!   dropped, and the old ramp then ends on its own value. A ramp advances only with processed
//!   audio, one whole cycle at a time, and starts on the cycle after it arrives. So each volume line
//!   sends at most one ramp per busy window (the ramp rounded up to whole cycles, plus a margin of
//!   one cycle and IPC time) and sends its latest target when the window ends. Audioconvert keeps an unfinished ramp across a suspend,
//!   so when the filter starts running again both lines wait one window and re-send their targets.
//! - While the filter isn't running, nothing can be heard: values are set directly, without ramps or
//!   dips.
//! - Structural changes (on/off, A/B, a preset switch) happen inside a *dip*: the playback volume
//!   ramps to 0, the change is made, the volume ramps back.
//! - On "DSP on", the filter's output stays at 0 until WirePlumber has linked streams into it (or a
//!   timeout passes), then ramps up.

use std::time::{Duration, Instant};

use omadsp_core::controls::{self, Controls, EQ, LIM, LiveStep, PRE};

use crate::pw::Ramp;

/// The capture node's volume is the preamp (before the graph); the playback node's volume is the
/// dip/mute stage (after the limiter).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line {
    Capture,
    Playback,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// filter-chain control values, sent to the capture node.
    Params(Vec<(String, f32)>),
    Volume {
        line: Line,
        volume: f32,
        ramp: Option<Ramp>,
    },
    /// `filter.smart.disabled` in the `filters` metadata.
    SmartDisabled(bool),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timing {
    /// Dips (to 0 and back). Step 6: 30 ms in 1-sample steps stays under −60 dBc on a 1 kHz tone;
    /// 15 ms measured −49 dBc, 4-sample steps −40 dBc.
    pub dip: Ramp,
    /// Gain (preamp) ramps: `gain_min_ms` + `gain_ms_per_db` per dB moved, at most `gain_max_ms`.
    /// Small drag steps stay quick; a 5.5 dB jump in 15 ms measured −53 dBc.
    pub gain_min_ms: i32,
    pub gain_ms_per_db: f32,
    pub gain_max_ms: i32,
    /// The longest cycle: ramps run in whole cycles of it.
    pub quantum: Duration,
    /// Added to a ramp's cycles before the same line ramps again: covers IPC and the cycle a ramp
    /// waits before it starts.
    pub margin: Duration,
    pub params_interval: Duration,
    /// How long "DSP on" waits for streams to be linked into the filter before ramping up anyway.
    pub link_timeout: Duration,
}

impl Timing {
    /// Margin = the longest quantum at this rate, plus 15 ms.
    pub fn for_clock(rate: u32, max_quantum: u32) -> Timing {
        let quantum = Duration::from_secs_f64(f64::from(max_quantum.max(32)) / f64::from(rate.max(8000)));
        Timing {
            dip: Ramp { time_ms: 30, step_samples: 1 },
            gain_min_ms: 15,
            gain_ms_per_db: 5.0,
            gain_max_ms: 60,
            quantum,
            margin: (quantum + Duration::from_millis(15)).min(Duration::from_millis(250)),
            params_interval: Duration::from_millis(16),
            link_timeout: Duration::from_millis(300),
        }
    }

    /// The ramp for a gain change from `from` (unknown: the longest) to `to`.
    pub fn gain_ramp(&self, from: Option<f32>, to: f32) -> Ramp {
        let ms = match from {
            Some(f) if f > 0.0 && to > 0.0 => {
                let db = (20.0 * (to / f).log10()).abs();
                (self.gain_min_ms as f32 + self.gain_ms_per_db * db).round() as i32
            }
            _ => self.gain_max_ms,
        };
        Ramp { time_ms: ms.clamp(self.gain_min_ms, self.gain_max_ms), step_samples: 1 }
    }

    /// How long a ramp keeps its line busy.
    pub fn busy(&self, r: Ramp) -> Duration {
        let ramp = Duration::from_millis(r.time_ms.max(0) as u64);
        let q = self.quantum.max(Duration::from_millis(1));
        q * ramp.as_nanos().div_ceil(q.as_nanos()) as u32 + self.margin
    }

    /// Covers the longest ramp that might still be running.
    fn reassert_window(&self) -> Duration {
        self.busy(Ramp { time_ms: self.dip.time_ms.max(self.gain_max_ms), step_samples: 1 })
    }
}

impl Default for Timing {
    fn default() -> Self {
        Timing::for_clock(48000, 2048)
    }
}

/// What the filter should be doing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Target {
    pub enabled: bool,
    /// Bypass for comparison: every stage off, preamp at unity. Only meaningful while enabled.
    pub ab: bool,
    pub controls: Controls,
    /// Linear preamp (capture volume), auto headroom included.
    pub preamp: f32,
}

/// One audioconvert volume, with at most one ramp in flight.
#[derive(Debug)]
struct VolumeLine {
    target: f32,
    /// Last value sent; `None` = unknown, send the target.
    sent: Option<f32>,
    busy_until: Option<Instant>,
}

impl VolumeLine {
    fn new(target: f32) -> VolumeLine {
        VolumeLine { target, sent: None, busy_until: None }
    }

    fn busy(&self, now: Instant) -> bool {
        self.busy_until.is_some_and(|t| now < t)
    }

    /// The value is sent and no ramp is still running.
    fn settled(&self, now: Instant) -> bool {
        self.sent == Some(self.target) && !self.busy(now)
    }

    /// `ramp` picks the ramp for a move from the last sent value (if known) to the target.
    fn poll(
        &mut self,
        now: Instant,
        running: bool,
        timing: &Timing,
        ramp: impl Fn(Option<f32>, f32) -> Ramp,
    ) -> Option<(f32, Option<Ramp>)> {
        if self.busy(now) {
            return None;
        }
        self.busy_until = None;
        if self.sent == Some(self.target) {
            return None;
        }
        let from = self.sent.replace(self.target);
        if running {
            let r = ramp(from, self.target);
            self.busy_until = Some(now + timing.busy(r));
            Some((self.target, Some(r)))
        } else {
            Some((self.target, None))
        }
    }

    /// After a resume: let any ramp left over from before the suspend finish, then send again.
    fn reassert(&mut self, now: Instant, timing: &Timing) {
        self.sent = None;
        self.busy_until = Some(now + timing.reassert_window());
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Steady,
    /// The output is ramping to 0.
    DipDown,
    /// The change is made; waiting until it has reached the audio (one margin) and the preamp has
    /// finished its ramp.
    DipHold {
        until: Instant,
    },
    /// The smart filter was just switched on; waiting for streams to be linked into it.
    Linking {
        deadline: Instant,
    },
}

/// State that only exists while the filter's nodes exist.
#[derive(Debug)]
struct Attached {
    /// Control values the filter has (sent or from the host config).
    have: Controls,
    stages_on: bool,
    smart_disabled: bool,
    capture: VolumeLine,
    playback: VolumeLine,
    phase: Phase,
    /// A preset switch waiting for its dip.
    pending_smooth: bool,
    /// The smart filter was switched on in this dip: link before ramping up.
    linking: bool,
    /// Earliest time for the next live control update.
    next_params: Option<Instant>,
    running: bool,
    input_linked: bool,
}

#[derive(Debug, Default)]
pub struct Applier {
    timing: Timing,
    target: Target,
    attached: Option<Attached>,
}

fn stage_switches(on: bool) -> Vec<(String, f32)> {
    let v = if on { 1.0 } else { 0.0 };
    [PRE, EQ, LIM].iter().map(|s| (format!("{s}:enabled"), v)).collect()
}

impl Attached {
    fn structural_pending(&self, t: &Target) -> bool {
        self.stages_on == t.ab || self.smart_disabled == t.enabled || self.pending_smooth
    }

    /// The output volume is at 0 and no ramp is running.
    fn silent(&self, now: Instant) -> bool {
        self.playback.sent == Some(0.0) && !self.playback.busy(now)
    }

    /// Whether the filter's output can be heard right now.
    fn audible(&self, now: Instant) -> bool {
        self.running && !self.silent(now)
    }

    fn apply_structural(&mut self, t: &Target, now: Instant, timing: &Timing, out: &mut Vec<Action>) {
        let mut params = Vec::new();
        if self.stages_on == t.ab {
            params.extend(stage_switches(!t.ab));
            self.stages_on = !t.ab;
        }
        if self.pending_smooth {
            let d = controls::diff(&self.have, &t.controls);
            self.have.extend(d.iter().map(|(k, v)| (k.clone(), *v)));
            params.extend(d);
            self.pending_smooth = false;
        }
        if !params.is_empty() {
            out.push(Action::Params(params));
            self.next_params = Some(now + timing.params_interval);
        }
        if self.smart_disabled == t.enabled {
            out.push(Action::SmartDisabled(!t.enabled));
            self.smart_disabled = !t.enabled;
            self.linking = t.enabled;
        }
    }

    fn after_change(&mut self, now: Instant, timing: &Timing) -> Phase {
        if std::mem::take(&mut self.linking) {
            Phase::Linking { deadline: now + timing.link_timeout }
        } else {
            Phase::Steady
        }
    }
}

impl Applier {
    pub fn new(timing: Timing) -> Applier {
        Applier { timing, ..Applier::default() }
    }

    pub fn set_timing(&mut self, timing: Timing) {
        self.timing = timing;
    }

    pub fn timing(&self) -> Timing {
        self.timing
    }

    /// The filter's nodes appeared. `have` = the controls its config was generated with;
    /// `smart_disabled` = its `filter.smart.disabled` node property.
    pub fn attach(&mut self, have: Controls, smart_disabled: bool) {
        self.attached = Some(Attached {
            have,
            stages_on: true,
            smart_disabled,
            capture: VolumeLine::new(self.target.preamp),
            playback: VolumeLine::new(0.0),
            phase: Phase::Steady,
            pending_smooth: false,
            linking: false,
            next_params: None,
            running: false,
            input_linked: false,
        });
    }

    pub fn detach(&mut self) {
        self.attached = None;
    }

    pub fn is_attached(&self) -> bool {
        self.attached.is_some()
    }

    /// The `filters` metadata was (re)created: the switch is back at the node property's value.
    pub fn smart_reset(&mut self, smart_disabled: bool) {
        if let Some(a) = &mut self.attached {
            a.smart_disabled = smart_disabled;
        }
    }

    /// `smooth`: a preset switch, applied inside a dip. Otherwise changes go out live (drags).
    pub fn set_target(&mut self, t: Target, smooth: bool) {
        if let Some(a) = &mut self.attached
            && smooth
            && a.have != t.controls
        {
            a.pending_smooth = true;
        }
        self.target = t;
    }

    pub fn target(&self) -> &Target {
        &self.target
    }

    pub fn set_running(&mut self, now: Instant, running: bool) {
        let timing = self.timing;
        if let Some(a) = &mut self.attached {
            if running && !a.running {
                a.capture.reassert(now, &timing);
                a.playback.reassert(now, &timing);
            }
            a.running = running;
        }
    }

    pub fn set_input_linked(&mut self, linked: bool) {
        if let Some(a) = &mut self.attached {
            a.input_linked = linked;
        }
    }

    /// The DSP is on and audible: switched on, linked, output up, nothing pending.
    pub fn is_active(&self) -> bool {
        self.attached.as_ref().is_some_and(|a| {
            self.target.enabled && !a.smart_disabled && a.phase == Phase::Steady && a.playback.target == 1.0
        })
    }

    /// The filter is switched off in the metadata and no change is in flight.
    pub fn is_off(&self) -> bool {
        self.attached.as_ref().is_none_or(|a| a.smart_disabled && a.phase == Phase::Steady)
    }

    pub fn poll(&mut self, now: Instant) -> Vec<Action> {
        let timing = self.timing;
        let t = &self.target;
        let Some(a) = &mut self.attached else { return Vec::new() };
        let mut out = Vec::new();

        // Live control changes. Some can't be made click-free while audio runs: those wait for a dip.
        let live = (!a.pending_smooth && a.have != t.controls).then(|| {
            if a.running {
                controls::live_step(&a.have, &t.controls)
            } else {
                LiveStep { now: controls::diff(&a.have, &t.controls), ..LiveStep::default() }
            }
        });
        if live.as_ref().is_some_and(|s| s.structural) {
            a.pending_smooth = true;
        }

        loop {
            let next = match a.phase {
                Phase::Steady if a.structural_pending(t) => {
                    if a.audible(now) {
                        Phase::DipDown
                    } else {
                        a.apply_structural(t, now, &timing, &mut out);
                        a.after_change(now, &timing)
                    }
                }
                Phase::DipDown if a.silent(now) => {
                    a.apply_structural(t, now, &timing, &mut out);
                    Phase::DipHold { until: now + timing.margin }
                }
                Phase::DipHold { until } if now >= until && a.capture.settled(now) => {
                    if a.structural_pending(t) {
                        // Changed again during the dip: apply while still silent.
                        a.apply_structural(t, now, &timing, &mut out);
                        Phase::DipHold { until: now + timing.margin }
                    } else {
                        a.after_change(now, &timing)
                    }
                }
                Phase::Linking { deadline } if !t.enabled || now >= deadline || (a.input_linked && a.running) => {
                    Phase::Steady
                }
                p => p,
            };
            if next == a.phase {
                break;
            }
            a.phase = next;
        }

        if let Some(step) = live
            && !a.pending_smooth
            && !step.now.is_empty()
            && a.next_params.is_none_or(|n| now >= n)
        {
            a.have.extend(step.now.iter().map(|(k, v)| (k.clone(), *v)));
            out.push(Action::Params(step.now.into_iter().collect()));
            a.next_params = Some(now + if step.settle { timing.margin } else { timing.params_interval });
        }

        // The preamp belongs with the controls: a preset switch changes both inside the dip.
        if !a.pending_smooth {
            a.capture.target = if a.stages_on { t.preamp } else { 1.0 };
        }
        a.playback.target = if t.enabled && !a.smart_disabled && a.phase == Phase::Steady { 1.0 } else { 0.0 };
        if let Some((volume, ramp)) = a.capture.poll(now, a.running, &timing, |f, t| timing.gain_ramp(f, t)) {
            out.push(Action::Volume { line: Line::Capture, volume, ramp });
        }
        if let Some((volume, ramp)) = a.playback.poll(now, a.running, &timing, |_, _| timing.dip) {
            out.push(Action::Volume { line: Line::Playback, volume, ramp });
        }
        out
    }

    /// When to poll next, if nothing else happens. Always later than `now`.
    pub fn next_wakeup(&self, now: Instant) -> Option<Instant> {
        let a = self.attached.as_ref()?;
        let params = (!a.pending_smooth && a.have != self.target.controls).then_some(a.next_params).flatten();
        let phase = match a.phase {
            Phase::DipHold { until } => Some(until),
            Phase::Linking { deadline } => Some(deadline),
            _ => None,
        };
        [a.capture.busy_until, a.playback.busy_until, params, phase].into_iter().flatten().filter(|&t| t > now).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omadsp_core::model::Preset;

    const MS: Duration = Duration::from_millis(1);

    fn timing() -> Timing {
        Timing {
            dip: Ramp { time_ms: 30, step_samples: 1 },
            gain_min_ms: 15,
            gain_ms_per_db: 5.0,
            gain_max_ms: 60,
            quantum: Duration::from_millis(1),
            margin: Duration::from_millis(50),
            params_interval: Duration::from_millis(16),
            link_timeout: Duration::from_millis(300),
        }
    }

    fn target(enabled: bool) -> Target {
        Target { enabled, ab: false, controls: controls::controls(&Preset::default()), preamp: 0.5 }
    }

    /// A volume action; `ms` = ramp time (1-sample steps), `None` = set directly.
    fn vol(line: Line, volume: f32, ms: Option<i32>) -> Action {
        Action::Volume { line, volume, ramp: ms.map(|time_ms| Ramp { time_ms, step_samples: 1 }) }
    }

    /// An applier attached to a filter that is enabled, linked, running and steady at `t0`.
    fn running_steady(t0: Instant) -> Applier {
        let mut ap = Applier::new(timing());
        ap.set_target(target(true), false);
        ap.attach(target(true).controls, false);
        ap.set_input_linked(true);
        ap.poll(t0);
        ap.set_running(t0, true);
        // Re-assert after resume (longest ramp + margin); from an unknown value: the longest ramp.
        // The capture line stays busy until 220 ms, playback until 190 ms.
        assert_eq!(ap.poll(t0 + 110 * MS), [vol(Line::Capture, 0.5, Some(60)), vol(Line::Playback, 1.0, Some(30))]);
        assert!(ap.is_active());
        ap
    }

    /// Polls at every wakeup until `end`, collecting (ms since `t0`, action).
    fn run(ap: &mut Applier, t0: Instant, from: Instant, end: Instant) -> Vec<(u128, Action)> {
        let mut out = Vec::new();
        let mut now = from;
        loop {
            for a in ap.poll(now) {
                out.push(((now - t0).as_millis(), a));
            }
            match ap.next_wakeup(now) {
                Some(n) if n <= end => {
                    assert!(n > now, "wakeup must move forward");
                    now = n;
                }
                _ => break,
            }
        }
        out
    }

    #[test]
    fn startup_sets_gains_then_enables_then_ramps_up_after_linking() {
        let t0 = Instant::now();
        let mut ap = Applier::new(timing());
        ap.set_target(target(true), false);
        ap.attach(target(true).controls, true);
        // Not running: direct values, switch the filter on, output still muted.
        assert_eq!(
            ap.poll(t0),
            [Action::SmartDisabled(false), vol(Line::Capture, 0.5, None), vol(Line::Playback, 0.0, None)]
        );
        assert!(!ap.is_active());
        // WirePlumber links a stream in; the node runs.
        ap.set_input_linked(true);
        ap.set_running(t0 + 40 * MS, true);
        assert_eq!(ap.poll(t0 + 40 * MS), []);
        // After the resume window, the output ramps up.
        let acts = run(&mut ap, t0, t0 + 41 * MS, t0 + 500 * MS);
        assert_eq!(acts, [(150, vol(Line::Capture, 0.5, Some(60))), (150, vol(Line::Playback, 1.0, Some(30)))]);
        assert!(ap.is_active());
    }

    #[test]
    fn startup_without_streams_unmutes_after_the_link_timeout() {
        let t0 = Instant::now();
        let mut ap = Applier::new(timing());
        ap.set_target(target(true), false);
        ap.attach(target(true).controls, true);
        ap.poll(t0);
        let acts = run(&mut ap, t0, t0 + MS, t0 + 1000 * MS);
        assert_eq!(acts, [(300, vol(Line::Playback, 1.0, None))]);
        assert!(ap.is_active());
    }

    #[test]
    fn startup_disabled_stays_muted_and_switched_off() {
        let t0 = Instant::now();
        let mut ap = Applier::new(timing());
        ap.set_target(target(false), false);
        ap.attach(target(false).controls, true);
        assert_eq!(ap.poll(t0), [vol(Line::Capture, 0.5, None), vol(Line::Playback, 0.0, None)]);
        assert!(ap.is_off());
        assert_eq!(ap.next_wakeup(t0), None);
    }

    /// `running_steady`, with `gains[0]` already active, so later moves are gain-only.
    fn running_with_band0(t0: Instant, g: f64) -> (Applier, Preset) {
        let mut ap = running_steady(t0);
        let mut p = Preset::default();
        p.eq.gains[0] = g;
        let mut tg = target(true);
        tg.controls = controls::controls(&p);
        ap.set_target(tg.clone(), false);
        ap.attach(tg.controls, false);
        ap.set_input_linked(true);
        ap.set_running(t0, true);
        run(&mut ap, t0, t0, t0 + 300 * MS);
        (ap, p)
    }

    fn with_gain(p: &mut Preset, g: f64) -> Target {
        p.eq.gains[0] = g;
        Target { controls: controls::controls(p), ..target(true) }
    }

    #[test]
    fn control_drags_are_coalesced_to_one_update_per_16ms() {
        let t0 = Instant::now();
        let (mut ap, mut p) = running_with_band0(t0, 0.5);
        let t1 = t0 + 300 * MS;
        for (i, g) in [1.0, 2.0, 3.0].into_iter().enumerate() {
            ap.set_target(with_gain(&mut p, g), false);
            let acts = ap.poll(t1 + (i as u32) * 5 * MS);
            if i == 0 {
                assert!(matches!(&acts[..], [Action::Params(_)]));
            } else {
                assert_eq!(acts, [], "within 16 ms of the last update");
            }
        }
        let acts = run(&mut ap, t0, t1 + 11 * MS, t1 + 100 * MS);
        assert_eq!(acts.len(), 1);
        let (at, Action::Params(ps)) = &acts[0] else { panic!("{acts:?}") };
        assert_eq!(*at, 316);
        let gl0 = ps.iter().find(|(k, _)| k == "eq:gl_0").unwrap().1;
        assert!((gl0 - 1.412_538).abs() < 1e-5, "only the latest value (3 dB) goes out");
    }

    #[test]
    fn dragging_through_zero_switches_the_band_at_unity() {
        let t0 = Instant::now();
        let (mut ap, mut p) = running_with_band0(t0, 1.0);
        let t1 = t0 + 300 * MS;
        let key = |acts: &[(u128, Action)], k: &str| -> Vec<(u128, f32)> {
            acts.iter()
                .filter_map(|(t, a)| match a {
                    Action::Params(ps) => ps.iter().find(|(n, _)| n == k).map(|(_, v)| (*t, *v)),
                    _ => None,
                })
                .collect()
        };
        // +1 dB -> -1 dB in one move: still a bell, gain only.
        ap.set_target(with_gain(&mut p, -1.0), false);
        let acts = run(&mut ap, t0, t1, t1 + 500 * MS);
        assert_eq!(key(&acts, "eq:ftl_0"), []);
        // -1 dB -> 0 dB (Off): gain to unity first, the type one margin later.
        ap.set_target(with_gain(&mut p, 0.0), false);
        let acts = run(&mut ap, t0, t1 + 500 * MS, t1 + 1000 * MS);
        assert_eq!(key(&acts, "eq:gl_0"), [(800, 1.0)]);
        assert_eq!(key(&acts, "eq:ftl_0"), [(850, 0.0)]);
        // 0 -> +2 dB: switch to bell at unity, the gain one margin later.
        ap.set_target(with_gain(&mut p, 2.0), false);
        let acts = run(&mut ap, t0, t1 + 1000 * MS, t1 + 1500 * MS);
        assert_eq!(key(&acts, "eq:ftl_0"), [(1300, 1.0)]);
        assert_eq!(key(&acts, "eq:gl_0").last().unwrap().0, 1350);
        assert!(!acts.iter().any(|(_, a)| matches!(a, Action::Volume { line: Line::Playback, .. })), "no dip");
    }

    #[test]
    fn a_high_pass_switching_on_waits_for_a_dip() {
        let t0 = Instant::now();
        let mut ap = running_steady(t0);
        let mut p = Preset::default();
        p.eq.mode = omadsp_core::model::EqMode::Parametric;
        p.parametric = vec![omadsp_core::model::ParamBand {
            kind: omadsp_core::model::FilterKind::HighPass,
            f: 80.0,
            ..Default::default()
        }];
        ap.set_target(Target { controls: controls::controls(&p), ..target(true) }, false);
        let acts = run(&mut ap, t0, t0 + 300 * MS, t0 + 1000 * MS);
        assert_eq!(acts[0], (300, vol(Line::Playback, 0.0, Some(30))));
        assert!(matches!(&acts[1], (380, Action::Params(_))));
        assert_eq!(acts.last().unwrap(), &(430, vol(Line::Playback, 1.0, Some(30))));
    }

    #[test]
    fn preamp_ramps_are_serialized_and_the_latest_wins() {
        let t0 = Instant::now();
        let mut ap = running_steady(t0);
        let t1 = t0 + 300 * MS;
        for (i, v) in [0.4, 0.3, 0.2].into_iter().enumerate() {
            ap.set_target(Target { preamp: v, ..target(true) }, false);
            let acts = ap.poll(t1 + (i as u32) * 10 * MS);
            if i == 0 {
                // -1.9 dB: 15 + 5 * 1.9 ms.
                assert_eq!(acts, [vol(Line::Capture, 0.4, Some(25))]);
            } else {
                assert_eq!(acts, []);
            }
        }
        let acts = run(&mut ap, t0, t1 + 21 * MS, t1 + 500 * MS);
        // After 25 + 50 ms; -6 dB from 0.4: 45 ms.
        assert_eq!(acts, [(375, vol(Line::Capture, 0.2, Some(45)))]);
    }

    #[test]
    fn ab_dips_then_bypasses_then_ramps_up() {
        let t0 = Instant::now();
        let mut ap = running_steady(t0);
        let t1 = t0 + 300 * MS;
        ap.set_target(Target { ab: true, ..target(true) }, false);
        let acts = run(&mut ap, t0, t1, t1 + 1000 * MS);
        assert_eq!(
            acts,
            [
                (300, vol(Line::Playback, 0.0, Some(30))),
                (380, Action::Params(stage_switches(false))),
                (380, vol(Line::Capture, 1.0, Some(45))),
                // Up once the preamp ramp is done (45 + 50 ms), not just one margin after.
                (475, vol(Line::Playback, 1.0, Some(30))),
            ]
        );
        // Release: the same dip, the other way.
        ap.set_target(target(true), false);
        let acts = run(&mut ap, t0, t1 + 1000 * MS, t1 + 2000 * MS);
        assert_eq!(acts[0], (1300, vol(Line::Playback, 0.0, Some(30))));
        assert_eq!(acts[1], (1380, Action::Params(stage_switches(true))));
        assert_eq!(acts[2], (1380, vol(Line::Capture, 0.5, Some(45))));
        assert_eq!(acts[3], (1475, vol(Line::Playback, 1.0, Some(30))));
        assert!(ap.is_active());
    }

    #[test]
    fn nothing_running_means_no_dip() {
        let t0 = Instant::now();
        let mut ap = running_steady(t0);
        ap.set_running(t0 + 250 * MS, false);
        ap.set_target(Target { ab: true, ..target(true) }, false);
        assert_eq!(
            ap.poll(t0 + 300 * MS),
            [Action::Params(stage_switches(false)), vol(Line::Capture, 1.0, None)],
            "applied at once, playback untouched"
        );
    }

    #[test]
    fn preset_switch_waits_for_the_dip() {
        let t0 = Instant::now();
        let mut ap = running_steady(t0);
        let t1 = t0 + 300 * MS;
        let warm = &omadsp_core::model::builtin_presets()[2];
        ap.set_target(Target { controls: controls::controls(warm), preamp: 0.7, ..target(true) }, true);
        let acts = run(&mut ap, t0, t1, t1 + 1000 * MS);
        assert_eq!(acts[0], (300, vol(Line::Playback, 0.0, Some(30))), "no params before the dip");
        assert!(matches!(&acts[1], (380, Action::Params(p)) if p.len() > 10));
        assert_eq!(acts[2], (380, vol(Line::Capture, 0.7, Some(30))), "+2.9 dB");
        assert_eq!(acts[3], (460, vol(Line::Playback, 1.0, Some(30))));
        assert_eq!(acts.len(), 4);
    }

    #[test]
    fn dsp_off_dips_then_switches_the_filter_off() {
        let t0 = Instant::now();
        let mut ap = running_steady(t0);
        let t1 = t0 + 300 * MS;
        ap.set_target(target(false), false);
        let acts = run(&mut ap, t0, t1, t1 + 1000 * MS);
        assert_eq!(acts, [(300, vol(Line::Playback, 0.0, Some(30))), (380, Action::SmartDisabled(true))]);
        assert!(ap.is_off());
        // And back on while streams are still running through it.
        ap.set_target(target(true), false);
        let acts = run(&mut ap, t0, t1 + 1000 * MS, t1 + 2000 * MS);
        assert_eq!(acts, [(1300, Action::SmartDisabled(false)), (1300, vol(Line::Playback, 1.0, Some(30)))]);
    }

    #[test]
    fn resume_reasserts_both_volumes_after_one_window() {
        let t0 = Instant::now();
        let mut ap = running_steady(t0);
        ap.set_running(t0 + 250 * MS, false);
        ap.set_target(Target { preamp: 0.25, ..target(true) }, false);
        assert_eq!(ap.poll(t0 + 260 * MS), [vol(Line::Capture, 0.25, None)]);
        ap.set_running(t0 + 400 * MS, true);
        let acts = run(&mut ap, t0, t0 + 400 * MS, t0 + 1000 * MS);
        assert_eq!(acts, [(510, vol(Line::Capture, 0.25, Some(60))), (510, vol(Line::Playback, 1.0, Some(30)))]);
    }

    #[test]
    fn metadata_reset_switches_the_filter_on_again() {
        let t0 = Instant::now();
        let mut ap = running_steady(t0);
        ap.set_running(t0 + 250 * MS, false);
        ap.set_input_linked(false);
        ap.smart_reset(true);
        let acts = ap.poll(t0 + 300 * MS);
        assert_eq!(acts, [Action::SmartDisabled(false), vol(Line::Playback, 0.0, None)]);
    }

    #[test]
    fn detached_does_nothing() {
        let mut ap = Applier::new(timing());
        ap.set_target(target(true), true);
        assert_eq!(ap.poll(Instant::now()), []);
        assert_eq!(ap.next_wakeup(Instant::now()), None);
        assert!(!ap.is_active());
    }

    #[test]
    fn timing_follows_the_quantum() {
        let t = Timing::for_clock(48000, 2048);
        assert_eq!(t.margin.as_millis(), 57);
        // 30 ms is one 42.7 ms cycle.
        assert_eq!(t.busy(t.dip).as_millis(), 100);
        assert_eq!(t.busy(Ramp { time_ms: 58, step_samples: 1 }).as_millis(), 143);
        assert_eq!(Timing::for_clock(44100, 8192).margin.as_millis(), 200);
    }

    #[test]
    fn gain_ramps_scale_with_the_step() {
        let t = timing();
        let ms = |from: Option<f32>, to: f32| t.gain_ramp(from, to).time_ms;
        assert_eq!(ms(Some(1.0), 1.0), 15);
        assert_eq!(ms(Some(0.5), 0.4), 25, "-1.9 dB");
        assert_eq!(ms(Some(0.4), 0.5), 25, "symmetric");
        assert_eq!(ms(Some(0.5), 1.0), 45, "+6 dB");
        assert_eq!(ms(Some(0.1), 1.0), 60, "capped");
        assert_eq!(ms(None, 0.5), 60, "unknown start");
        assert_eq!(ms(Some(0.0), 0.5), 60, "from silence");
        assert_eq!(t.gain_ramp(None, 0.5).step_samples, 1);
    }
}
