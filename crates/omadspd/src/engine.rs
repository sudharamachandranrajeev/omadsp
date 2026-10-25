//! The engine: one PipeWire connection, the filter host and the [`Applier`], all on the PipeWire
//! main loop (one thread, no locks).
//!
//! - PipeWire callbacks, timers, the host's pidfd and signals only queue an [`Ev`] and drain the
//!   queue into the core. A callback that fires while the core is busy (re-entrancy) just queues;
//!   the drain already running picks it up.
//! - Objects released while handling an event (proxies, listeners, sources — possibly the one whose
//!   callback is running) go to a graveyard that a timer empties on the next loop iteration.
//! - Lifecycle: connect → write `host.conf` and spawn the host → wait for its two nodes (created by
//!   the host's pid) → attach the applier. Host exit → backoff and respawn (ADR-2). A node in the
//!   error state → stop the host, `failed` (ADR-10). Server gone → drop everything, reconnect with
//!   backoff, respawn. WirePlumber restart → rebind its metadata and switch the filter on again.
//! - Picker guard (ADR-9): the filter never stays the configured default sink.
//! - SIGTERM/SIGINT: switch the filter off inside a dip, clear our metadata key, stop the host, quit.

use std::any::Any;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use anyhow::Result;
use pipewire as pw;
use pw::loop_::{IoSource, Signal, SignalSource, TimerSource};
use pw::main_loop::MainLoopRc;
use pw::spa::support::system::IoFlags;
use pw::types::ObjectType;
use tracing::{debug, info, warn};

use omadsp_core::controls::{self, Controls};
use omadsp_core::dsp_math;
use omadsp_core::graph;
use omadsp_core::model::Preset;
use omadsp_core::protocol::{DeviceInfo, Engine as EngineState};

use crate::apply::{Action, Applier, Line, Target, Timing};
use crate::device;
use crate::host::{self, Decision, HostProcess, PidFd, RestartPolicy};
use crate::pw::{self as opw, Globals};

/// The host must create its nodes within this time, or it counts as a crash.
pub const SPAWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest wait for the shutdown dip before the host is stopped anyway.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
/// SIGTERM → SIGKILL grace for the host.
pub const HOST_GRACE: Duration = Duration::from_millis(500);
const RECONNECT_MIN: Duration = Duration::from_millis(250);
const RECONNECT_MAX: Duration = Duration::from_secs(5);

const SMART_DISABLED: &str = "filter.smart.disabled";
const CONFIGURED_SINK: &str = "default.configured.audio.sink";
const DEFAULT_SINK: &str = "default.audio.sink";
const JSON: &str = "Spa:String:JSON";

#[derive(Clone, Debug)]
pub struct Options {
    pub graph: graph::Options,
    /// Where `host.conf` goes (`$XDG_RUNTIME_DIR/omadsp`).
    pub runtime_dir: PathBuf,
}

/// What the user wants; the engine makes the filter follow it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Desired {
    pub enabled: bool,
    pub ab: bool,
    pub preset: Preset,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub engine: EngineState,
    pub error: Option<String>,
    pub device: Option<DeviceInfo>,
    /// The preamp (dB), auto headroom included.
    pub preamp_db: f64,
    /// The auto-headroom part of it.
    pub headroom_db: f64,
    /// Graph rate the curve and headroom are computed for.
    pub rate: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum NodeStatus {
    Creating,
    Suspended,
    Idle,
    Running,
    Error(String),
}

