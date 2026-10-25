//! `omadsp`: the command line for omadspd, for keybinds and scripts (PROMPT §8.3).
//!
//! Each run connects to the daemon's socket, sends one request (two for `ab` without an
//! argument), prints the result and exits: 0 on success, 1 when the daemon refused the request,
//! 2 when it can't be reached. Band numbers start at 1 here (the protocol counts from 0).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::{Map, Value, json};

use omadsp_core::model::{EqMode, Preset};
use omadsp_core::protocol::{self, Request, ServerMessage, Snapshot};

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(name = "omadsp", version, about = "Control the omadsp system-wide DSP")]
struct Cli {
    /// Daemon socket [default: $XDG_RUNTIME_DIR/omadsp/omadsp.sock]
    #[arg(long, global = true, env = "OMADSP_SOCKET")]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Switch the DSP on or off
    Toggle,
    /// Switch the DSP on
    On,
    /// Switch the DSP off
    Off,
    /// A/B: bypass every stage while on (no argument toggles)
    Ab { state: Option<OnOff> },
    /// Presets: next, prev, list, save, rename, delete, lock, unlock, or a preset name to apply
    #[command(subcommand)]
    Preset(PresetCmd),
    /// Set band N (from 1): its gain, and in parametric mode its other fields
    #[command(allow_negative_numbers = true)]
    Band(BandArgs),
    /// Add a parametric band
    #[command(allow_negative_numbers = true)]
    BandAdd(NewBand),
    /// Remove parametric band N (from 1)
    BandRemove { band: usize },
    /// Move parametric band FROM to position TO (from 1)
    BandMove { from: usize, to: usize },
    /// Choose the EQ mode
    Mode { mode: Mode },
    /// Graphic EQ layout: band count and frequency range
    Layout {
        #[arg(long)]
        bands: Option<usize>,
        #[arg(long, num_args = 2, value_names = ["LO", "HI"])]
        range: Option<Vec<f64>>,
    },
    /// All graphic gains to 0 dB, or every parametric band removed
    Reset,
    /// Preamp: a value in dB, or --delta to nudge it; --auto adds automatic headroom
    #[command(allow_negative_numbers = true)]
    Preamp {
        db: Option<f64>,
        #[arg(long, conflicts_with = "db")]
        delta: Option<f64>,
        #[arg(long, conflicts_with = "manual")]
        auto: bool,
        #[arg(long)]
        manual: bool,
    },
    /// Bass and treble shelves
    #[command(allow_negative_numbers = true)]
    Tone {
        #[arg(long)]
        bass: Option<f64>,
        #[arg(long, value_name = "HZ")]
        bass_freq: Option<f64>,
        #[arg(long)]
        treble: Option<f64>,
        #[arg(long, value_name = "HZ")]
        treble_freq: Option<f64>,
    },
    /// Limiter ceiling (dBTP) and release (ms)
    #[command(allow_negative_numbers = true)]
    Limiter {
        #[arg(long, value_name = "DBTP")]
        ceiling: Option<f64>,
        #[arg(long, value_name = "MS")]
        release: Option<f64>,
    },
    /// Show the state
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Print every event the daemon sends, one JSON object per line
    Watch,
    /// Show post-DSP peaks (~20 per second) and the overload LED; --led shows only LED changes
    Meter {
        #[arg(long)]
        led: bool,
        /// One JSON object per event
        #[arg(long)]
        json: bool,
    },
    /// Send a raw request: TYPE and an optional JSON payload
    Raw { kind: String, payload: Option<String> },
}

#[derive(Subcommand)]
enum PresetCmd {
    Next,
    Prev,
    List,
    /// Save the working copy, under NAME or the current preset's name
    Save {
        name: Vec<String>,
    },
    Rename {
        from: String,
        to: String,
    },
    Delete {
        name: Vec<String>,
    },
    Lock {
        name: Vec<String>,
    },
    Unlock {
        name: Vec<String>,
    },
    /// Apply a preset by name
    Apply {
        name: Vec<String>,
    },
    #[command(external_subcommand)]
    Name(Vec<String>),
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Graphic,
    Parametric,
}

#[derive(Clone, Copy, ValueEnum)]
enum Kind {
    Peak,
    Lowshelf,
    Highshelf,
    Lowpass,
    Highpass,
    Bandpass,
    Notch,
}

#[derive(Clone, Copy, ValueEnum)]
enum Ch {
    Both,
    Left,
    Right,
}

fn enum_str<T: ValueEnum>(v: T) -> String {
    v.to_possible_value().expect("no skipped variants").get_name().to_string()
}

