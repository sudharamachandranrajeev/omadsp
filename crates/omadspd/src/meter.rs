//! The post-chain tap and the overload LED (ADR-7).
//!
//! - [`Tap`]: a passive capture stream (`node.autoconnect = false`) that the engine links by hand
//!   to the filter's playback-node outputs, so WirePlumber's smart-filter diversion can't move it.
//!   Its process callback (main loop, not RT) folds samples into 50 ms windows and reports each
//!   window's per-channel peak.
//! - [`Led`]: on when a window's peak is within [`LED_MARGIN_DB`] of the limiter ceiling (the
//!   limiter is working, or the output sits at the ceiling), held for [`LED_HOLD`].
//!
//! The tap only exists while some client subscribes to meter traffic.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use pipewire as pw;
use pw::spa;

/// Peak window length (20 windows per second).
pub const WINDOW: Duration = Duration::from_millis(50);
/// How close to the ceiling a peak must be to light the LED. True-peak limiting leaves sample
/// peaks up to ~0.5 dB under the ceiling (Spike E2).
pub const LED_MARGIN_DB: f64 = 0.5;
pub const LED_HOLD: Duration = Duration::from_millis(500);
/// Peaks below this read as silence.
pub const FLOOR_DB: f64 = -120.0;

pub fn lin_to_db(x: f32) -> f64 {
    if x > 0.0 { (20.0 * f64::from(x).log10()).max(FLOOR_DB) } else { FLOOR_DB }
}

/// Folds interleaved stereo frames into fixed-length windows.
#[derive(Debug)]
pub struct Windows {
    frames_per_window: usize,
    frames: usize,
    peak: [f32; 2],
}

impl Windows {
    pub fn new(rate: u32) -> Windows {
        Windows { frames_per_window: Windows::frames_for(rate), frames: 0, peak: [0.0; 2] }
    }

    fn frames_for(rate: u32) -> usize {
        ((u128::from(rate.max(1)) * WINDOW.as_millis()) / 1000).max(1) as usize
    }

    pub fn set_rate(&mut self, rate: u32) {
        self.frames_per_window = Windows::frames_for(rate);
    }

    /// Feeds interleaved FL/FR samples; calls `done` with the peaks (linear) of each finished window.
    pub fn feed(&mut self, samples: &[f32], mut done: impl FnMut([f32; 2])) {
        for f in samples.chunks_exact(2) {
            self.peak[0] = self.peak[0].max(f[0].abs());
            self.peak[1] = self.peak[1].max(f[1].abs());
            self.frames += 1;
            if self.frames >= self.frames_per_window {
                done(self.peak);
                self.frames = 0;
                self.peak = [0.0; 2];
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct Led {
    last_hot: Option<Instant>,
}

impl Led {
    /// Takes one window's peaks (dBFS) against the limiter ceiling (dBTP).
    pub fn window(&mut self, now: Instant, peak_db: [f64; 2], ceiling_db: f64) {
        if peak_db.iter().any(|&p| p >= ceiling_db - LED_MARGIN_DB) {
            self.last_hot = Some(now);
        }
    }

    pub fn on(&self, now: Instant) -> bool {
        self.last_hot.is_some_and(|t| now.duration_since(t) < LED_HOLD)
    }

    /// When the LED goes off unless another hot window arrives.
    pub fn next_change(&self, now: Instant) -> Option<Instant> {
        self.last_hot.map(|t| t + LED_HOLD).filter(|&t| t > now)
    }

    pub fn reset(&mut self) {
        self.last_hot = None;
    }
}

/// The tap stream. Field order = drop order: the listener before its stream.
pub struct Tap {
    pub name: String,
    windows: Rc<RefCell<Windows>>,
    _listener: pw::stream::StreamListener<()>,
    _stream: pw::stream::StreamRc,
}

impl Tap {
    /// Creates the stream `name` (unlinked). `on_window` gets each window's peaks (linear).
    pub fn new(core: &pw::core::CoreRc, name: &str, rate: u32, on_window: impl Fn([f32; 2]) + 'static) -> Result<Tap> {
        let props = pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "DSP",
            *pw::keys::NODE_NAME => name,
            "node.description" => "omadsp meter",
            "node.autoconnect" => "false",
            "node.passive" => "true",
            "node.dont-reconnect" => "true",
            "state.restore-props" => "false",
            "state.restore-target" => "false",
        };
        let stream = pw::stream::StreamRc::new(core.clone(), name, props)?;
        let windows = Rc::new(RefCell::new(Windows::new(rate)));
        let w = windows.clone();
        let listener = stream
            .add_local_listener::<()>()
            .process(move |s, _| {
                let Some(mut buf) = s.dequeue_buffer() else { return };
                let Some(d) = buf.datas_mut().first_mut() else { return };
                let (off, size) = (d.chunk().offset() as usize, d.chunk().size() as usize);
                let Some(bytes) = d.data() else { return };
                let bytes = &bytes[off.min(bytes.len())..(off + size).min(bytes.len())];
                let samples: Vec<f32> =
                    bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
                w.borrow_mut().feed(&samples, &on_window);
            })
            .register()?;

        let mut info = spa::param::audio::AudioInfoRaw::new();
        info.set_format(spa::param::audio::AudioFormat::F32LE);
        info.set_channels(2);
        let mut pos = [0u32; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
        pos[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
        pos[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
        info.set_position(pos);
        let obj = spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        };
        let bytes = spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &spa::pod::Value::Object(obj),
        )?
        .0
        .into_inner();
        let mut params = [spa::pod::Pod::from_bytes(&bytes).context("format pod")?];
        stream.connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::DONT_RECONNECT,
            &mut params,
        )?;
        Ok(Tap { name: name.to_string(), windows, _listener: listener, _stream: stream })
    }

    pub fn set_rate(&self, rate: u32) {
        self.windows.borrow_mut().set_rate(rate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_are_50_ms() {
        let mut w = Windows::new(48000);
        let mut got = Vec::new();
        let mut frames = vec![0.0f32; 2 * 4800];
        frames[2 * 100] = 0.5;
        frames[2 * 3000 + 1] = -0.25;
        w.feed(&frames[..2 * 3000], |p| got.push(p));
        w.feed(&frames[2 * 3000..], |p| got.push(p));
        assert_eq!(got, vec![[0.5, 0.0], [0.0, 0.25]]);
        w.set_rate(44100);
        w.feed(&vec![0.1; 2 * 2205], |p| got.push(p));
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn led_lights_near_the_ceiling_and_holds() {
        let t0 = Instant::now();
        let mut led = Led::default();
        led.window(t0, [-1.6, -3.0], -1.0);
        assert!(!led.on(t0), "0.6 dB under the ceiling is not hot");
        led.window(t0, [-1.4, -3.0], -1.0);
        assert!(led.on(t0));
        assert_eq!(led.next_change(t0), Some(t0 + LED_HOLD));
        assert!(led.on(t0 + Duration::from_millis(499)));
        assert!(!led.on(t0 + LED_HOLD));
        assert_eq!(led.next_change(t0 + LED_HOLD), None);
    }

    #[test]
    fn db() {
        assert_eq!(lin_to_db(0.0), FLOOR_DB);
        assert!((lin_to_db(0.5) + 6.0206).abs() < 1e-3);
    }
}