impl From<pw::node::NodeState<'_>> for NodeStatus {
    fn from(s: pw::node::NodeState<'_>) -> NodeStatus {
        match s {
            pw::node::NodeState::Creating => NodeStatus::Creating,
            pw::node::NodeState::Suspended => NodeStatus::Suspended,
            pw::node::NodeState::Idle => NodeStatus::Idle,
            pw::node::NodeState::Running => NodeStatus::Running,
            pw::node::NodeState::Error(e) => NodeStatus::Error(e.to_string()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Md {
    Default,
    Filters,
    Settings,
}

impl Md {
    const ALL: [Md; 3] = [Md::Default, Md::Filters, Md::Settings];
    fn name(self) -> &'static str {
        match self {
            Md::Default => "default",
            Md::Filters => "filters",
            Md::Settings => "settings",
        }
    }
}

#[derive(Debug)]
enum Ev {
    Graph,
    Metadata { md: Md, subject: u32, key: Option<String>, kind: Option<String>, value: Option<String> },
    NodeState { node: u32, state: NodeStatus },
    CoreError { id: u32, res: i32, message: String },
    HostExited,
    ApplyTimer,
    HostTimer,
    ReconnectTimer,
    Reap,
    Terminate,
}

// Field order = drop order: listeners before their proxies, proxies before the core.

struct BoundMd {
    id: u32,
    _listener: pw::metadata::MetadataListener,
    proxy: pw::metadata::Metadata,
}

struct Conn {
    mds: [Option<BoundMd>; 3],
    _reg_listener: pw::registry::Listener,
    globals: Rc<RefCell<Globals>>,
    registry: pw::registry::RegistryRc,
    _core_listener: pw::core::Listener,
    _core: pw::core::CoreRc,
}

impl Conn {
    fn md(&self, md: Md) -> Option<&BoundMd> {
        self.mds[md as usize].as_ref()
    }
}

struct FilterNodes {
    capture_id: u32,
    playback_id: u32,
    _listeners: Vec<pw::node::NodeListener>,
    capture: pw::node::Node,
    playback: pw::node::Node,
    states: [NodeStatus; 2],
}

struct HostRun {
    _watch: IoSource<'static, PidFd>,
    proc: HostProcess,
    /// The controls `host.conf` was generated with: what the filter starts with.
    controls: Controls,
}

enum HostState {
    Down,
    /// Spawned; waiting for its nodes.
    Starting(HostRun),
    Up(HostRun, FilterNodes),
    /// Waiting to respawn after a crash.
    Backoff,
    /// Gave up; `Core::failure` says why.
    Failed,
}

struct Timers {
    apply: TimerSource<'static>,
    host: TimerSource<'static>,
    reconnect: TimerSource<'static>,
    reaper: TimerSource<'static>,
}

fn arm(t: &TimerSource<'_>, after: Option<Duration>) {
    // A zero value disarms the timer, so "now" becomes 1 µs.
    let v = after.map(|d| d.max(Duration::from_micros(1)));
    if let Err(e) = t.update_timer(v, None).into_result() {
        warn!("timer: {e}");
    }
}

/// Called after every status change.
pub type Observer = Box<dyn FnMut(&Status)>;

pub struct Core {
    // Drop order matters: everything holding proxies goes before `conn`.
    graveyard: Vec<Box<dyn Any>>,
    host: HostState,
    conn: Option<Conn>,
    timers: Timers,
    _signals: Vec<SignalSource<'static>>,
    weak: Weak<Shared>,
    ml: &'static MainLoopRc,
    context: pw::context::ContextRc,
    opts: Options,
    desired: Desired,
    policy: RestartPolicy,
    applier: Applier,
    failure: Option<String>,
    rate: u32,
    max_quantum: u32,
    force_quantum: u32,
    preamp: (f64, f64),
    default_sink: Option<String>,
    /// (type, value) of the last configured default sink that wasn't the filter.
    configured_prev: Option<(Option<String>, String)>,
    picker_reverts: u32,
    device: Option<DeviceInfo>,
    reconnect_delay: Duration,
    shutdown: Option<Instant>,
    status: Status,
    observer: Option<Observer>,
}

struct Shared {
    queue: RefCell<VecDeque<Ev>>,
    core: RefCell<Core>,
}

fn post(w: &Weak<Shared>, ev: Ev) {
    if let Some(s) = w.upgrade() {
        s.queue.borrow_mut().push_back(ev);
        drain(&s);
    }
}

fn drain(s: &Shared) {
    let Ok(mut core) = s.core.try_borrow_mut() else { return };
    loop {
        loop {
            let ev = s.queue.borrow_mut().pop_front();
            let Some(ev) = ev else { break };
            core.handle(ev);
        }
        core.settle();
        if s.queue.borrow().is_empty() {
            break;
        }
    }
}

/// The handle drivers (the IPC server, test tools) use. Everything runs on the thread that created it.
pub struct Engine {
    shared: Rc<Shared>,
    ml: &'static MainLoopRc,
}

impl Engine {
    /// Call before any other thread exists: SIGTERM/SIGINT get blocked here so that only the
    /// loop's signal sources see them, and threads inherit the mask.
    pub fn new(opts: Options, desired: Desired) -> Result<Engine> {
        host::set_term_signals(libc::SIG_BLOCK)?;
        pw::init();
        // One loop per process, alive for the process: sources can then be stored as `'static`.
        let ml: &'static MainLoopRc = Box::leak(Box::new(MainLoopRc::new(None)?));
        let context = pw::context::ContextRc::new(ml, None)?;
        let shared = Rc::new_cyclic(|w: &Weak<Shared>| {
            let timer = |ev: fn() -> Ev| {
                let w = w.clone();
                ml.loop_().add_timer(move |_| post(&w, ev()))
            };
            let timers = Timers {
                apply: timer(|| Ev::ApplyTimer),
                host: timer(|| Ev::HostTimer),
                reconnect: timer(|| Ev::ReconnectTimer),
                reaper: timer(|| Ev::Reap),
            };
            let signals = [Signal::TERM, Signal::INT]
                .into_iter()
                .map(|sig| {
                    let w = w.clone();
                    ml.loop_().add_signal_local(sig, move || post(&w, Ev::Terminate))
                })
                .collect();
            let rate = 48000;
            let mut core = Core {
                graveyard: Vec::new(),
                host: HostState::Down,
                conn: None,
                timers,
                _signals: signals,
                weak: w.clone(),
                ml,
                context,
                opts,
                desired,
                policy: RestartPolicy::default(),
                applier: Applier::new(Timing::for_clock(rate, 2048)),
                failure: None,
                rate,
                max_quantum: 2048,
                force_quantum: 0,
                preamp: (0.0, 0.0),
                default_sink: None,
                configured_prev: None,
                picker_reverts: 0,
                device: None,
                reconnect_delay: RECONNECT_MIN,
                shutdown: None,
                status: Status {
                    engine: EngineState::NoServer,
                    error: None,
                    device: None,
                    preamp_db: 0.0,
                    headroom_db: 0.0,
                    rate,
                },
                observer: None,
            };
            core.retarget(false);
            Shared { queue: RefCell::new(VecDeque::new()), core: RefCell::new(core) }
        });
        let w = Rc::downgrade(&shared);
        post(&w, Ev::ReconnectTimer);
        Ok(Engine { shared, ml })
    }

    pub fn main_loop(&self) -> &'static MainLoopRc {
        self.ml
    }

    /// Runs `f` on the core, then lets the engine act on the change. Don't call it from an observer.
    pub fn with<R>(&self, f: impl FnOnce(&mut Core) -> R) -> R {
        let r = f(&mut self.shared.core.borrow_mut());
        drain(&self.shared);
        r
    }

    /// Runs the main loop until shutdown.
    pub fn run(&self) {
        self.ml.run();
    }
}

impl Core {
    // ---- driver API ----

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn desired(&self) -> &Desired {
        &self.desired
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.desired.enabled = enabled;
        if enabled && self.failure.is_some() {
            self.retry();
        }
        self.retarget(false);
    }

    pub fn set_ab(&mut self, hold: bool) {
        self.desired.ab = hold;
        self.retarget(false);
    }

    /// `smooth`: a preset switch, applied inside a dip. Otherwise live (drags).
    pub fn set_preset(&mut self, preset: Preset, smooth: bool) {
        self.desired.preset = preset;
        self.retarget(smooth);
    }

    /// Clears a failure and starts the host again.
    pub fn retry(&mut self) {
        self.failure = None;
        self.policy.reset();
        if matches!(self.host, HostState::Failed | HostState::Down | HostState::Backoff) {
            self.host = HostState::Down;
            self.spawn_host();
        }
    }

    /// Called after every status change. It must not call back into the engine.
    pub fn set_observer(&mut self, f: impl FnMut(&Status) + 'static) {
        self.observer = Some(Box::new(f));
    }

    pub fn shutdown(&mut self) {
        self.begin_shutdown(Instant::now());
    }

    /// How often the picker guard put the previous output back.
    pub fn picker_reverts(&self) -> u32 {
        self.picker_reverts
    }

    // ---- internals ----

    fn retarget(&mut self, smooth: bool) {
        let d = &self.desired;
        self.preamp = dsp_math::preamp_db(&d.preset, f64::from(self.rate));
        let enabled = d.enabled && self.shutdown.is_none() && self.failure.is_none();
        let t = Target {
            enabled,
            ab: enabled && d.ab,
            controls: controls::controls(&d.preset),
            preamp: dsp_math::db_to_lin(self.preamp.0) as f32,
        };
        self.applier.set_target(t, smooth);
    }

    fn handle(&mut self, ev: Ev) {
        let now = Instant::now();
        match ev {
            Ev::Graph => self.on_graph(),
            Ev::Metadata { md, subject, key, kind, value } => self.on_metadata(md, subject, key, kind, value),
            Ev::NodeState { node, state } => self.on_node_state(now, node, state),
            Ev::CoreError { id, res, message } => {
                if id == pw::core::PW_ID_CORE && res == -libc::EPIPE {
                    self.lose_connection();
                } else {
                    debug!("PipeWire error on object {id}: {message} ({res})");
                }
            }
            Ev::HostExited => self.on_host_exit(now),
            Ev::HostTimer => self.on_host_timer(now),
            Ev::ReconnectTimer => self.try_connect(),
            Ev::ApplyTimer => {}
            Ev::Reap => drop(std::mem::take(&mut self.graveyard)),
            Ev::Terminate => self.begin_shutdown(now),
        }
    }

    /// After a batch of events: perform what the applier wants, re-arm timers, publish the status.
    fn settle(&mut self) {
        let now = Instant::now();
        if let HostState::Up(_, nodes) = &self.host {
            let filters = self.conn.as_ref().and_then(|c| c.md(Md::Filters));
            for a in self.applier.poll(now) {
                perform(nodes, filters, a);
            }
        }
        if let Some(deadline) = self.shutdown
            && (self.applier.is_off() || now >= deadline)
        {
            self.finish_shutdown();
        }
        let wake = [self.applier.next_wakeup(now), self.shutdown].into_iter().flatten().min();
        arm(&self.timers.apply, wake.map(|t| t.saturating_duration_since(now)));

        let st = self.compute_status();
        if st != self.status {
            info!(
                "status: {:?}{}{}",
                st.engine,
                st.device.as_ref().map(|d| format!(", device {} [{}]", d.node, d.key)).unwrap_or_default(),
                st.error.as_ref().map(|e| format!(", error: {e}")).unwrap_or_default()
            );
            self.status = st;
            if let Some(o) = &mut self.observer {
                o(&self.status);
            }
        }
        if !self.graveyard.is_empty() {
            arm(&self.timers.reaper, Some(Duration::ZERO));
        }
    }

    fn compute_status(&self) -> Status {
        let engine = if self.conn.is_none() {
            EngineState::NoServer
        } else if !self.desired.enabled || self.shutdown.is_some() {
            EngineState::Off
        } else if self.failure.is_some() {
            EngineState::Failed
        } else if self.applier.is_active() {
            EngineState::Running
        } else {
            EngineState::Starting
        };
        Status {
            engine,
            error: self.failure.clone(),
            device: self.device.clone(),
            preamp_db: self.preamp.0,
            headroom_db: self.preamp.1,
            rate: self.rate,
        }
    }

    // ---- connection ----

    fn try_connect(&mut self) {
        if self.conn.is_some() || self.shutdown.is_some() {
            return;
        }
        match self.connect() {
            Ok(c) => {
                info!("connected to PipeWire");
                self.conn = Some(c);
                self.reconnect_delay = RECONNECT_MIN;
                // A fresh server is a fresh start.
                self.failure = None;
                self.policy.reset();
                self.retarget(false);
                self.host = HostState::Down;
                self.spawn_host();
            }
            Err(e) => {
                debug!("PipeWire not reachable ({e}); retrying in {:?}", self.reconnect_delay);
                arm(&self.timers.reconnect, Some(self.reconnect_delay));
                self.reconnect_delay = (self.reconnect_delay * 2).min(RECONNECT_MAX);
            }
        }
    }

    fn connect(&self) -> Result<Conn> {
        let props = pw::properties::properties! { "application.name" => "omadspd" };
        let core = self.context.connect_rc(Some(props))?;
        let w = self.weak.clone();
        let core_listener = core
            .add_listener_local()
            .error(move |id, _seq, res, message| post(&w, Ev::CoreError { id, res, message: message.to_string() }))
            .register();
        let registry = core.get_registry_rc()?;
        let globals = Rc::new(RefCell::new(Globals::default()));
        let w = self.weak.clone();
        let reg_listener = opw::mirror_registry(&registry, globals.clone(), move || post(&w, Ev::Graph));
        Ok(Conn {
            mds: [None, None, None],
            _reg_listener: reg_listener,
            globals,
            registry,
            _core_listener: core_listener,
            _core: core,
        })
    }

    fn lose_connection(&mut self) {
        warn!("lost the connection to PipeWire");
        // The host's filter-chain module unloads itself when the server goes away, but the process
        // would stay: stop it, and start a fresh one after reconnecting.
        self.stop_host(false);
        self.host = HostState::Down;
        if let Some(c) = self.conn.take() {
            self.graveyard.push(Box::new(c));
        }
        self.device = None;
        if self.shutdown.is_none() {
            self.reconnect_delay = RECONNECT_MIN;
            arm(&self.timers.reconnect, Some(self.reconnect_delay));
        }
    }

    // ---- registry and metadata ----

    fn bind_md(&self, conn: &Conn, md: Md, id: u32) -> Result<BoundMd> {
        let proxy: pw::metadata::Metadata = {
            let gl = conn.globals.borrow();
            conn.registry.bind(&gl.raw[&id])?
        };
        let w = self.weak.clone();
        let listener = proxy
            .add_listener_local()
            .property(move |subject, key, kind, value| {
                post(
                    &w,
                    Ev::Metadata {
                        md,
                        subject,
                        key: key.map(str::to_string),
                        kind: kind.map(str::to_string),
                        value: value.map(str::to_string),
                    },
                );
                0
            })
            .register();
        Ok(BoundMd { id, _listener: listener, proxy })
    }

    fn on_graph(&mut self) {
        // Metadata objects come and go with WirePlumber.
        for md in Md::ALL {
            let Some(conn) = self.conn.as_mut() else { return };
            let current = conn.globals.borrow().metadata_by_name(md.name()).map(|g| g.id);
            if current == conn.md(md).map(|b| b.id) {
                continue;
            }
            if let Some(old) = conn.mds[md as usize].take() {
                self.graveyard.push(Box::new(old));
            }
            let Some(id) = current else { continue };
            match self.bind_md(self.conn.as_ref().expect("connected"), md, id) {
                Ok(b) => {
                    debug!("bound metadata {} ({id})", md.name());
                    self.conn.as_mut().expect("connected").mds[md as usize] = Some(b);
                    if md == Md::Filters {
                        // A new `filters` object knows nothing of our switch.
                        self.applier.smart_reset(self.opts.graph.start_disabled);
                    }
                }
                Err(e) => warn!("can't bind metadata {}: {e}", md.name()),
            }
        }

        self.discover_nodes();

        let Some(conn) = self.conn.as_ref() else { return };
        let gl = conn.globals.borrow();
        let playback = match &self.host {
            HostState::Up(_, n) => {
                self.applier.set_input_linked(!gl.links_into(n.capture_id).is_empty());
                Some(n.playback_id)
            }
            _ => None,
        };
        self.device = device::active_device(&gl, playback, self.default_sink.as_deref());
    }

    /// Starting: bind the host's nodes once both exist. Up: notice if they vanish.
    fn discover_nodes(&mut self) {
        let Some(conn) = self.conn.as_ref() else { return };
        let (name, out_name) = (self.opts.graph.name.clone(), self.opts.graph.playback_name());
        match &self.host {
            HostState::Starting(run) => {
                let pid = run.proc.pid();
                let found = {
                    let gl = conn.globals.borrow();
                    let find = |n: &str| {
                        gl.all
                            .values()
                            .find(|g| {
                                g.kind == ObjectType::Node
                                    && g.get("node.name") == Some(n)
                                    && gl.client_pid(g.id) == Some(pid)
                            })
                            .map(|g| g.id)
                    };
                    find(&name).zip(find(&out_name))
                };
                let Some((c, p)) = found else { return };
                match self.bind_nodes(c, p) {
                    Ok(nodes) => {
                        let HostState::Starting(run) = std::mem::replace(&mut self.host, HostState::Down) else {
                            unreachable!()
                        };
                        info!("filter up: {name} ({c}) → {out_name} ({p}), host pid {pid}");
                        self.applier.attach(run.controls.clone(), self.opts.graph.start_disabled);
                        self.host = HostState::Up(run, nodes);
                        arm(&self.timers.host, None);
                    }
                    Err(e) => warn!("can't bind the filter nodes: {e}"),
                }
            }
            HostState::Up(_, n) => {
                let gone = {
                    let gl = conn.globals.borrow();
                    !gl.all.contains_key(&n.capture_id) || !gl.all.contains_key(&n.playback_id)
                };
                if gone {
                    warn!("the filter nodes disappeared; restarting the host");
                    self.host_crashed(Instant::now());
                }
            }
            _ => {}
        }
    }

    fn bind_nodes(&self, capture_id: u32, playback_id: u32) -> Result<FilterNodes> {
        let conn = self.conn.as_ref().expect("connected");
        let (capture, playback): (pw::node::Node, pw::node::Node) = {
            let gl = conn.globals.borrow();
            (conn.registry.bind(&gl.raw[&capture_id])?, conn.registry.bind(&gl.raw[&playback_id])?)
        };
        let listen = |node: &pw::node::Node, id: u32| {
            let w = self.weak.clone();
            node.add_listener_local()
                .info(move |info| post(&w, Ev::NodeState { node: id, state: info.state().into() }))
                .register()
        };
        let listeners = vec![listen(&capture, capture_id), listen(&playback, playback_id)];
        Ok(FilterNodes {
            capture_id,
            playback_id,
            _listeners: listeners,
            capture,
            playback,
            states: [NodeStatus::Creating, NodeStatus::Creating],
        })
    }

    fn on_metadata(&mut self, md: Md, subject: u32, key: Option<String>, kind: Option<String>, value: Option<String>) {
        if subject != 0 {
            return;
        }
        let key = key.as_deref().unwrap_or("");
        match md {
            Md::Settings => {
                let n = value.as_deref().and_then(|v| v.parse::<u32>().ok());
                match (key, n) {
                    ("clock.rate", Some(r)) if r > 0 => self.rate = r,
                    ("clock.max-quantum", Some(q)) => self.max_quantum = q,
                    ("clock.force-quantum", Some(q)) => self.force_quantum = q,
                    _ => return,
                }
                self.applier.set_timing(Timing::for_clock(self.rate, self.max_quantum.max(self.force_quantum)));
                self.retarget(false);
            }
            Md::Default if key == DEFAULT_SINK => {
                self.default_sink = value.as_deref().and_then(device::metadata_node_name);
                self.on_graph();
            }
            Md::Default if key == CONFIGURED_SINK => self.picker_guard(kind, value),
            _ => {}
        }
    }

    fn picker_guard(&mut self, kind: Option<String>, value: Option<String>) {
        let g = &self.opts.graph;
        match value {
            Some(v) if device::names_filter(&v, &g.name, &g.playback_name()) => {
                let Some(md) = self.conn.as_ref().and_then(|c| c.md(Md::Default)) else { return };
                match &self.configured_prev {
                    Some((k, prev)) => md.proxy.set_property(0, CONFIGURED_SINK, k.as_deref(), Some(prev)),
                    None => md.proxy.set_property(0, CONFIGURED_SINK, None, None),
                }
                self.picker_reverts += 1;
                warn!(
                    "{} was picked as the output; omadsp isn't an output, it follows your device. Put back: {}",
                    g.name,
                    self.configured_prev.as_ref().map_or("(automatic)", |(_, v)| v.as_str())
                );
            }
            Some(v) => self.configured_prev = Some((kind, v)),
            None => self.configured_prev = None,
        }
    }

    fn on_node_state(&mut self, now: Instant, node: u32, state: NodeStatus) {
        let HostState::Up(_, nodes) = &mut self.host else { return };
        let i = if node == nodes.capture_id {
            0
        } else if node == nodes.playback_id {
            1
        } else {
            return;
        };
        if nodes.states[i] != state {
            debug!("node {node}: {state:?}");
        }
        nodes.states[i] = state.clone();
        if let NodeStatus::Error(e) = state {
            self.fail(format!("the filter reported an error: {e}"));
            return;
        }
        let running = nodes.states.contains(&NodeStatus::Running);
        self.applier.set_running(now, running);
    }

    // ---- host ----

    fn spawn_host(&mut self) {
        if self.conn.is_none() || self.shutdown.is_some() || !matches!(self.host, HostState::Down | HostState::Backoff)
        {
            return;
        }
        let conf = self.opts.runtime_dir.join("host.conf");
        if let Err(e) = host::write_conf(&conf, &self.desired.preset, &self.opts.graph) {
            self.fail(format!("can't write {}: {e}", conf.display()));
            return;
        }
        let proc = match HostProcess::spawn(&conf) {
            Ok(p) => p,
            Err(e) => {
                self.fail(format!("can't start the filter host (pipewire -c): {e}"));
                return;
            }
        };
        let w = self.weak.clone();
        let watch = self.ml.loop_().add_io(proc.pidfd(), IoFlags::IN, move |_| post(&w, Ev::HostExited));
        info!("filter host started (pid {})", proc.pid());
        let controls = controls::controls(&self.desired.preset);
        self.host = HostState::Starting(HostRun { _watch: watch, proc, controls });
        arm(&self.timers.host, Some(SPAWN_TIMEOUT));
    }

    /// Stops the host (if any) and forgets its nodes. `clear_switch`: remove our `filters` metadata
    /// key first (the server is still there).
    fn stop_host(&mut self, clear_switch: bool) {
        let (run, nodes) = match std::mem::replace(&mut self.host, HostState::Down) {
            HostState::Starting(run) => (Some(run), None),
            HostState::Up(run, nodes) => (Some(run), Some(nodes)),
            other => {
                self.host = other;
                (None, None)
            }
        };
        if let Some(n) = nodes {
            if clear_switch && let Some(md) = self.conn.as_ref().and_then(|c| c.md(Md::Filters)) {
                md.proxy.set_property(n.capture_id, SMART_DISABLED, None, None);
            }
            self.graveyard.push(Box::new(n));
        }
        if let Some(mut run) = run {
            let st = run.proc.terminate(HOST_GRACE);
            debug!("filter host {} stopped: {st:?}", run.proc.pid());
            // The watch goes before the pidfd it watches (field order), on the next iteration.
            self.graveyard.push(Box::new(run));
        }
        self.applier.detach();
        arm(&self.timers.host, None);
    }

    fn on_host_exit(&mut self, now: Instant) {
        let exited = match &mut self.host {
            HostState::Starting(run) | HostState::Up(run, _) => run.proc.try_exit(),
            _ => return,
        };
        let Some(status) = exited else { return };
        warn!("filter host exited: {status}");
        self.host_crashed(now);
    }

    /// The host died, hung, or lost its nodes: stop what's left, then back off or give up.
    fn host_crashed(&mut self, now: Instant) {
        let ran_for = match &self.host {
            HostState::Starting(run) | HostState::Up(run, _) => run.proc.started.elapsed(),
            _ => Duration::ZERO,
        };
        self.stop_host(true);
        if self.shutdown.is_some() {
            return;
        }
        match self.policy.on_crash(now, ran_for) {
            Decision::RestartAfter(d) => {
                info!("restarting the filter host in {d:?}");
                self.host = HostState::Backoff;
                arm(&self.timers.host, Some(d));
            }
            Decision::GiveUp => self.fail(format!(
                "the filter host crashed {} times within {:?}; audio bypasses omadsp",
                host::MAX_CRASHES,
                host::CRASH_WINDOW
            )),
        }
    }

    fn on_host_timer(&mut self, now: Instant) {
        match &self.host {
            HostState::Backoff => self.spawn_host(),
            HostState::Starting(run) if run.proc.started.elapsed() >= SPAWN_TIMEOUT => {
                warn!("the filter host didn't create its nodes within {SPAWN_TIMEOUT:?}");
                self.host_crashed(now);
            }
            _ => {}
        }
    }

    fn fail(&mut self, msg: String) {
        warn!("{msg}");
        self.stop_host(true);
        self.host = HostState::Failed;
        self.failure = Some(msg);
        self.retarget(false);
    }

    // ---- shutdown ----

    fn begin_shutdown(&mut self, now: Instant) {
        if self.shutdown.is_some() {
            info!("second signal: stopping now");
            self.finish_shutdown();
            return;
        }
        info!("shutting down");
        self.shutdown = Some(now + SHUTDOWN_TIMEOUT);
        self.retarget(false);
    }

    fn finish_shutdown(&mut self) {
        self.stop_host(true);
        self.host = HostState::Down;
        self.ml.quit();
    }
}

fn perform(nodes: &FilterNodes, filters: Option<&BoundMd>, a: Action) {
    match a {
        Action::Params(p) => {
            debug!("params: {} values", p.len());
            opw::set_props(&nodes.capture, vec![opw::params_prop(p.iter().map(|(k, v)| (k.as_str(), *v)))]);
        }
        Action::Volume { line, volume, ramp } => {
            debug!("volume {line:?} → {volume:.4} ramp {ramp:?}");
            let node = match line {
                Line::Capture => &nodes.capture,
                Line::Playback => &nodes.playback,
            };
            opw::set_props(node, opw::volume_props(volume, ramp));
        }
        Action::SmartDisabled(d) => match filters {
            Some(md) => {
                debug!("{SMART_DISABLED} = {d}");
                md.proxy.set_property(
                    nodes.capture_id,
                    SMART_DISABLED,
                    Some(JSON),
                    Some(if d { "true" } else { "false" }),
                );
            }
            None => warn!("no `filters` metadata (is WirePlumber running?): can't switch the filter"),
        },
    }
}
