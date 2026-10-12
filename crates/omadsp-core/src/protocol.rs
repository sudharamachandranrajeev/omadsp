//! NDJSON v1 IPC (PROMPT §8.2): one JSON object per line over `$XDG_RUNTIME_DIR/omadsp/omadsp.sock`.
//!
//! - Request: `{"v":1,"id":"<client id>","type":"band.set","payload":{...}}`
//! - Response: `{"v":1,"id":<echoed>,"ok":true,"payload":{...}}` or
//!   `{"v":1,"id":<echoed>,"ok":false,"error":{"code":"invalid_value","message":"..."}}`
//! - Event: `{"v":1,"event":"state.changed","payload":{...}}`
//!
//! Every client receives `state.changed`, `curve.updated` and `device.changed`. Meter traffic is
//! opt-in with `meter.subscribe` (ADR-7): level `led` sends `overload` on changes only, level
//! `full` also sends `meter` at ~20 Hz. The daemon's post-chain tap runs only while some client
//! is subscribed.
//!
//! Additions to PROMPT §8.2's initial list: `dsp.toggle`, `preset.step`, `eq.setLayout`,
//! `eq.reset`, `meter.subscribe`, and the `overload` event. Payloads reject unknown fields, so a
//! typo is an error rather than a silent no-op.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{Channel, EqMode, FilterKind, ParamBand, Preset};

pub const VERSION: u32 = 1;
/// Socket file name inside `$XDG_RUNTIME_DIR/omadsp/`.
pub const SOCKET_NAME: &str = "omadsp.sock";

pub mod event {
    pub const STATE_CHANGED: &str = "state.changed";
    pub const DEVICE_CHANGED: &str = "device.changed";
    pub const CURVE_UPDATED: &str = "curve.updated";
    pub const METER: &str = "meter";
    pub const OVERLOAD: &str = "overload";
}

/// Request types PROMPT §8.2 lists for later phases. The daemon answers them with
/// `not_implemented` so clients can tell "not yet" from a typo.
pub const LATER_PHASE_TYPES: &[&str] = &[
    "preset.import",
    "preset.export",
    "stereo.set",
    "crossfeed.set",
    "spatial.set",
    "reverb.set",
    "leveler.set",
    "output.set",
    "profile.assign",
    "profile.clear",
    "autoeq.search",
    "autoeq.apply",
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    #[serde(default)]
    pub id: Value,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub payload: Value,
}

impl Request {
    pub fn new(id: impl Into<Value>, kind: &str, payload: Value) -> Request {
        Request { v: VERSION, id: id.into(), kind: kind.to_string(), payload }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Not a valid v1 request (bad JSON, wrong version, malformed payload).
    BadRequest,
    UnknownType,
    /// Listed in PROMPT §8.2 but belongs to a later phase.
    NotImplemented,
    /// A value is out of range or doesn't fit the current mode.
    InvalidValue,
    NotFound,
    /// The preset is locked or built in.
    Locked,
    Exists,
    /// The engine can't do it right now (no PipeWire, host failed).
    Unavailable,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> ErrorBody {
        ErrorBody { code, message: message.into() }
    }
}

impl std::fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = serde_json::to_value(self.code).ok();
        let code = code.as_ref().and_then(Value::as_str).unwrap_or("error");
        write!(f, "{code}: {}", self.message)
    }
}

