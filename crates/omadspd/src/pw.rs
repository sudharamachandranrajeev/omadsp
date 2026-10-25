//! PipeWire building blocks: Props pods, link and stream properties, and a registry mirror.
//!
//! Everything here runs on the PipeWire main loop thread; nothing is `Send`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use pipewire as pw;
use pw::spa;
use pw::spa::pod::{Object, Property, Value, serialize::PodSerializer};
use pw::types::ObjectType;

/// A linear volume ramp for audioconvert. Both fields are needed: without a step size audioconvert
/// silently drops the ramp (Spike A §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ramp {
    pub time_ms: i32,
    pub step_samples: i32,
}

/// Serializes a `Props` object (for `Node::set_param(ParamType::Props, …)`).
pub fn props_pod(properties: Vec<Property>) -> Vec<u8> {
    let obj = Value::Object(Object {
        type_: spa::utils::SpaTypes::ObjectParamProps.as_raw(),
        id: spa::param::ParamType::Props.as_raw(),
        properties,
    });
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &obj).expect("Props pod serializes").0.into_inner()
}

/// `params = [ "node:port" value … ]`: filter-chain control values.
pub fn params_prop<'a>(pairs: impl IntoIterator<Item = (&'a str, f32)>) -> Property {
    let mut v = Vec::new();
    for (k, x) in pairs {
        v.push(Value::String(k.to_string()));
        v.push(Value::Float(x));
    }
    Property::new(spa::sys::SPA_PROP_params, Value::Struct(v))
}

/// audioconvert's master `volume`, optionally ramped.
pub fn volume_props(volume: f32, ramp: Option<Ramp>) -> Vec<Property> {
    let mut p = vec![Property::new(spa::sys::SPA_PROP_volume, Value::Float(volume))];
    if let Some(r) = ramp {
        p.push(Property::new(spa::sys::SPA_PROP_volumeRampTime, Value::Int(r.time_ms)));
        p.push(Property::new(spa::sys::SPA_PROP_volumeRampStepSamples, Value::Int(r.step_samples)));
    }
    p
}

pub fn set_props(node: &pw::node::Node, properties: Vec<Property>) {
    let bytes = props_pod(properties);
    let pod = spa::pod::Pod::from_bytes(&bytes).expect("valid pod");
    node.set_param(spa::param::ParamType::Props, 0, pod);
}

/// Properties for a link made by omadsp. `passive` links don't keep the graph awake.
pub fn link_props(
    out_node: u32,
    out_port: u32,
    in_node: u32,
    in_port: u32,
    passive: bool,
) -> pw::properties::PropertiesBox {
    let mut p = pw::properties::PropertiesBox::new();
    p.insert("link.output.node", out_node.to_string());
    p.insert("link.output.port", out_port.to_string());
    p.insert("link.input.node", in_node.to_string());
    p.insert("link.input.port", in_port.to_string());
    p.insert("object.linger", "false");
    if passive {
        p.insert("link.passive", "true");
    }
    p
}

/// One global object as the registry announced it.
#[derive(Clone, Debug)]
pub struct Global {
    pub id: u32,
    pub kind: ObjectType,
    pub props: BTreeMap<String, String>,
}

impl Global {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.props.get(key).map(String::as_str)
    }
    pub fn get_u32(&self, key: &str) -> Option<u32> {
        self.get(key).and_then(|v| v.parse().ok())
    }
}

/// A mirror of the server's globals, kept current by a registry listener.
#[derive(Default)]
pub struct Globals {
    pub all: BTreeMap<u32, Global>,
    /// Owned copies, for `Registry::bind`.
    pub raw: BTreeMap<u32, pw::registry::GlobalObject<pw::properties::PropertiesBox>>,
}

