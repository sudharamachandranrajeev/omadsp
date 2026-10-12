//! omadsp core: everything that can be tested without a PipeWire server.
//!
//! - [`model`]: presets, device profiles and daemon state (what is stored on disk).
//! - [`protocol`]: the NDJSON v1 IPC messages shared by the daemon, the CLI and the bar plugin.

pub mod model;
pub mod protocol;
