//! Phase 1 step 2: checks that pipewire-rs covers what omadspd needs, on throwaway test nodes.
//!
//!   pwcheck info [NODE...]                          default/filters metadata, links into NODEs
//!   pwcheck set NODE KEY=VALUE...                   filter-chain control values (Props params)
//!   pwcheck volume NODE LINEAR [RAMP_MS STEP]       audioconvert volume, optionally ramped
//!   pwcheck sweep NODE TARGET LO_DB HI_DB STEP_DB PERIOD_MS SECONDS [RAMP_MS STEP]
//!       TARGET is `volume` or a control (`pre:g_in`). Triangle wave in dB, one change per period,
//!       then holds HI_DB for 1 s. A ramp applies to `volume` only.
//!   pwcheck smart NODE on|off                       filters metadata filter.smart.disabled
//!   pwcheck tap NODE SECONDS                        in-process capture stream, passive links by hand
//!
//! Prints one `RESULT key=value` line per finding.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use omadspd::pw::{self as opw, Globals, Ramp};
use pipewire as pw;
use pw::spa;
use pw::types::ObjectType;

struct App {
    mainloop: pw::main_loop::MainLoopRc,
    core: pw::core::CoreRc,
    registry: pw::registry::RegistryRc,
    globals: Rc<RefCell<Globals>>,
    _listener: pw::registry::Listener,
}

impl App {
    fn new() -> Result<App> {
        pw::init();
        let mainloop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;
        let registry = core.get_registry_rc()?;
        let globals = Rc::new(RefCell::new(Globals::default()));
        let listener = opw::mirror_registry(&registry, globals.clone(), || {});
        let app = App { mainloop, core, registry, globals, _listener: listener };
        app.roundtrip();
        Ok(app)
    }

    /// Runs the loop until the server has processed everything sent so far.
    fn roundtrip(&self) {
        let done = Rc::new(Cell::new(false));
        let pending = self.core.sync(0).expect("sync");
        let (d, ml) = (done.clone(), self.mainloop.clone());
        let _l = self
            .core
            .add_listener_local()
            .done(move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    d.set(true);
                    ml.quit();
                }
            })
            .register();
        while !done.get() {
            self.mainloop.run();
        }
    }

    /// Runs the loop for `d`.
    fn run_for(&self, d: Duration) {
        let ml = self.mainloop.clone();
        let t = self.mainloop.loop_().add_timer(move |_| ml.quit());
        t.update_timer(Some(d), None).into_result().expect("timer");
        self.mainloop.run();
    }

    fn node(&self, name: &str) -> Result<(u32, pw::node::Node)> {
        let gl = self.globals.borrow();
        let g = gl.node_by_name(name).ok_or_else(|| anyhow!("no node named {name}"))?;
        let node: pw::node::Node = self.registry.bind(&gl.raw[&g.id])?;
        Ok((g.id, node))
    }

    fn metadata(&self, name: &str) -> Result<pw::metadata::Metadata> {
        let gl = self.globals.borrow();
        let g = gl.metadata_by_name(name).ok_or_else(|| anyhow!("no metadata {name}"))?;
        Ok(self.registry.bind(&gl.raw[&g.id])?)
    }

    fn print_links_into(&self, name: &str) {
        let gl = self.globals.borrow();
        let Some(n) = gl.node_by_name(name) else {
            println!("  {name}: absent");
            return;
        };
        let mut from: Vec<String> = gl.links_into(n.id).iter().map(|(on, _, _)| gl.name_of(*on)).collect();
        from.sort();
        from.dedup();
        println!("  links into {name}: {from:?}");
    }
}

fn db_to_lin(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let app = App::new()?;
    match cmd {
        "info" => info(&app, &args[1..]),
        "set" => set(&app, &args[1..]),
        "volume" => volume(&app, &args[1..]),
        "sweep" => sweep(&app, &args[1..]),
        "smart" => smart(&app, &args[1..]),
        "tap" => tap(&app, &args[1..]),
        _ => bail!("usage: pwcheck info|set|volume|sweep|smart|tap … (see source)"),
    }
}