#[derive(Args)]
struct BandArgs {
    band: usize,
    gain: Option<f64>,
    #[arg(long = "type")]
    kind: Option<Kind>,
    #[arg(long)]
    ch: Option<Ch>,
    #[arg(long)]
    freq: Option<f64>,
    #[arg(long)]
    q: Option<f64>,
    /// A colour for the band's handle, e.g. "#e06c75"; empty clears it
    #[arg(long)]
    color: Option<String>,
    #[arg(long, conflicts_with = "off")]
    on: bool,
    #[arg(long)]
    off: bool,
}

#[derive(Args)]
struct NewBand {
    #[arg(long = "type", default_value = "peak")]
    kind: Kind,
    #[arg(long, default_value = "both")]
    ch: Ch,
    #[arg(long)]
    freq: f64,
    #[arg(long, default_value_t = 0.0)]
    gain: f64,
    #[arg(long, default_value_t = 1.0)]
    q: f64,
    /// Position (from 1); the end by default
    #[arg(long)]
    at: Option<usize>,
}

fn index(n: usize) -> Result<usize> {
    n.checked_sub(1).context("bands are numbered from 1")
}

fn name(words: &[String]) -> Option<String> {
    let n = words.join(" ");
    (!n.trim().is_empty()).then_some(n)
}

fn need_name(words: &[String]) -> Result<String> {
    name(words).context("give a preset name")
}

/// Puts the `Some` fields of `pairs` into a JSON object.
fn obj(pairs: &[(&str, Option<Value>)]) -> Value {
    let m: Map<String, Value> = pairs.iter().filter_map(|(k, v)| v.clone().map(|v| (k.to_string(), v))).collect();
    Value::Object(m)
}

struct Conn {
    tx: UnixStream,
    rx: BufReader<UnixStream>,
    next_id: u64,
}

impl Conn {
    fn open(path: &PathBuf) -> std::io::Result<Conn> {
        let tx = UnixStream::connect(path)?;
        tx.set_read_timeout(Some(TIMEOUT))?;
        tx.set_write_timeout(Some(TIMEOUT))?;
        let rx = BufReader::new(tx.try_clone()?);
        Ok(Conn { tx, rx, next_id: 1 })
    }

    fn line(&mut self) -> Result<Option<String>> {
        let mut s = String::new();
        if self.rx.read_line(&mut s).context("reading from the daemon")? == 0 {
            return Ok(None);
        }
        Ok(Some(s))
    }

    /// Sends a request and waits for its response, skipping events. `Err` carries the daemon's error.
    fn call(&mut self, kind: &str, payload: Value) -> Result<Option<Value>> {
        let id = self.next_id;
        self.next_id += 1;
        let req = Request::new(id, kind, payload);
        self.tx.write_all(protocol::to_line(&req).as_bytes()).context("writing to the daemon")?;
        loop {
            let line = self.line()?.context("the daemon closed the connection")?;
            match serde_json::from_str::<ServerMessage>(&line) {
                Ok(ServerMessage::Response(r)) if r.id == json!(id) => {
                    return match r.error {
                        Some(e) if !r.ok => Err(anyhow::anyhow!(e)),
                        _ => Ok(r.payload),
                    };
                }
                Ok(_) => {}
                Err(e) => bail!("unexpected message from the daemon: {e}"),
            }
        }
    }

    fn snapshot(&mut self) -> Result<Snapshot> {
        let v = self.call("state.get", Value::Null)?.context("state.get returned nothing")?;
        Ok(serde_json::from_value(v)?)
    }
}

fn socket_path(cli: &Cli) -> Result<PathBuf> {
    if let Some(p) = &cli.socket {
        return Ok(p.clone());
    }
    let rt = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set; use --socket")?;
    Ok(PathBuf::from(rt).join("omadsp").join(protocol::SOCKET_NAME))
}

fn fmt_db(v: f64) -> String {
    format!("{v:+.1} dB")
}

fn fmt_hz(f: f64) -> String {
    if f >= 1000.0 { format!("{:.1} kHz", f / 1000.0).replace(".0 ", " ") } else { format!("{f:.0} Hz") }
}

