//! Bounded material dependency stamps. No part geometry or catalog payloads persist.
use super::{
    material_service::fields,
    world::{Node, World, WorldRef},
};
use serde_json::{json, Value as Json};
use std::{
    collections::{BTreeMap, HashMap},
    rc::Rc,
};
const LIMIT: usize = 4096;
const NAME_BYTES: usize = 256 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Base(&'static str),
    Variant(Option<i64>, String),
}
impl Key {
    fn bytes(&self) -> usize {
        match self {
            Self::Base(_) => 0,
            Self::Variant(_, n) => n.len(),
        }
    }
}
#[derive(Clone, Copy, Default)]
struct Stamp {
    sequence: u64,
    local: bool,
}
impl Stamp {
    fn current(self, sequence: u64) -> bool {
        !self.local && self.sequence <= sequence
    }
}
#[derive(Clone, Default)]
struct History {
    entries: BTreeMap<Key, Stamp>,
    bytes: usize,
    overflow: Stamp,
}
#[derive(Clone, Default)]
pub(crate) struct MaterialHistory(Rc<History>);
#[derive(Clone, PartialEq, Eq)]
struct Signature {
    digest: [u8; 32],
    local: bool,
}
struct Entry {
    signature: Signature,
    sequence: u64,
}
pub(crate) struct Snapshot(Option<BTreeMap<Key, Entry>>);
#[derive(Default)]
struct Cause {
    all: u64,
    fields: HashMap<String, u64>,
}
#[derive(Default)]
pub(crate) struct Causes(HashMap<String, Cause>, u64);
impl Causes {
    pub(crate) fn received(world: &World, delta: &Json) -> Option<Self> {
        let services_changed = delta["services"].get("MaterialService").is_some();
        // Normal pose/property traffic needs no catalog capture or cause map.
        let relevant = services_changed
            || delta["actions"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|row| {
                    if row["sequence"].as_u64().unwrap_or(0) <= world.sequence {
                        return false;
                    }
                    if matches!(row["kind"].as_str(), Some("create" | "remove" | "destroy")) {
                        return true;
                    }
                    if !super::property_events::relevant(row) {
                        return false;
                    }
                    let Some(id) = row["id"].as_str() else {
                        return false;
                    };
                    row["fields"]
                        .as_object()
                        .into_iter()
                        .flat_map(|f| f.keys().map(String::as_str))
                        .chain(
                            row.get("property")
                                .or_else(|| row.get("key"))
                                .and_then(Json::as_str),
                        )
                        .any(|key| affects_property(world, id, key))
                });
        if !relevant {
            return None;
        }
        let mut result = Self::default();
        if services_changed {
            result.1 = delta["sequence"].as_u64().unwrap_or(0);
        }
        for row in delta["actions"].as_array().into_iter().flatten() {
            let seq = row["sequence"].as_u64().unwrap_or(0);
            if seq <= world.sequence {
                continue;
            }
            let kind = row["kind"].as_str().unwrap_or("");
            if matches!(kind, "create" | "remove" | "destroy") {
                let ids: Vec<_> = row["id"]
                    .as_str()
                    .into_iter()
                    .chain(row["node"]["id"].as_str())
                    .chain(
                        row["ids"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Json::as_str),
                    )
                    .collect();
                for id in ids {
                    result.0.entry(id.into()).or_default().all = seq;
                }
            } else if super::property_events::relevant(row) {
                if let Some(id) = row["id"].as_str() {
                    for (key, _) in super::property_events::fields(row) {
                        if !matches!(
                            key,
                            "Name" | "Parent" | "BaseMaterial" | "CustomPhysicalProperties"
                        ) && !fields::OVERRIDE_FIELDS.contains(&key)
                        {
                            continue;
                        }
                        result
                            .0
                            .entry(id.into())
                            .or_default()
                            .fields
                            .insert(key.into(), seq);
                    }
                }
            }
        }
        Some(result)
    }
    fn revision(&self, id: &str, keys: &[&str]) -> u64 {
        self.0.get(id).map_or(self.1, |c| {
            keys.iter()
                .filter_map(|k| c.fields.get(*k))
                .copied()
                .fold(c.all.max(self.1), u64::max)
        })
    }
}
pub(crate) fn affects_property(world: &World, id: &str, key: &str) -> bool {
    key == "Parent"
        || match world.nodes.class_name(id) {
            Some("MaterialVariant") => {
                matches!(key, "Name" | "BaseMaterial" | "CustomPhysicalProperties")
            }
            Some("MaterialService") => fields::OVERRIDE_FIELDS.contains(&key),
            _ => false,
        }
}
fn property(node: &Node, key: &str) -> Option<Json> {
    node.local_overrides
        .get(key)
        .cloned()
        .or_else(|| {
            if let Some(p) = &node.projected {
                p.property(key).ok().flatten()
            } else {
                node.properties.get(key).cloned()
            }
        })
        .or_else(|| {
            (!node.server_owned)
                .then(|| {
                    super::world::constructor_default(&node.class_name, key)
                        .ok()
                        .map(|x| x.0)
                })
                .flatten()
        })
}
fn signature(value: Json, local: bool) -> Signature {
    Signature {
        digest: *blake3::hash(value.to_string().as_bytes()).as_bytes(),
        local,
    }
}
fn changed(node: &Node, keys: &[&str]) -> bool {
    !node.server_owned || keys.iter().any(|k| node.local_overrides.contains_key(*k))
}
/// Reads only service/variant properties. Ancestor checks use headers and peek
/// at already materialized local overlays; unrelated part data stays packed.
pub(crate) fn capture(world: &World, causes: Option<&Causes>) -> Snapshot {
    let empty = Causes::default();
    let causes = causes.unwrap_or(&empty);
    let service = world
        .services
        .get("MaterialService")
        .filter(|id| world.nodes.is_live(id))
        .and_then(|id| world.nodes.get(id))
        .or_else(|| world.nodes.first_live_of_class("MaterialService"));
    let Some(service) = service else {
        return Snapshot(Some(BTreeMap::new()));
    };
    if service.class_name != "MaterialService" {
        return Snapshot(None);
    }
    let mut entries = BTreeMap::new();
    let mut variants = Vec::new();
    let mut count = 0;
    let mut bytes = 0;
    for id in world.header_order().filter(|id| {
        world.nodes.class_name(id) == Some("MaterialVariant") && world.nodes.is_live(id)
    }) {
        count += 1;
        if count > LIMIT {
            return Snapshot(None);
        }
        let mut cursor = world.nodes.live_parent(id);
        let mut local = false;
        let mut seq = 0;
        let mut found = false;
        for _ in 0..1024 {
            let Some(parent) = cursor else {
                break;
            };
            if parent == service.id {
                found = true;
                break;
            }
            seq = seq.max(causes.revision(&parent, &["Parent"]));
            local |= world
                .nodes
                .peek(&parent)
                .is_some_and(|n| n.local_overrides.contains_key("Parent"));
            cursor = world.nodes.live_parent(&parent);
        }
        if !found {
            continue;
        }
        let node = world.nodes.get(id).unwrap();
        bytes += node.name.len();
        if bytes > NAME_BYTES {
            return Snapshot(None);
        }
        let base = property(node, "BaseMaterial");
        let material = base
            .as_ref()
            .and_then(|b| b.get("$enum"))
            .filter(|e| e["type"] == "Material")
            .and_then(|e| e["value"].as_i64());
        local |= changed(
            node,
            &["Name", "Parent", "BaseMaterial", "CustomPhysicalProperties"],
        );
        seq = seq
            .max(causes.revision(
                id,
                &["Name", "Parent", "BaseMaterial", "CustomPhysicalProperties"],
            ))
            .max(causes.revision(&service.id, &[]));
        let value = property(node, "CustomPhysicalProperties");
        let sig = signature(json!([node.id, base, value]), local);
        let key = Key::Variant(material, node.name.clone());
        entries.entry(key).or_insert_with(|| Entry {
            signature: sig.clone(),
            sequence: seq,
        });
        variants.push((material, node.name.as_str(), sig, seq));
    }
    for &field in fields::OVERRIDE_FIELDS {
        let material = fields::override_material(field).unwrap();
        let name = property(service, field);
        let selected = name
            .as_ref()
            .and_then(Json::as_str)
            .filter(|n| !n.is_empty())
            .and_then(|name| {
                variants
                    .iter()
                    .find(|(m, n, _, _)| *n == name && (m.is_none() || *m == Some(material)))
            });
        let local = changed(service, &[field]) || selected.is_some_and(|v| v.2.local);
        let sequence = causes
            .revision(&service.id, &[field])
            .max(selected.map_or(0, |v| v.3));
        entries.insert(
            Key::Base(field),
            Entry {
                signature: signature(
                    json!([service.id, name, selected.map(|v| v.2.digest)]),
                    local,
                ),
                sequence,
            },
        );
    }
    Snapshot(Some(entries))
}
impl MaterialHistory {
    pub(crate) fn shared_with(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
    pub(crate) fn record(
        &mut self,
        before: Snapshot,
        after: Snapshot,
        received: bool,
        sequence: u64,
    ) {
        let (Some(before), Some(after)) = (before.0, after.0) else {
            let h = Rc::make_mut(&mut self.0);
            h.overflow = Stamp {
                sequence: h.overflow.sequence.max(sequence),
                local: h.overflow.local || !received,
            };
            return;
        };
        for key in before
            .keys()
            .chain(after.keys().filter(|key| !before.contains_key(*key)))
        {
            let old = before.get(key);
            let new = after.get(key);
            if old.map(|e| &e.signature) == new.map(|e| &e.signature) {
                continue;
            }
            let h = Rc::make_mut(&mut self.0);
            if !h.entries.contains_key(key)
                && (h.entries.len() >= LIMIT || h.bytes + key.bytes() > NAME_BYTES)
            {
                h.overflow = Stamp {
                    sequence: h.overflow.sequence.max(sequence),
                    local: h.overflow.local || !received,
                };
                continue;
            }
            if !h.entries.contains_key(key) {
                h.bytes += key.bytes();
            }
            let seq = old
                .map_or(0, |e| e.sequence)
                .max(new.map_or(0, |e| e.sequence));
            let stamp = h.entries.entry(key.clone()).or_default();
            stamp.sequence = stamp.sequence.max(if received { seq } else { 0 });
            stamp.local = !received || new.is_some_and(|e| e.signature.local);
        }
    }
    fn current(&self, key: &Key, sequence: u64) -> bool {
        self.0.entries.get(key).is_none_or(|s| s.current(sequence))
    }
}
pub(crate) fn report_current(
    ctx: &WorldRef,
    material: Option<i64>,
    variant: Option<&str>,
    sequence: u64,
) -> bool {
    let history = ctx.borrow().material_history.clone();
    if !history.0.overflow.current(sequence) {
        return false;
    }
    if history.0.entries.is_empty() {
        return true;
    }
    let (Some(material), Some(variant)) = (material, variant) else {
        return false;
    };
    if !variant.is_empty() {
        if !history.current(&Key::Variant(Some(material), variant.into()), sequence)
            || !history.current(&Key::Variant(None, variant.into()), sequence)
        {
            return false;
        }
        // A named variant with explicit data does not depend on a base override.
        if super::material_service::named_custom(ctx, material, variant).unwrap_or(false) {
            return true;
        }
    }
    fields::override_field(material)
        .is_none_or(|field| history.current(&Key::Base(field), sequence))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(name: &str, sequence: u64) -> Snapshot {
        Snapshot(Some(BTreeMap::from([(
            Key::Variant(Some(512), name.into()),
            Entry {
                signature: signature(json!(sequence), false),
                sequence,
            },
        )])))
    }
    #[test]
    fn history_is_copy_on_write_and_keeps_removed_dependencies() {
        let mut root = MaterialHistory::default();
        root.record(snapshot("Dense", 0), snapshot("Dense", 1), true, 1);
        let mut actor = root.clone();
        assert!(actor.shared_with(&root));
        actor.record(
            snapshot("Dense", 1),
            Snapshot(Some(BTreeMap::new())),
            false,
            1,
        );
        assert!(!actor.shared_with(&root));
        let key = Key::Variant(Some(512), "Dense".into());
        assert!(root.current(&key, 1));
        assert!(!actor.current(&key, 100));
        actor.record(
            Snapshot(Some(BTreeMap::new())),
            snapshot("Dense", 2),
            true,
            2,
        );
        assert!(actor.current(&key, 2));
        assert!(!actor.current(&key, 1));
    }
    #[test]
    fn repeated_names_and_large_names_cannot_grow_history_without_bound() {
        let mut history = MaterialHistory::default();
        for i in 0..LIMIT + 10 {
            history.record(
                Snapshot(Some(BTreeMap::new())),
                snapshot(&format!("v{i}"), i as u64 + 1),
                true,
                i as u64 + 1,
            );
        }
        assert_eq!(history.0.entries.len(), LIMIT);
        assert!(!history.0.overflow.current(0));
        assert!(history.0.overflow.current((LIMIT + 10) as u64));
        let mut history = MaterialHistory::default();
        for i in 0..8 {
            history.record(
                Snapshot(Some(BTreeMap::new())),
                snapshot(&format!("{i}{}", "x".repeat(65535)), 0),
                false,
                0,
            );
        }
        assert!(history.0.bytes <= NAME_BYTES);
        assert!(history.0.entries.len() <= 4);
        assert!(!history.0.overflow.current(u64::MAX));
    }
    #[test]
    fn captures_leave_unrelated_properties_and_ancestors_unmaterialized() {
        use crate::{
            engine_world_projection::ProjectionContext, engine_world_view::SharedWorldView,
        };
        use rove_client_core::{schema::Schema, world::WorldStore};
        let mut nodes = vec![
            json!({"id":"s","class_name":"MaterialService","properties":{"WoodName":"Dense"}}),
            json!({"id":"f","class_name":"Folder","parent":"s"}),
            json!({"id":"v","class_name":"MaterialVariant","name":"Dense","parent":"f","properties":{"BaseMaterial":{"$enum":{"type":"Material","value":512}},"CustomPhysicalProperties":{"$physical_properties":[2,0.5,0.2,1,1,0]}}}),
        ];
        for i in 0..1000 {
            nodes.push(json!({"id":format!("p{i}"),"class_name":"Part"}));
        }
        let snapshot = json!({"version":1,"sequence":0,"nodes":nodes});
        let store = WorldStore::from_snapshot(&snapshot).unwrap();
        let schema = Schema {
            version: 3,
            flags: 0,
            enums: vec![],
            classes: vec![],
            properties: vec![],
            events: vec![],
            strings: vec![],
            names: vec![],
            lr_types: vec![],
            parent_property_id: 0,
            decoded_bytes: 0,
        };
        let view = SharedWorldView::projected(
            store.view(),
            ProjectionContext::new(&schema, HashMap::new()).unwrap(),
        );
        let world =
            World::from_snapshot(&json!({"version":1,"sequence":0,"nodes":[]}), Some(&view))
                .unwrap();
        let captured = capture(&world, None);
        assert_eq!(captured.0.unwrap().len(), 41);
        assert!(world.nodes.peek("f").is_none());
        for i in 0..1000 {
            assert!(world.nodes.peek(&format!("p{i}")).is_none());
        }
    }
}
