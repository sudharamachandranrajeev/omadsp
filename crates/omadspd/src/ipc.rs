//! The NDJSON v1 server on `$XDG_RUNTIME_DIR/omadsp/omadsp.sock`, inside the PipeWire main loop.
//!
//! - Sockets are non-blocking. A client's unsent output waits in its buffer and a short timer
//!   retries; a client that falls more than [`OUT_MAX`] behind, or sends a line over [`LINE_MAX`],
//!   is dropped.
//! - A new client first gets `state.changed` and `curve.updated`, so a panel can draw at once.
//! - Every change (a command, or the engine's status) goes through [`Server::publish`], which sends
//!   `state.changed`, `device.changed` and `curve.updated` only when their payload changed.
//! - Borrow rule: the server is never borrowed while the engine runs (`Engine::with`), because the
//!   engine's status observer publishes through it.
//! - Closed clients are dropped from a timer, never from their own I/O callback.

use std::cell::RefCell;
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use pipewire::loop_::{IoSource, TimerSource};
use pipewire::spa::support::system::IoFlags;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use omadsp_core::dsp_math;
use omadsp_core::managed_file;
use omadsp_core::model::Preset;
use omadsp_core::protocol::{
    self, Command, DeviceInfo, ErrorBody, ErrorCode, Event, MeterLevel, Request, Response, Snapshot, event,
};

use crate::engine::{Core, Desired, Engine, Status};
use crate::session::Session;

/// Longest request line.
pub const LINE_MAX: usize = 64 * 1024;
/// Most unsent output per client before it is dropped.
pub const OUT_MAX: usize = 4 * 1024 * 1024;
pub const MAX_CLIENTS: usize = 64;
/// How long a state change may wait before `state.json` is written.
pub const SAVE_DELAY: Duration = Duration::from_secs(1);
const RETRY: Duration = Duration::from_millis(10);

struct Client {
    id: u64,
    _src: IoSource<'static, UnixStream>,
    /// A second handle on the socket, for writing.
    tx: UnixStream,
    inbuf: Vec<u8>,
    out: Vec<u8>,
    meter: MeterLevel,
    closed: bool,
}

impl Client {
    fn send(&mut self, line: &str) {
        if self.closed {
            return;
        }
        self.out.extend_from_slice(line.as_bytes());
        self.pump();
    }

    /// Writes as much pending output as the socket takes.
    fn pump(&mut self) {
        while !self.out.is_empty() && !self.closed {
            match self.tx.write(&self.out) {
                Ok(0) => self.closed = true,
                Ok(n) => drop(self.out.drain(..n)),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => {
                    debug!("client {}: write: {e}", self.id);
                    self.closed = true;
                }
            }
        }
        if self.out.len() > OUT_MAX {
            warn!("client {} is not reading; dropping it", self.id);
            self.closed = true;
        }
    }
}

/// What was last published, to send only changes.
#[derive(Default)]
struct Sent {
    snapshot: Option<Snapshot>,
    device: Option<Option<DeviceInfo>>,
    curve_key: Option<(Preset, u32)>,
    curve: Option<Value>,
}

pub struct Server {
    clients: Vec<Client>,
    next_id: u64,
    session: Session,
    status: Status,
    sent: Sent,
    socket: PathBuf,
    _listener: Option<IoSource<'static, UnixListener>>,
    tick: TimerSource<'static>,
    save: TimerSource<'static>,
    save_armed: bool,
}

fn arm(t: &TimerSource<'_>, after: Duration) {
    // A zero value disarms a timerfd; the shortest real wait is 1 µs.
    let _ = t.update_timer(Some(after.max(Duration::from_micros(1))), None);
}

/// Makes the runtime directory (0700) and clears a stale socket. Fails if a daemon answers on it.
fn prepare_socket(dir: &Path) -> Result<PathBuf> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(protocol::SOCKET_NAME);
    if managed_file::is_socket(&path) {
        if UnixStream::connect(&path).is_ok() {
            bail!("another omadspd is already listening on {}", path.display());
        }
        std::fs::remove_file(&path).with_context(|| format!("removing stale {}", path.display()))?;
    } else if path.symlink_metadata().is_ok() {
        bail!("{} exists and is not a socket; not touching it", path.display());
    }
    Ok(path)
}

/// Rounds dB values to 0.01 for the wire.
fn r2(v: &[f64]) -> Vec<f64> {
    v.iter().map(|x| (x * 100.0).round() / 100.0).collect()
}

/// Makes the engine follow `d`, calling only the setters whose value changed.
fn follow(c: &mut Core, d: Desired, smooth: bool) {
    let cur = c.desired().clone();
    if cur.preset != d.preset {
        c.set_preset(d.preset, smooth);
    }
    if cur.ab != d.ab {
        c.set_ab(d.ab);
    }
    if cur.enabled != d.enabled {
        c.set_enabled(d.enabled);
    }
}