fn print_status(s: &Snapshot) {
    let w: &Preset = &s.working;
    let engine = serde_json::to_value(s.engine).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
    println!(
        "DSP {} ({engine}){}{}",
        if s.enabled { "on" } else { "off" },
        if s.ab { ", A/B bypass held" } else { "" },
        s.error.as_ref().map(|e| format!(": {e}")).unwrap_or_default()
    );
    println!("preset: {}{}", s.preset, if s.dirty { " (edited)" } else { "" });
    match &s.device {
        Some(d) => println!("device: {} ({})", d.description, d.node),
        None => println!("device: none"),
    }
    println!(
        "preamp: {}{}",
        fmt_db(s.preamp_db),
        if w.preamp.auto {
            format!(" (auto: {} headroom, {} set)", fmt_db(s.headroom_db), fmt_db(w.preamp.db))
        } else {
            String::new()
        }
    );
    match w.eq.mode {
        EqMode::Graphic => {
            println!("eq: graphic, {} bands, {}–{}", w.eq.bands, fmt_hz(w.eq.range[0]), fmt_hz(w.eq.range[1]));
            for (i, (f, g)) in w.graphic_freqs().iter().zip(&w.eq.gains).enumerate() {
                println!("  {:>2}  {:>9}  {}", i + 1, fmt_hz(*f), fmt_db(*g));
            }
        }
        EqMode::Parametric => {
            println!("eq: parametric, {} band(s)", w.parametric.len());
            for (i, b) in w.parametric.iter().enumerate() {
                let kind =
                    serde_json::to_value(b.kind).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
                let ch = serde_json::to_value(b.ch).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
                println!(
                    "  {:>2}  {kind:<9} {:>9}  {}  Q {:.2}  {ch}{}",
                    i + 1,
                    fmt_hz(b.f),
                    fmt_db(b.g),
                    b.q,
                    if b.on { "" } else { "  (off)" }
                );
            }
        }
    }
    println!(
        "tone: bass {} at {}, treble {} at {}",
        fmt_db(w.tone.bass.db),
        fmt_hz(w.tone.bass.f),
        fmt_db(w.tone.treble.db),
        fmt_hz(w.tone.treble.f)
    );
    println!("limiter: {:.1} dBTP, {} ms release", w.limiter.ceiling_dbtp, w.limiter.release_ms);
}