impl Globals {
    pub fn node_by_name(&self, name: &str) -> Option<&Global> {
        self.all.values().find(|g| g.kind == ObjectType::Node && g.get("node.name") == Some(name))
    }
    pub fn metadata_by_name(&self, name: &str) -> Option<&Global> {
        self.all.values().find(|g| g.kind == ObjectType::Metadata && g.get("metadata.name") == Some(name))
    }
    /// Ports of `node`, as (channel, port id), for one direction (`"in"` / `"out"`).
    pub fn ports(&self, node: u32, direction: &str) -> Vec<(String, u32)> {
        self.all
            .values()
            .filter(|g| {
                g.kind == ObjectType::Port
                    && g.get_u32("node.id") == Some(node)
                    && g.get("port.direction") == Some(direction)
            })
            .filter(|g| g.get("port.monitor") != Some("true"))
            .map(|g| (g.get("audio.channel").unwrap_or("").to_string(), g.id))
            .collect()
    }
    /// Links into `node`, as (output node, output port, input port).
    pub fn links_into(&self, node: u32) -> Vec<(u32, u32, u32)> {
        self.all
            .values()
            .filter(|g| g.kind == ObjectType::Link && g.get_u32("link.input.node") == Some(node))
            .filter_map(|g| {
                Some((g.get_u32("link.output.node")?, g.get_u32("link.output.port")?, g.get_u32("link.input.port")?))
            })
            .collect()
    }
    /// Links out of `node`, as (output port, input node, input port).
    pub fn links_from(&self, node: u32) -> Vec<(u32, u32, u32)> {
        self.all
            .values()
            .filter(|g| g.kind == ObjectType::Link && g.get_u32("link.output.node") == Some(node))
            .filter_map(|g| {
                Some((g.get_u32("link.output.port")?, g.get_u32("link.input.node")?, g.get_u32("link.input.port")?))
            })
            .collect()
    }
    /// The process id of the client that created `node`.
    pub fn client_pid(&self, node: u32) -> Option<u32> {
        let client = self.all.get(&node)?.get_u32("client.id")?;
        let c = self.all.get(&client).filter(|c| c.kind == ObjectType::Client)?;
        c.get_u32("pipewire.sec.pid").or_else(|| c.get_u32("application.process.id"))
    }
    pub fn name_of(&self, id: u32) -> String {
        self.all.get(&id).and_then(|g| g.get("node.name")).unwrap_or("?").to_string()
    }
}

/// Registers a listener that mirrors the registry into `globals` and calls `changed` after each
/// update (with `globals` no longer borrowed). Keep the listener alive.
pub fn mirror_registry(
    registry: &pw::registry::Registry,
    globals: Rc<RefCell<Globals>>,
    changed: impl Fn() + 'static,
) -> pw::registry::Listener {
    let g_add = globals.clone();
    let changed = Rc::new(changed);
    let changed_add = changed.clone();
    registry
        .add_listener_local()
        .global(move |g| {
            let props =
                g.props.map(|d| d.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()).unwrap_or_default();
            {
                let mut gl = g_add.borrow_mut();
                gl.all.insert(g.id, Global { id: g.id, kind: g.type_.clone(), props });
                gl.raw.insert(g.id, g.to_owned());
            }
            changed_add();
        })
        .global_remove(move |id| {
            {
                let mut gl = globals.borrow_mut();
                gl.all.remove(&id);
                gl.raw.remove(&id);
            }
            changed();
        })
        .register()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn props_pods_parse_back() {
        use spa::pod::deserialize::PodDeserializer;
        let bytes = props_pod(vec![params_prop([("eq:gl_0", 2.0), ("lim:th", 0.89)])]);
        let (_, v) = PodDeserializer::deserialize_any_from(&bytes).unwrap();
        let Value::Object(o) = v else { panic!("not an object") };
        assert_eq!(o.type_, spa::sys::SPA_TYPE_OBJECT_Props);
        assert_eq!(o.properties[0].key, spa::sys::SPA_PROP_params);
        let Value::Struct(s) = &o.properties[0].value else { panic!("params not a struct") };
        assert_eq!(s[0], Value::String("eq:gl_0".into()));
        assert_eq!(s[3], Value::Float(0.89));

        let bytes = props_pod(volume_props(0.5, Some(Ramp { time_ms: 30, step_samples: 1 })));
        let (_, Value::Object(o)) = PodDeserializer::deserialize_any_from(&bytes).unwrap() else { panic!() };
        let keys: Vec<u32> = o.properties.iter().map(|p| p.key).collect();
        assert_eq!(
            keys,
            [spa::sys::SPA_PROP_volume, spa::sys::SPA_PROP_volumeRampTime, spa::sys::SPA_PROP_volumeRampStepSamples]
        );
    }
}