impl Server {
    /// Binds the socket and wires the server into the engine's loop and status observer.
    pub fn start(engine: &Rc<Engine>, session: Session, runtime_dir: &Path) -> Result<Rc<RefCell<Server>>> {
        let socket = prepare_socket(runtime_dir)?;
        let listener = UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let ml = engine.main_loop();
        let status = engine.with(|c| c.status().clone());
        let server = Rc::new_cyclic(|w: &Weak<RefCell<Server>>| {
            let tick = {
                let w = w.clone();
                ml.loop_().add_timer(move |_| {
                    if let Some(s) = w.upgrade() {
                        s.borrow_mut().on_tick();
                    }
                })
            };
            let save = {
                let w = w.clone();
                ml.loop_().add_timer(move |_| {
                    if let Some(s) = w.upgrade() {
                        let mut s = s.borrow_mut();
                        s.save_armed = false;
                        s.session.flush();
                    }
                })
            };
            RefCell::new(Server {
                clients: Vec::new(),
                next_id: 1,
                session,
                status,
                sent: Sent::default(),
                socket: socket.clone(),
                _listener: None,
                tick,
                save,
                save_armed: false,
            })
        });
        let src = {
            let (w, eng) = (Rc::downgrade(&server), Rc::downgrade(engine));
            ml.loop_().add_io(listener, IoFlags::IN, move |l| {
                if let (Some(s), Some(e)) = (w.upgrade(), eng.upgrade()) {
                    Server::accept(&s, &e, l);
                }
            })
        };
        server.borrow_mut()._listener = Some(src);
        {
            let w = Rc::downgrade(&server);
            engine.with(|c| {
                c.set_observer(move |st| {
                    if let Some(s) = w.upgrade() {
                        let mut s = s.borrow_mut();
                        s.status = st.clone();
                        s.publish();
                    }
                })
            });
        }
        server.borrow_mut().publish();
        info!("listening on {}", socket.display());
        Ok(server)
    }