fn info(app: &App, nodes: &[String]) -> Result<()> {
    let mut keep = Vec::new();
    for name in ["default", "filters"] {
        let md = app.metadata(name)?;
        let l = md
            .add_listener_local()
            .property(move |subject, key, type_, value| {
                println!(
                    "RESULT metadata.{name} subject={subject} {}={} ({})",
                    key.unwrap_or("-"),
                    value.unwrap_or("-"),
                    type_.unwrap_or("-")
                );
                0
            })
            .register();
        keep.push((md, l));
    }
    app.roundtrip();
    app.roundtrip();
    for n in nodes {
        app.print_links_into(n);
    }
    Ok(())
}

fn set(app: &App, a: &[String]) -> Result<()> {
    let (_, node) = app.node(a.first().context("NODE")?)?;
    let mut pairs = Vec::new();
    for kv in &a[1..] {
        let (k, v) = kv.split_once('=').context("KEY=VALUE")?;
        pairs.push((k.to_string(), v.parse::<f32>()?));
    }
    let t = Instant::now();
    opw::set_props(&node, vec![opw::params_prop(pairs.iter().map(|(k, v)| (k.as_str(), *v)))]);
    app.roundtrip();
    println!("RESULT set n={} roundtrip_ms={:.2}", pairs.len(), t.elapsed().as_secs_f64() * 1000.0);
    Ok(())
}

fn ramp_arg(a: &[String], i: usize) -> Result<Option<Ramp>> {
    Ok(match (a.get(i), a.get(i + 1)) {
        (Some(t), Some(s)) => Some(Ramp { time_ms: t.parse()?, step_samples: s.parse()? }),
        _ => None,
    })
}

fn volume(app: &App, a: &[String]) -> Result<()> {
    let (_, node) = app.node(a.first().context("NODE")?)?;
    let v: f32 = a.get(1).context("LINEAR")?.parse()?;
    opw::set_props(&node, opw::volume_props(v, ramp_arg(a, 2)?));
    app.roundtrip();
    println!("RESULT volume={v}");
    Ok(())
}

fn sweep(app: &App, a: &[String]) -> Result<()> {
    let (_, node) = app.node(a.first().context("NODE")?)?;
    let target = a.get(1).context("TARGET")?.clone();
    let f = |i: usize| -> Result<f64> { Ok(a.get(i).context("missing number")?.parse::<f64>()?) };
    let (lo, hi, step, period_ms, secs) = (f(2)?, f(3)?, f(4)?, f(5)?, f(6)?);
    let ramp = ramp_arg(a, 7)?;
    let node = Rc::new(node);
    let send = {
        let node = node.clone();
        let target = target.clone();
        move |db: f64| {
            let lin = db_to_lin(db);
            if target == "volume" {
                opw::set_props(&node, opw::volume_props(lin, ramp));
            } else {
                opw::set_props(&node, vec![opw::params_prop([(target.as_str(), lin)])]);
            }
        }
    };
    let n = (secs * 1000.0 / period_ms) as usize;
    let (cur, dir, count) = (Rc::new(Cell::new(hi)), Rc::new(Cell::new(-1.0)), Rc::new(Cell::new(0usize)));
    let ml = app.mainloop.clone();
    let send2 = send.clone();
    let t = app.mainloop.loop_().add_timer(move |_| {
        if count.get() >= n {
            send2(hi);
            ml.quit();
            return;
        }
        let mut v = cur.get() + dir.get() * step;
        if v <= lo || v >= hi {
            v = v.clamp(lo, hi);
            dir.set(-dir.get());
        }
        cur.set(v);
        send2(v);
        count.set(count.get() + 1);
    });
    send(hi);
    let p = Duration::from_secs_f64(period_ms / 1000.0);
    t.update_timer(Some(p), Some(p)).into_result()?;
    app.mainloop.run();
    drop(t);
    app.run_for(Duration::from_secs(1));
    println!("RESULT sweep target={target} changes={n} final_db={hi}");
    Ok(())
}

fn smart(app: &App, a: &[String]) -> Result<()> {
    let name = a.first().context("NODE")?;
    let (id, _) = app.node(name)?;
    let md = app.metadata("filters")?;
    match a.get(1).map(String::as_str) {
        Some("off") => md.set_property(id, "filter.smart.disabled", Some("Spa:String:JSON"), Some("true")),
        Some("on") => md.set_property(id, "filter.smart.disabled", None, None),
        _ => bail!("on|off"),
    }
    app.roundtrip();
    println!("RESULT smart {} subject={id}", a[1]);
    Ok(())
}