fn run(cli: Cli) -> Result<()> {
    let path = socket_path(&cli)?;
    let mut c = Conn::open(&path).map_err(|e| NoDaemon(format!("{}: {e}", path.display())))?;
    let (kind, payload): (&str, Value) = match cli.cmd {
        Cmd::Toggle => ("dsp.toggle", Value::Null),
        Cmd::On => ("dsp.setEnabled", json!({"enabled": true})),
        Cmd::Off => ("dsp.setEnabled", json!({"enabled": false})),
        Cmd::Ab { state } => {
            let hold = match state {
                Some(s) => matches!(s, OnOff::On),
                None => !c.snapshot()?.ab,
            };
            ("dsp.abHold", json!({"hold": hold}))
        }
        Cmd::Preset(p) => match p {
            PresetCmd::Next => ("preset.step", json!({"delta": 1})),
            PresetCmd::Prev => ("preset.step", json!({"delta": -1})),
            PresetCmd::List => {
                let s = c.snapshot()?;
                for p in &s.presets {
                    let mark = if p.name.eq_ignore_ascii_case(&s.preset) { "*" } else { " " };
                    let tag = if p.builtin {
                        "  (built in)"
                    } else if p.locked {
                        "  (locked)"
                    } else {
                        ""
                    };
                    println!("{mark} {}{tag}", p.name);
                }
                return Ok(());
            }
            PresetCmd::Save { name: n } => ("preset.save", obj(&[("name", name(&n).map(Value::from))])),
            PresetCmd::Rename { from, to } => ("preset.rename", json!({"from": from, "to": to})),
            PresetCmd::Delete { name: n } => ("preset.delete", json!({"name": need_name(&n)?})),
            PresetCmd::Lock { name: n } => ("preset.lock", json!({"name": need_name(&n)?, "locked": true})),
            PresetCmd::Unlock { name: n } => ("preset.lock", json!({"name": need_name(&n)?, "locked": false})),
            PresetCmd::Apply { name: n } | PresetCmd::Name(n) => ("preset.apply", json!({"name": need_name(&n)?})),
        },
        Cmd::Band(b) => {
            let on = if b.on {
                Some(true)
            } else if b.off {
                Some(false)
            } else {
                None
            };
            let p = obj(&[
                ("index", Some(json!(index(b.band)?))),
                ("g", b.gain.map(Value::from)),
                ("type", b.kind.map(|k| enum_str(k).into())),
                ("ch", b.ch.map(|k| enum_str(k).into())),
                ("f", b.freq.map(Value::from)),
                ("q", b.q.map(Value::from)),
                ("color", b.color.map(Value::from)),
                ("on", on.map(Value::from)),
            ]);
            if p.as_object().is_some_and(|m| m.len() == 1) {
                bail!("give a gain or at least one --option");
            }
            ("band.set", p)
        }
        Cmd::BandAdd(b) => {
            let band =
                json!({"type": enum_str(b.kind), "ch": enum_str(b.ch), "f": b.freq, "g": b.gain, "q": b.q, "on": true});
            let at = b.at.map(index).transpose()?;
            ("band.add", obj(&[("band", Some(band)), ("index", at.map(Value::from))]))
        }
        Cmd::BandRemove { band } => ("band.remove", json!({"index": index(band)?})),
        Cmd::BandMove { from, to } => ("band.reorder", json!({"from": index(from)?, "to": index(to)?})),
        Cmd::Mode { mode } => ("eq.setMode", json!({"mode": enum_str(mode)})),
        Cmd::Layout { bands, range } => {
            ("eq.setLayout", obj(&[("bands", bands.map(Value::from)), ("range", range.map(|r| json!(r)))]))
        }
        Cmd::Reset => ("eq.reset", Value::Null),
        Cmd::Preamp { db, delta, auto, manual } => {
            let auto = if auto {
                Some(true)
            } else if manual {
                Some(false)
            } else {
                None
            };
            if db.is_none() && delta.is_none() && auto.is_none() {
                bail!("give a value in dB, --delta, --auto or --manual");
            }
            (
                "preamp.set",
                obj(&[("db", db.map(Value::from)), ("delta", delta.map(Value::from)), ("auto", auto.map(Value::from))]),
            )
        }
        Cmd::Tone { bass, bass_freq, treble, treble_freq } => {
            let shelf = |db: Option<f64>, f: Option<f64>| {
                (db.is_some() || f.is_some()).then(|| obj(&[("db", db.map(Value::from)), ("f", f.map(Value::from))]))
            };
            let p = obj(&[("bass", shelf(bass, bass_freq)), ("treble", shelf(treble, treble_freq))]);
            if p.as_object().is_some_and(|m| m.is_empty()) {
                bail!("give --bass, --treble or a frequency");
            }
            ("tone.set", p)
        }
        Cmd::Limiter { ceiling, release } => {
            if ceiling.is_none() && release.is_none() {
                bail!("give --ceiling or --release");
            }
            (
                "limiter.set",
                obj(&[("ceiling_dbtp", ceiling.map(Value::from)), ("release_ms", release.map(Value::from))]),
            )
        }
        Cmd::Status { json } => {
            let v = c.call("state.get", Value::Null)?.context("state.get returned nothing")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print_status(&serde_json::from_value(v)?);
            }
            return Ok(());
        }
        Cmd::Watch => {
            c.tx.set_read_timeout(None)?;
            while let Some(line) = c.line()? {
                print!("{line}");
                std::io::stdout().flush()?;
            }
            return Ok(());
        }
        Cmd::Meter { led, json } => {
            c.call("meter.subscribe", json!({"level": if led { "led" } else { "full" }}))?;
            c.tx.set_read_timeout(None)?;
            while let Some(line) = c.line()? {
                let Ok(ServerMessage::Event(e)) = serde_json::from_str::<ServerMessage>(&line) else { continue };
                if e.event != protocol::event::METER && e.event != protocol::event::OVERLOAD {
                    continue;
                }
                if json {
                    print!("{line}");
                } else if e.event == protocol::event::OVERLOAD {
                    println!("overload {}", if e.payload["on"] == json!(true) { "ON" } else { "off" });
                } else {
                    let p = &e.payload["peak_db"];
                    let lit = if e.payload["overload"] == json!(true) { "  ● OVERLOAD" } else { "" };
                    println!(
                        "L {:>6.1}  R {:>6.1} dBFS{lit}",
                        p[0].as_f64().unwrap_or(f64::NAN),
                        p[1].as_f64().unwrap_or(f64::NAN)
                    );
                }
                std::io::stdout().flush()?;
            }
            return Ok(());
        }
        Cmd::Raw { kind, payload } => {
            let p = match payload {
                Some(t) => serde_json::from_str(&t).context("the payload is not JSON")?,
                None => Value::Null,
            };
            if let Some(v) = c.call(&kind, p)? {
                println!("{}", serde_json::to_string_pretty(&v)?);
            }
            return Ok(());
        }
    };
    c.call(kind, payload)?;
    Ok(())
}

/// The daemon isn't reachable.
#[derive(Debug)]
struct NoDaemon(String);
impl std::fmt::Display for NoDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "omadspd is not running ({})", self.0)
    }
}
impl std::error::Error for NoDaemon {}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("omadsp: {e:#}");
            ExitCode::from(if e.is::<NoDaemon>() { 2 } else { 1 })
        }
    }
}