    fn accept(rc: &Rc<RefCell<Server>>, engine: &Rc<Engine>, l: &mut UnixListener) {
        loop {
            let stream = match l.accept() {
                Ok((s, _)) => s,
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    warn!("accept: {e}");
                    return;
                }
            };
            let mut s = rc.borrow_mut();
            if s.clients.iter().filter(|c| !c.closed).count() >= MAX_CLIENTS {
                warn!("too many clients; refusing one");
                continue;
            }
            let (Ok(()), Ok(tx)) = (stream.set_nonblocking(true), stream.try_clone()) else { continue };
            let id = s.next_id;
            s.next_id += 1;
            let src = {
                let (w, eng) = (Rc::downgrade(rc), Rc::downgrade(engine));
                engine.main_loop().loop_().add_io(stream, IoFlags::IN, move |st| {
                    if let (Some(s), Some(e)) = (w.upgrade(), eng.upgrade()) {
                        Server::on_readable(&s, &e, id, st);
                    }
                })
            };
            debug!("client {id} connected");
            let mut c =
                Client { id, _src: src, tx, inbuf: Vec::new(), out: Vec::new(), meter: MeterLevel::Off, closed: false };
            if let Some(snap) = &s.sent.snapshot {
                c.send(&protocol::to_line(&Event::new(
                    event::STATE_CHANGED,
                    serde_json::to_value(snap).expect("serializes"),
                )));
            }
            if let Some(curve) = &s.sent.curve {
                c.send(&protocol::to_line(&Event::new(event::CURVE_UPDATED, curve.clone())));
            }
            s.clients.push(c);
            s.schedule_tick();
        }
    }

    fn on_readable(rc: &Rc<RefCell<Server>>, engine: &Rc<Engine>, id: u64, st: &mut UnixStream) {
        let lines = {
            let mut s = rc.borrow_mut();
            let Some(c) = s.clients.iter_mut().find(|c| c.id == id && !c.closed) else { return };
            let mut lines = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                match st.read(&mut buf) {
                    Ok(0) => {
                        c.closed = true;
                        break;
                    }
                    Ok(n) => c.inbuf.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(e) => {
                        debug!("client {id}: read: {e}");
                        c.closed = true;
                        break;
                    }
                }
            }
            while let Some(pos) = c.inbuf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = c.inbuf.drain(..=pos).collect();
                lines.push(line);
            }
            if c.inbuf.len() > LINE_MAX {
                warn!("client {id}: request line too long; dropping it");
                c.closed = true;
                lines.clear();
            }
            if c.closed {
                s.schedule_tick();
            }
            lines
        };
        for line in lines {
            let text = String::from_utf8_lossy(&line);
            if text.trim().is_empty() {
                continue;
            }
            let resp = Server::request(rc, engine, id, text.trim());
            let mut s = rc.borrow_mut();
            if let Some(c) = s.clients.iter_mut().find(|c| c.id == id) {
                c.send(&protocol::to_line(&resp));
            }
            s.schedule_tick();
        }
    }

    /// Handles one request line and returns the response.
    fn request(rc: &Rc<RefCell<Server>>, engine: &Rc<Engine>, client: u64, line: &str) -> Response {
        let req: Request = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                let id =
                    serde_json::from_str::<Value>(line).ok().and_then(|v| v.get("id").cloned()).unwrap_or(Value::Null);
                return Response::err(id, ErrorBody::new(ErrorCode::BadRequest, format!("not a v1 request: {e}")));
            }
        };
        let id = req.id.clone();
        let cmd = match Command::parse(&req) {
            Ok(c) => c,
            Err(e) => return Response::err(id, e),
        };
        debug!("client {client}: {}", req.kind);
        match cmd {
            Command::StateGet => {
                let mut s = rc.borrow_mut();
                let snap = s.snapshot();
                Response::ok(id, Some(serde_json::to_value(snap).expect("serializes")))
            }
            Command::MeterSubscribe { level } => {
                let mut s = rc.borrow_mut();
                if let Some(c) = s.clients.iter_mut().find(|c| c.id == client) {
                    c.meter = level;
                }
                Response::ok(id, Some(json!({ "level": level })))
            }
            cmd => {
                let r = rc.borrow_mut().session.handle(cmd);
                match r {
                    Err(e) => Response::err(id, e),
                    Ok(out) => {
                        let desired = rc.borrow().session.desired();
                        engine.with(|c| follow(c, desired, out.smooth));
                        let status = engine.with(|c| c.status().clone());
                        let mut s = rc.borrow_mut();
                        s.status = status;
                        s.publish();
                        s.schedule_save();
                        Response::ok(id, None)
                    }
                }
            }
        }
    }

    fn snapshot(&mut self) -> Snapshot {
        let st = self.session.state();
        Snapshot {
            enabled: st.enabled,
            ab: self.session.ab(),
            engine: self.status.engine,
            error: self.status.error.clone(),
            preset: st.preset.clone(),
            dirty: st.dirty,
            working: st.working.clone(),
            presets: self.session.presets(),
            device: self.status.device.clone(),
            preamp_db: (self.status.preamp_db * 100.0).round() / 100.0,
            headroom_db: (self.status.headroom_db * 100.0).round() / 100.0,
            overload: false,
        }
    }

    fn broadcast(&mut self, name: &str, payload: Value) {
        let line = protocol::to_line(&Event::new(name, payload));
        for c in &mut self.clients {
            c.send(&line);
        }
        self.schedule_tick();
    }

    /// Sends whatever changed since the last publish.
    pub fn publish(&mut self) {
        let snap = self.snapshot();
        if self.sent.snapshot.as_ref() != Some(&snap) {
            let device = snap.device.clone();
            self.broadcast(event::STATE_CHANGED, serde_json::to_value(&snap).expect("serializes"));
            self.sent.snapshot = Some(snap);
            if self.sent.device.as_ref() != Some(&device) {
                if self.sent.device.is_some() {
                    self.broadcast(event::DEVICE_CHANGED, json!({ "device": device }));
                }
                self.sent.device = Some(device);
            }
        }
        let key = (self.session.state().working.clone(), self.status.rate);
        if self.sent.curve_key.as_ref() != Some(&key) {
            let c = dsp_math::curve(&key.0, f64::from(key.1));
            let payload = json!({
                "rate": key.1,
                "freqs": c.freqs.iter().map(|f| f.round()).collect::<Vec<_>>(),
                "left": r2(&c.left),
                "right": c.right.as_deref().map(r2),
            });
            self.broadcast(event::CURVE_UPDATED, payload.clone());
            self.sent.curve = Some(payload);
            self.sent.curve_key = Some(key);
        }
    }

    fn schedule_tick(&self) {
        if self.clients.iter().any(|c| c.closed || !c.out.is_empty()) {
            arm(&self.tick, RETRY);
        }
    }

    fn schedule_save(&mut self) {
        if self.session.unsaved() && !self.save_armed {
            self.save_armed = true;
            arm(&self.save, SAVE_DELAY);
        }
    }

    /// Retries pending output and drops closed clients.
    fn on_tick(&mut self) {
        for c in &mut self.clients {
            c.pump();
        }
        let before = self.clients.len();
        self.clients.retain(|c| !c.closed);
        if self.clients.len() != before {
            debug!("{} client(s) connected", self.clients.len());
        }
        self.schedule_tick();
    }

    /// The meter level the most demanding client asked for.
    pub fn meter_level(&self) -> MeterLevel {
        self.clients.iter().filter(|c| !c.closed).map(|c| c.meter).max_by_key(|l| *l as u8).unwrap_or_default()
    }

    /// Saves the state and removes the socket. Call after the main loop returns.
    pub fn finish(&mut self) -> io::Result<()> {
        self.session.flush();
        self.clients.clear();
        self._listener = None;
        match std::fs::remove_file(&self.socket) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}