struct TapState {
    peak: f32,
    frames: u64,
    buffers: u64,
}

fn tap(app: &App, a: &[String]) -> Result<()> {
    let name = a.first().context("NODE")?;
    let secs: f64 = a.get(1).context("SECONDS")?.parse()?;
    let src = app.globals.borrow().node_by_name(name).map(|g| g.id).context("source node")?;

    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "DSP",
        *pw::keys::NODE_NAME => "omadsp_pwcheck_tap",
        "node.autoconnect" => "false",
        "node.passive" => "true",
        "node.dont-reconnect" => "true",
        "state.restore-props" => "false",
        "state.restore-target" => "false",
    };
    let stream = pw::stream::StreamRc::new(app.core.clone(), "omadsp-pwcheck-tap", props)?;
    let st = Rc::new(RefCell::new(TapState { peak: 0.0, frames: 0, buffers: 0 }));
    let st2 = st.clone();
    let _sl = stream
        .add_local_listener::<()>()
        .process(move |s, _| {
            let Some(mut buf) = s.dequeue_buffer() else { return };
            let d = &mut buf.datas_mut()[0];
            let size = d.chunk().size() as usize;
            let off = d.chunk().offset() as usize;
            if let Some(bytes) = d.data() {
                let bytes = &bytes[off.min(bytes.len())..(off + size).min(bytes.len())];
                let mut t = st2.borrow_mut();
                for c in bytes.chunks_exact(4) {
                    let x = f32::from_le_bytes([c[0], c[1], c[2], c[3]]).abs();
                    if x > t.peak {
                        t.peak = x;
                    }
                }
                t.frames += (bytes.len() / 8) as u64;
                t.buffers += 1;
            }
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
    let bytes =
        spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(obj))?
            .0
            .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&bytes).context("format pod")?];
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::DONT_RECONNECT,
        &mut params,
    )?;
    let t0 = Instant::now();
    let mut tap_id = None;
    while t0.elapsed() < Duration::from_secs(2) {
        app.roundtrip();
        tap_id = app.globals.borrow().node_by_name("omadsp_pwcheck_tap").map(|g| g.id);
        if let Some(id) = tap_id
            && app.globals.borrow().ports(id, "in").len() == 2
        {
            break;
        }
        app.run_for(Duration::from_millis(20));
    }
    let tap_id = tap_id.context("tap node never appeared")?;
    let (outs, ins) = {
        let gl = app.globals.borrow();
        (gl.ports(src, "out"), gl.ports(tap_id, "in"))
    };
    println!("RESULT tap ports src={outs:?} tap={ins:?} setup_ms={}", t0.elapsed().as_millis());
    let mut links: Vec<pw::link::Link> = Vec::new();
    for ch in ["FL", "FR"] {
        let o = outs.iter().find(|(c, _)| c == ch).context("source port")?.1;
        let i = ins.iter().find(|(c, _)| c == ch).context("tap port")?.1;
        links.push(app.core.create_object("link-factory", &opw::link_props(src, o, tap_id, i, true))?);
    }
    app.roundtrip();
    {
        let gl = app.globals.borrow();
        let l: Vec<_> = gl
            .all
            .values()
            .filter(|g| g.kind == ObjectType::Link && g.get_u32("link.input.node") == Some(tap_id))
            .map(|g| {
                format!(
                    "{}:{} passive={}",
                    g.get("link.output.port").unwrap_or("?"),
                    g.get("link.input.port").unwrap_or("?"),
                    g.get("link.passive").unwrap_or("-")
                )
            })
            .collect();
        println!("RESULT tap links={l:?}");
    }
    let windows = (secs * 4.0) as usize;
    for _ in 0..windows {
        app.run_for(Duration::from_millis(250));
        let mut t = st.borrow_mut();
        let db = if t.peak > 0.0 { 20.0 * t.peak.log10() } else { f32::NEG_INFINITY };
        println!("RESULT tap peak_dbfs={db:.2} frames={} buffers={}", t.frames, t.buffers);
        t.peak = 0.0;
    }
    drop(links);
    stream.disconnect()?;
    app.roundtrip();
    Ok(())
}