impl std::error::Error for ErrorBody {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub v: u32,
    pub id: Value,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl Response {
    pub fn ok(id: Value, payload: Option<Value>) -> Response {
        Response { v: VERSION, id, ok: true, payload, error: None }
    }
    pub fn err(id: Value, error: ErrorBody) -> Response {
        Response { v: VERSION, id, ok: false, payload: None, error: Some(error) }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub v: u32,
    pub event: String,
    pub payload: Value,
}

impl Event {
    pub fn new(event: &str, payload: Value) -> Event {
        Event { v: VERSION, event: event.to_string(), payload }
    }
}

/// Anything the daemon sends, as a client parses it.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    Event(Event),
    Response(Response),
}

/// Serializes a message as one NDJSON line (with the trailing newline).
pub fn to_line<T: Serialize>(msg: &T) -> String {
    let mut s = serde_json::to_string(msg).expect("protocol types always serialize");
    s.push('\n');
    s
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MeterLevel {
    #[default]
    Off,
    Led,
    Full,
}

/// Partial update of a parametric band (`band.set`). In graphic mode only `g` applies.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BandPatch {
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<FilterKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ch: Option<Channel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub f: Option<f64>,
    #[serde(default, alias = "gain", skip_serializing_if = "Option::is_none")]
    pub g: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShelfPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub f: Option<f64>,
}

/// A parsed request.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    StateGet,
    DspSetEnabled {
        enabled: bool,
    },
    DspToggle,
    /// A/B: while held, every stage is bypassed (ADR-4).
    DspAbHold {
        hold: bool,
    },
    PresetApply {
        name: String,
    },
    /// Applies the next (`delta` 1) or previous (−1) preset in list order, wrapping around.
    PresetStep {
        delta: i32,
    },
    /// Saves the working copy, under `name` or the current preset's name.
    PresetSave {
        name: Option<String>,
    },
    PresetRename {
        from: String,
        to: String,
    },
    PresetDelete {
        name: String,
    },
    PresetLock {
        name: String,
        locked: bool,
    },
    EqSetMode {
        mode: EqMode,
    },
    EqSetLayout {
        bands: Option<usize>,
        range: Option<[f64; 2]>,
    },
    /// All user-EQ gains to 0 dB (graphic) or all parametric bands removed.
    EqReset,
    BandSet {
        index: usize,
        patch: BandPatch,
    },
    BandAdd {
        band: ParamBand,
        index: Option<usize>,
    },
    BandRemove {
        index: usize,
    },
    BandReorder {
        from: usize,
        to: usize,
    },
    ToneSet {
        bass: Option<ShelfPatch>,
        treble: Option<ShelfPatch>,
    },
    /// `db` sets the value, `delta` nudges it (the bar's scroll wheel).
    PreampSet {
        db: Option<f64>,
        delta: Option<f64>,
        auto: Option<bool>,
    },
    LimiterSet {
        ceiling_dbtp: Option<f64>,
        release_ms: Option<f64>,
    },
    MeterSubscribe {
        level: MeterLevel,
    },
}

fn payload<T: for<'de> Deserialize<'de>>(kind: &str, v: Value) -> Result<T, ErrorBody> {
    // A missing payload is an empty object, so all-optional payloads may be omitted.
    let v = if v.is_null() { Value::Object(Default::default()) } else { v };
    serde_json::from_value(v).map_err(|e| ErrorBody::new(ErrorCode::BadRequest, format!("{kind}: {e}")))
}

