//! The filter host's config: a pure function of the preset and a few options.
//!
//! omadspd writes the result to `$XDG_RUNTIME_DIR/omadsp/host.conf` and runs `pipewire -c` on it
//! (ADR-2). The topology never changes at runtime (ADR-4): `<name>` (smart-filter sink) → `pre` →
//! `eq` → `lim` → `<name>_out`. Live changes go through Props, so this file is only regenerated
//! when the host (re)starts, carrying the current values as the plugins' initial controls.

use std::fmt::Write;

use crate::controls::{self, EQ, EQ_BANDS, LIM, PRE, PRE_BANDS};
use crate::model::Preset;

/// First line of every file omadsp writes outside its repo (PROMPT §2).
pub const MANAGED_HEADER: &str = "# Managed by omadsp — do not edit";

pub const LSP_EQ_X16: &str = "http://lsp-plug.in/plugins/lv2/para_equalizer_x16_lr";
pub const LSP_EQ_X32: &str = "http://lsp-plug.in/plugins/lv2/para_equalizer_x32_lr";
pub const LSP_LIMITER: &str = "http://lsp-plug.in/plugins/lv2/limiter_stereo";

/// Limiter oversampling: 22 = "True Peak/24 bit" (ADR-5). The null test decides whether it stays.
pub const LIMITER_OVS: u32 = 22;
/// Limiter lookahead (ms).
pub const LIMITER_LOOKAHEAD_MS: f32 = 5.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    /// Capture (sink) `node.name`; the playback node is `<name>_out`, which keeps Omarchy's
    /// output-sink resolution working (ADR-9).
    pub name: String,
    pub description: String,
    /// `filter.smart.target` node name. Tests only: production is targetless (ADR-1).
    pub target: Option<String>,
    /// Start with `filter.smart.disabled = true`, so WirePlumber routes nothing through the
    /// filter until the daemon has set its gains and enables it through the metadata.
    pub start_disabled: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            name: "omadsp".into(),
            description: "omadsp DSP (follows your output)".into(),
            target: None,
            start_disabled: true,
        }
    }
}

impl Options {
    pub fn playback_name(&self) -> String {
        format!("{}_out", self.name)
    }
}

/// A SPA-JSON string literal.
fn q(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            c if (c as u32) < 0x20 => {}
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn band_line(out: &mut String, c: &controls::Controls, stage: &str, i: usize, side: char) {
    out.push_str("               ");
    for p in ["ft", "fm", "f", "q", "g"] {
        let port = format!("{p}{side}_{i}");
        let v = c[&format!("{stage}:{port}")];
        let _ = write!(out, " {} = {v}", q(&port));
    }
    out.push('\n');
}

fn eq_node(out: &mut String, c: &controls::Controls, stage: &str, uri: &str, bands: usize) {
    let _ = writeln!(out, "            {{ type = lv2 name = {stage} plugin = {}", q(uri));
    out.push_str("              control = {\n");
    // IIR mode, FFT analysis off (UI only), no L/R link, LSP's own gains left at unity.
    out.push_str("                \"mode\" = 0 \"enabled\" = 1 \"clink\" = 0 \"g_in\" = 1 \"g_out\" = 1\n");
    out.push_str(
        "                \"ife_l\" = 0 \"ofe_l\" = 0 \"rfe_l\" = 0 \"ife_r\" = 0 \"ofe_r\" = 0 \"rfe_r\" = 0\n",
    );
    for side in ['l', 'r'] {
        for i in 0..bands {
            band_line(out, c, stage, i, side);
        }
    }
    out.push_str("              } }\n");
}

/// The host config for `p`.
pub fn host_conf(p: &Preset, o: &Options) -> String {
    let c = controls::controls(p);
    let mut s = String::new();
    s.push_str(MANAGED_HEADER);
    s.push_str(
        "
#
# PipeWire filter host for omadsp. omadspd rewrites this file whenever it (re)starts the host,
# and runs `pipewire -c` on it. Everything the host creates disappears when it exits.

context.properties = {
    log.level = 2
}

context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    support.*       = support/libspa-support
}

context.modules = [
    { name = libpipewire-module-rt args = { } flags = [ ifexists nofail ] }
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-filter-chain
      args = {
",
    );
    let _ = writeln!(s, "        node.description = {}", q(&o.description));
    let _ = writeln!(s, "        media.name       = {}", q(&o.description));
    s.push_str("        filter.graph = {\n          nodes = [\n");
    eq_node(&mut s, &c, PRE, LSP_EQ_X16, PRE_BANDS);
    eq_node(&mut s, &c, EQ, LSP_EQ_X32, EQ_BANDS);
    let _ = writeln!(s, "            {{ type = lv2 name = {LIM} plugin = {}", q(LSP_LIMITER));
    let _ = writeln!(
        s,
        "              control = {{ \"enabled\" = 1 \"alr\" = 0 \"boost\" = 0 \"ovs\" = {LIMITER_OVS} \"lk\" = {LIMITER_LOOKAHEAD_MS} \"th\" = {} \"rt\" = {} }} }}",
        c["lim:th"], c["lim:rt"]
    );
    s.push_str(
        "          ]
          links = [
            { output = \"pre:out_l\" input = \"eq:in_l\" }
            { output = \"pre:out_r\" input = \"eq:in_r\" }
            { output = \"eq:out_l\" input = \"lim:in_l\" }
            { output = \"eq:out_r\" input = \"lim:in_r\" }
          ]
          inputs  = [ \"pre:in_l\" \"pre:in_r\" ]
          outputs = [ \"lim:out_l\" \"lim:out_r\" ]
        }
        audio.channels = 2
        audio.position = [ FL FR ]
        capture.props = {
",
    );
    let _ = writeln!(s, "          node.name = {}", q(&o.name));
    s.push_str("          media.class = Audio/Sink\n");
    s.push_str("          filter.smart = true\n");
    s.push_str("          filter.smart.name = \"omadsp\"\n");
    if let Some(t) = &o.target {
        let _ = writeln!(s, "          filter.smart.target = {{ node.name = {} }}", q(t));
    }
    let _ = writeln!(s, "          filter.smart.disabled = {}", o.start_disabled);
    s.push_str("          state.restore-props = false\n          state.restore-target = false\n        }\n");
    s.push_str("        playback.props = {\n");
    let _ = writeln!(s, "          node.name = {}", q(&o.playback_name()));
    s.push_str("          node.passive = true\n          state.restore-props = false\n          state.restore-target = false\n");
    s.push_str("        }\n      }\n    }\n]\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_and_names() {
        let o = Options { name: "omadsp_test".into(), target: Some("x\"y".into()), ..Options::default() };
        let t = host_conf(&Preset::default(), &o);
        assert!(t.starts_with(MANAGED_HEADER));
        assert!(t.contains("node.name = \"omadsp_test\"\n"));
        assert!(t.contains("node.name = \"omadsp_test_out\"\n"));
        assert!(t.contains("filter.smart.target = { node.name = \"x\\\"y\" }"));
        assert!(t.contains("filter.smart.disabled = true"));
        // Production is targetless.
        assert!(!host_conf(&Preset::default(), &Options::default()).contains("filter.smart.target"));
    }

    #[test]
    fn every_managed_control_is_in_the_graph() {
        let p = &crate::model::builtin_presets()[2];
        let t = host_conf(p, &Options::default());
        for (k, v) in controls::controls(p) {
            let (_, port) = k.split_once(':').unwrap();
            assert!(t.contains(&format!("\"{port}\" = {v}")), "{k} = {v} missing");
        }
    }
}
