//! Phase 1 step 6: drives the engine from stdin, one command per line, until `quit` or EOF.
//!
//!   enginecheck [--name NODE] [--target SINK] [--runtime-dir DIR] [--on] [-v]
//!
//! Commands: on | off | ab 1|0 | band I DB | preamp DB | auto 1|0 | tone BASS_DB TREBLE_DB |
//! limiter CEILING_DBTP | preset NAME | retry | status | quit
//!
//! Prints `STATUS …` on every status change and `OK …` / `ERR …` per command.

use std::cell::RefCell;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use omadsp_core::graph;
use omadsp_core::model::builtin_presets;
use omadspd::engine::{Core, Desired, Engine, Options, Status};
use pipewire::loop_::IoSource;
use pipewire::spa::support::system::IoFlags;

struct StdinFd;
impl AsRawFd for StdinFd {
    fn as_raw_fd(&self) -> RawFd {
        0
    }
}

fn print_status(s: &Status) {
    let dev = s.device.as_ref().map_or("-".to_string(), |d| format!("{}[{}]", d.node, d.key));
    println!(
        "STATUS engine={:?} device={dev} preamp_db={:.2} headroom_db={:.2} rate={} error={}",
        s.engine,
        s.preamp_db,
        s.headroom_db,
        s.rate,
        s.error.as_deref().unwrap_or("-")
    );
}

fn command(c: &mut Core, line: &str) -> Result<String> {
    let w: Vec<&str> = line.split_whitespace().collect();
    let num = |i: usize| -> Result<f64> { Ok(w.get(i).context("missing number")?.parse::<f64>()?) };
    let flag = |i: usize| -> Result<bool> { Ok(num(i)? != 0.0) };
    let mut p = c.desired().preset.clone();
    match w.first().copied().unwrap_or("") {
        "on" => c.set_enabled(true),
        "off" => c.set_enabled(false),
        "ab" => c.set_ab(flag(1)?),
        "retry" => c.retry(),
        "status" => print_status(c.status()),
        "quit" => c.shutdown(),
        "band" => {
            let i = num(1)? as usize;
            *p.eq.gains.get_mut(i).context("no such band")? = num(2)?;
            p.sanitize();
            c.set_preset(p, false);
        }
        "preamp" => {
            p.preamp.db = num(1)?;
            p.sanitize();
            c.set_preset(p, false);
        }
        "auto" => {
            p.preamp.auto = flag(1)?;
            c.set_preset(p, false);
        }
        "tone" => {
            p.tone.bass.db = num(1)?;
            p.tone.treble.db = num(2)?;
            p.sanitize();
            c.set_preset(p, false);
        }
        "limiter" => {
            p.limiter.ceiling_dbtp = num(1)?;
            p.sanitize();
            c.set_preset(p, false);
        }
        "preset" => {
            let name = w[1..].join(" ");
            let p = builtin_presets()
                .into_iter()
                .find(|b| b.name.eq_ignore_ascii_case(&name))
                .with_context(|| format!("no built-in preset {name}"))?;
            c.set_preset(p, true);
        }
        "" => return Ok(String::new()),
        other => bail!("unknown command {other}"),
    }
    Ok(line.to_string())
}

fn main() -> Result<()> {
    let mut graph = graph::Options::default();
    let mut runtime_dir = PathBuf::from(std::env::var("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR")?).join("omadsp");
    let mut desired = Desired::default();
    let mut verbose = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--name" => graph.name = args.next().context("--name NODE")?,
            "--target" => graph.target = Some(args.next().context("--target SINK")?),
            "--runtime-dir" => runtime_dir = args.next().context("--runtime-dir DIR")?.into(),
            "--on" => desired.enabled = true,
            "-v" => verbose = true,
            _ => bail!("usage: enginecheck [--name NODE] [--target SINK] [--runtime-dir DIR] [--on] [-v]"),
        }
    }
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(if verbose { tracing::Level::DEBUG } else { tracing::Level::INFO })
        .init();

    let engine = Rc::new(Engine::new(Options { graph, runtime_dir }, desired)?);
    engine.with(|c| c.set_observer(print_status));
    let ml = engine.main_loop();

    // Stdin. The source is removed by a timer after EOF (not from its own callback), so a
    // readable-forever EOF can't spin the loop while the engine shuts down.
    type Slot = Rc<RefCell<Option<IoSource<'static, StdinFd>>>>;
    let slot: Slot = Rc::new(RefCell::new(None));
    let remover = {
        let slot = slot.clone();
        Rc::new(ml.loop_().add_timer(move |_| drop(slot.borrow_mut().take())))
    };
    let buf = Rc::new(RefCell::new(Vec::<u8>::new()));
    let src = {
        let (engine, remover) = (engine.clone(), remover.clone());
        ml.loop_().add_io(StdinFd, IoFlags::IN, move |_| {
            let mut chunk = [0u8; 4096];
            // SAFETY: reading into a local buffer from fd 0.
            let n = unsafe { libc::read(0, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n <= 0 {
                let _ = remover.update_timer(Some(Duration::from_micros(1)), None);
                engine.with(|c| c.shutdown());
                return;
            }
            let mut b = buf.borrow_mut();
            b.extend_from_slice(&chunk[..n as usize]);
            while let Some(pos) = b.iter().position(|&c| c == b'\n') {
                let line: Vec<u8> = b.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line).trim().to_string();
                match engine.with(|c| command(c, &line)) {
                    Ok(s) if s.is_empty() => {}
                    Ok(s) => println!("OK {s}"),
                    Err(e) => println!("ERR {line}: {e:#}"),
                }
            }
        })
    };
    *slot.borrow_mut() = Some(src);
    engine.run();
    Ok(())
}