macro_rules! p {
    ($name:ident { $($(#[$m:meta])* $field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct $name { $($(#[$m])* $field: $ty),* }
    };
}

p!(Enabled { enabled: bool });
p!(Hold { hold: bool });
p!(Name { name: String });
p!(OptName { #[serde(default)] name: Option<String> });
p!(Step { delta: i32 });
p!(Rename { from: String, to: String });
p!(Lock { name: String, locked: bool });
p!(Mode { mode: EqMode });
p!(Layout { #[serde(default)] bands: Option<usize>, #[serde(default)] range: Option<[f64; 2]> });
p!(Index { index: usize });
p!(Reorder { from: usize, to: usize });
p!(Add { band: ParamBand, #[serde(default)] index: Option<usize> });
p!(ToneP { #[serde(default)] bass: Option<ShelfPatch>, #[serde(default)] treble: Option<ShelfPatch> });
p!(PreampP { #[serde(default)] db: Option<f64>, #[serde(default)] delta: Option<f64>, #[serde(default)] auto: Option<bool> });
p!(LimiterP { #[serde(default)] ceiling_dbtp: Option<f64>, #[serde(default)] release_ms: Option<f64> });
p!(Meter { level: MeterLevel });

#[derive(Deserialize)]
struct BandSetP {
    index: usize,
    #[serde(flatten)]
    patch: Value,
}

fn finite(name: &str, v: Option<f64>) -> Result<(), ErrorBody> {
    match v {
        Some(x) if !x.is_finite() => Err(ErrorBody::new(ErrorCode::InvalidValue, format!("{name} must be finite"))),
        _ => Ok(()),
    }
}

impl Command {
    /// Parses a request's `type` and `payload`. Range checks against the current state are the
    /// daemon's job; this only checks shape.
    pub fn parse(req: &Request) -> Result<Command, ErrorBody> {
        if req.v != VERSION {
            return Err(ErrorBody::new(ErrorCode::BadRequest, format!("unsupported protocol version {}", req.v)));
        }
        let k = req.kind.as_str();
        let v = req.payload.clone();
        let cmd = match k {
            "state.get" => Command::StateGet,
            "dsp.setEnabled" => Command::DspSetEnabled { enabled: payload::<Enabled>(k, v)?.enabled },
            "dsp.toggle" => Command::DspToggle,
            "dsp.abHold" => Command::DspAbHold { hold: payload::<Hold>(k, v)?.hold },
            "preset.apply" => Command::PresetApply { name: payload::<Name>(k, v)?.name },
            "preset.step" => Command::PresetStep { delta: payload::<Step>(k, v)?.delta },
            "preset.save" => Command::PresetSave { name: payload::<OptName>(k, v)?.name },
            "preset.rename" => {
                let p: Rename = payload(k, v)?;
                Command::PresetRename { from: p.from, to: p.to }
            }
            "preset.delete" => Command::PresetDelete { name: payload::<Name>(k, v)?.name },
            "preset.lock" => {
                let p: Lock = payload(k, v)?;
                Command::PresetLock { name: p.name, locked: p.locked }
            }
            "eq.setMode" => Command::EqSetMode { mode: payload::<Mode>(k, v)?.mode },
            "eq.setLayout" => {
                let p: Layout = payload(k, v)?;
                if let Some(r) = p.range {
                    finite("range", Some(r[0]))?;
                    finite("range", Some(r[1]))?;
                }
                Command::EqSetLayout { bands: p.bands, range: p.range }
            }
            "eq.reset" => Command::EqReset,
            "band.set" => {
                let p: BandSetP = payload(k, v)?;
                let patch: BandPatch = payload(k, p.patch)?;
                for (n, x) in [("f", patch.f), ("g", patch.g), ("q", patch.q)] {
                    finite(n, x)?;
                }
                Command::BandSet { index: p.index, patch }
            }
            "band.add" => {
                let p: Add = payload(k, v)?;
                for (n, x) in [("f", p.band.f), ("g", p.band.g), ("q", p.band.q)] {
                    finite(n, Some(x))?;
                }
                Command::BandAdd { band: p.band, index: p.index }
            }
            "band.remove" => Command::BandRemove { index: payload::<Index>(k, v)?.index },
            "band.reorder" => {
                let p: Reorder = payload(k, v)?;
                Command::BandReorder { from: p.from, to: p.to }
            }
            "tone.set" => {
                let p: ToneP = payload(k, v)?;
                for s in [&p.bass, &p.treble].into_iter().flatten() {
                    finite("db", s.db)?;
                    finite("f", s.f)?;
                }
                Command::ToneSet { bass: p.bass, treble: p.treble }
            }
            "preamp.set" => {
                let p: PreampP = payload(k, v)?;
                finite("db", p.db)?;
                finite("delta", p.delta)?;
                if p.db.is_some() && p.delta.is_some() {
                    return Err(ErrorBody::new(ErrorCode::BadRequest, "preamp.set: give db or delta, not both"));
                }
                Command::PreampSet { db: p.db, delta: p.delta, auto: p.auto }
            }
            "limiter.set" => {
                let p: LimiterP = payload(k, v)?;
                finite("ceiling_dbtp", p.ceiling_dbtp)?;
                finite("release_ms", p.release_ms)?;
                Command::LimiterSet { ceiling_dbtp: p.ceiling_dbtp, release_ms: p.release_ms }
            }
            "meter.subscribe" => Command::MeterSubscribe { level: payload::<Meter>(k, v)?.level },
            _ if LATER_PHASE_TYPES.contains(&k) => {
                return Err(ErrorBody::new(ErrorCode::NotImplemented, format!("{k} arrives in a later phase")));
            }
            _ => return Err(ErrorBody::new(ErrorCode::UnknownType, format!("unknown request type {k:?}"))),
        };
        Ok(cmd)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    /// DSP is off: the smart filter is disabled and streams go straight to the device.
    #[default]
    Off,
    Starting,
    Running,
    /// The filter host gave up (crash loop or a broken graph); audio bypasses omadsp.
    Failed,
    /// No connection to PipeWire.
    NoServer,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PresetInfo {
    pub name: String,
    pub builtin: bool,
    pub locked: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// PipeWire `node.name` of the device omadsp feeds.
    pub node: String,
    pub description: String,
    /// Stable identity for profiles (never a node id).
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<u32>,
}

/// The `state.get` result and the `state.changed` payload.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub enabled: bool,
    pub ab: bool,
    pub engine: Engine,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub preset: String,
    pub dirty: bool,
    pub working: Preset,
    pub presets: Vec<PresetInfo>,
    pub device: Option<DeviceInfo>,
    /// Gain applied at the head of the chain: `working.preamp.db` plus the auto part.
    pub preamp_db: f64,
    /// The auto-headroom part (≤ 0), 0 when auto is off.
    pub headroom_db: f64,
    pub overload: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(kind: &str, payload: Value) -> Request {
        Request::new("1", kind, payload)
    }

    #[test]
    fn parses_band_set() {
        let c = Command::parse(&req("band.set", json!({"index": 3, "g": -2.5}))).unwrap();
        assert_eq!(c, Command::BandSet { index: 3, patch: BandPatch { g: Some(-2.5), ..Default::default() } });
        let c =
            Command::parse(&req("band.set", json!({"index": 0, "type": "lowshelf", "ch": "l", "gain": 1}))).unwrap();
        let Command::BandSet { patch, .. } = c else { panic!() };
        assert_eq!((patch.kind, patch.ch, patch.g), (Some(FilterKind::LowShelf), Some(Channel::Left), Some(1.0)));
    }

    #[test]
    fn rejects_typos_and_bad_values() {
        let e = Command::parse(&req("band.set", json!({"index": 3, "gian": 1}))).unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest);
        let e = Command::parse(&req("dsp.setEnabled", json!({}))).unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest);
        let e = Command::parse(&req("preamp.set", json!({"db": 1, "delta": 1}))).unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest);
        let e = Command::parse(&Request { v: 2, ..req("state.get", Value::Null) }).unwrap_err();
        assert_eq!(e.code, ErrorCode::BadRequest);
    }

    #[test]
    fn unknown_vs_later_phase() {
        assert_eq!(Command::parse(&req("bogus", Value::Null)).unwrap_err().code, ErrorCode::UnknownType);
        assert_eq!(Command::parse(&req("spatial.set", json!({}))).unwrap_err().code, ErrorCode::NotImplemented);
    }

    #[test]
    fn optional_payloads_may_be_omitted() {
        assert_eq!(Command::parse(&req("state.get", Value::Null)).unwrap(), Command::StateGet);
        assert_eq!(Command::parse(&req("preset.save", Value::Null)).unwrap(), Command::PresetSave { name: None });
        assert_eq!(
            Command::parse(&req("preamp.set", json!({"delta": 0.5}))).unwrap(),
            Command::PreampSet { db: None, delta: Some(0.5), auto: None }
        );
    }

    #[test]
    fn wire_format() {
        let line = to_line(&Response::err(json!("a"), ErrorBody::new(ErrorCode::NotFound, "no preset \"X\"")));
        assert_eq!(
            line,
            "{\"v\":1,\"id\":\"a\",\"ok\":false,\"error\":{\"code\":\"not_found\",\"message\":\"no preset \\\"X\\\"\"}}\n"
        );
        assert_eq!(to_line(&Response::ok(json!(7), None)), "{\"v\":1,\"id\":7,\"ok\":true}\n");
        let r: Request = serde_json::from_str(r#"{"v":1,"id":"u","type":"state.get"}"#).unwrap();
        assert_eq!(r.payload, Value::Null);
    }

    #[test]
    fn client_tells_events_from_responses() {
        let ev: ServerMessage =
            serde_json::from_str(r#"{"v":1,"event":"overload","payload":{"active":true}}"#).unwrap();
        assert!(matches!(ev, ServerMessage::Event(e) if e.event == event::OVERLOAD));
        let r: ServerMessage = serde_json::from_str(r#"{"v":1,"id":"x","ok":true,"payload":{}}"#).unwrap();
        assert!(matches!(r, ServerMessage::Response(r) if r.ok));
    }

    #[test]
    fn snapshot_round_trips() {
        let s = Snapshot { enabled: true, engine: Engine::Running, preset: "Flat".into(), ..Default::default() };
        let back: Snapshot = serde_json::from_value(serde_json::to_value(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
