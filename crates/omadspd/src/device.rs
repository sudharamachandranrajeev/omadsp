//! Which device the filter plays into, a stable key for it, and the output-picker guard.
//!
//! - The active device is the node `<name>_out` is linked to. While the filter is switched off
//!   (or not linked yet) it is the default sink, which is where the filter would go (ADR-1).
//! - The key identifies the device across reconnects and reboots, for per-device profiles
//!   (Phase 3): the Bluetooth address, otherwise the node name (ALSA names carry the card's bus path
//!   or USB serial, and the profile). Headphone jack vs speaker on the same card share a node; telling
//!   them apart needs the device's Route params (Phase 3).
//! - Picker guard (ADR-9): the filter must never become the configured default sink. WirePlumber
//!   skips smart filters when it picks a default, but not when the user picks one.

use omadsp_core::protocol::DeviceInfo;

use crate::pw::{Global, Globals};

/// The stable key for a sink node.
pub fn device_key(node: &Global) -> String {
    if let Some(addr) = node.get("api.bluez5.address") {
        return format!("bt:{addr}");
    }
    match node.get("node.name") {
        Some(n) => format!("node:{n}"),
        None => format!("id:{}", node.id),
    }
}

pub fn device_info(node: &Global) -> DeviceInfo {
    DeviceInfo {
        node: node.get("node.name").unwrap_or("").to_string(),
        description: node.get("node.description").or(node.get("node.nick")).unwrap_or("").to_string(),
        key: device_key(node),
        rate: node.get_u32("audio.rate"),
    }
}

/// The device the filter's playback node feeds, else the default sink named `default_sink`.
pub fn active_device(globals: &Globals, playback: Option<u32>, default_sink: Option<&str>) -> Option<DeviceInfo> {
    let linked = playback.and_then(|p| globals.links_from(p).first().map(|&(_, node, _)| node));
    linked
        .and_then(|id| globals.all.get(&id))
        .or_else(|| default_sink.and_then(|n| globals.node_by_name(n)))
        .map(device_info)
}

/// The `name` in a `default.*` metadata value (`{ "name": "…" }`).
pub fn metadata_node_name(value: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(value).ok()?;
    v.get("name")?.as_str().map(str::to_string)
}

/// Whether a configured default sink names one of the filter's nodes.
pub fn names_filter(value: &str, capture: &str, playback: &str) -> bool {
    metadata_node_name(value).is_some_and(|n| n == capture || n == playback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pipewire::types::ObjectType;
    use std::collections::BTreeMap;

    fn node(id: u32, props: &[(&str, &str)]) -> Global {
        Global {
            id,
            kind: ObjectType::Node,
            props: props.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>(),
        }
    }

    fn link(id: u32, on: u32, op: u32, inn: u32, ip: u32) -> Global {
        let s = |x: u32| x.to_string();
        Global {
            id,
            kind: ObjectType::Link,
            props: [
                ("link.output.node", s(on)),
                ("link.output.port", s(op)),
                ("link.input.node", s(inn)),
                ("link.input.port", s(ip)),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        }
    }

    #[test]
    fn keys() {
        let bt = node(
            1,
            &[("node.name", "bluez_output.D0_65_F3_86_70_0C.1"), ("api.bluez5.address", "D0:65:F3:86:70:0C")],
        );
        assert_eq!(device_key(&bt), "bt:D0:65:F3:86:70:0C");
        let alsa = node(2, &[("node.name", "alsa_output.pci-0000_00_1f.3.analog-stereo")]);
        assert_eq!(device_key(&alsa), "node:alsa_output.pci-0000_00_1f.3.analog-stereo");
    }

    #[test]
    fn active_device_follows_links_then_default() {
        let mut g = Globals::default();
        for n in [
            node(10, &[("node.name", "omadsp_out")]),
            node(20, &[("node.name", "speakers"), ("node.description", "Speaker Output"), ("audio.rate", "48000")]),
            node(30, &[("node.name", "hdmi")]),
        ] {
            g.all.insert(n.id, n);
        }
        assert_eq!(active_device(&g, Some(10), Some("hdmi")).unwrap().node, "hdmi", "not linked: default");
        g.all.insert(40, link(40, 10, 11, 20, 21));
        let d = active_device(&g, Some(10), Some("hdmi")).unwrap();
        assert_eq!((d.node.as_str(), d.description.as_str(), d.rate), ("speakers", "Speaker Output", Some(48000)));
        assert!(active_device(&g, None, None).is_none());
    }

    #[test]
    fn picker_guard_matches_only_our_nodes() {
        assert!(names_filter(r#"{ "name": "omadsp" }"#, "omadsp", "omadsp_out"));
        assert!(names_filter(r#"{"name":"omadsp_out"}"#, "omadsp", "omadsp_out"));
        assert!(!names_filter(r#"{"name":"omadsp_test"}"#, "omadsp", "omadsp_out"));
        assert!(!names_filter("garbage", "omadsp", "omadsp_out"));
        assert_eq!(metadata_node_name(r#"{"name":"alsa_output.x"}"#).as_deref(), Some("alsa_output.x"));
    }
}
