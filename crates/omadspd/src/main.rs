//! omadspd: hosts the DSP chain and serves the NDJSON socket.
//!
//!   omadspd [--name NODE] [--target SINK] [--runtime-dir DIR] [--config-dir DIR] [--state-dir DIR] [-v]
//!
//! `--target` pins the filter to one sink and `--name` renames its nodes: both for tests only.
//! The directories default to `$XDG_RUNTIME_DIR/omadsp`, `~/.config/omadsp` and
//! `~/.local/state/omadsp`.

use std::path::PathBuf;
use std::rc::Rc;

use anyhow::{Context, Result, bail};
use omadsp_core::graph;
use omadspd::engine::{Engine, Options};
use omadspd::ipc::Server;
use omadspd::session::Session;
use omadspd::store::{self, Store};
use tracing::{info, warn};

const USAGE: &str =
    "usage: omadspd [--name NODE] [--target SINK] [--runtime-dir DIR] [--config-dir DIR] [--state-dir DIR] [-v]";

fn main() -> Result<()> {
    let mut graph = graph::Options::default();
    let mut runtime_dir = None;
    let (mut config_dir, mut state_dir) = (None, None);
    let mut verbose = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().with_context(|| format!("{a} needs a value\n{USAGE}"));
        match a.as_str() {
            "--name" => graph.name = val()?,
            "--target" => graph.target = Some(val()?),
            "--runtime-dir" => runtime_dir = Some(PathBuf::from(val()?)),
            "--config-dir" => config_dir = Some(PathBuf::from(val()?)),
            "--state-dir" => state_dir = Some(PathBuf::from(val()?)),
            "-v" | "--verbose" => verbose = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => bail!("unknown argument {a}\n{USAGE}"),
        }
    }
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_max_level(if verbose { tracing::Level::DEBUG } else { tracing::Level::INFO })
        .init();

    let runtime_dir = match runtime_dir {
        Some(d) => d,
        None => {
            PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?).join("omadsp")
        }
    };
    let (default_config, default_state) = store::default_dirs()?;
    let store = Store::new(config_dir.unwrap_or(default_config), state_dir.unwrap_or(default_state));
    let session = Session::load(store);
    let st = session.state();
    info!(
        "starting: DSP {}, preset {:?}{}",
        if st.enabled { "on" } else { "off" },
        st.preset,
        if st.dirty { " (edited)" } else { "" }
    );

    let engine = Rc::new(Engine::new(Options { graph, runtime_dir: runtime_dir.clone() }, session.desired())?);
    let server = Server::start(&engine, session, &runtime_dir)?;
    engine.run();
    if let Err(e) = server.borrow_mut().finish() {
        warn!("cleanup: {e}");
    }
    info!("stopped");
    Ok(())
}
