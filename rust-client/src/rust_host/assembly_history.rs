//! Received topology freshness for assembly reports. Part data stays in the world.
use super::{
    joint_graph::{self, Graph, Scope},
    world::{self, World},
};
use serde_json::Value as Json;
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    rc::Rc,
    sync::Arc,
};
const MAX_EDGES: usize = 4096;
const MAX_PARTS: usize = 8192;
const ID_BYTES: usize = 512 * 1024;
#[derive(Default, Clone)]
struct History {
    parts: BTreeMap<Arc<str>, u64>,
    removed: HashSet<Arc<str>>,
    bytes: usize,
    overflow: u64,
}
#[derive(Default, Clone)]
pub(crate) struct AssemblyHistory(Rc<History>);
#[derive(Default)]
struct Cause {
    all: u64,
    fields: HashMap<String, u64>,
}
pub(crate) struct Causes(HashMap<String, Cause>);
impl Causes {
    pub(crate) fn received(w: &World, delta: &Json) -> Option<Self> {
        let rows = delta["actions"].as_array()?;
        if rows.len() > 10_000 {
            return None;
        }
        let relevant = |row: &Json| {
            if row["sequence"].as_u64().unwrap_or(0) <= w.sequence {
                return false;
            }
            if matches!(row["kind"].as_str(), Some("create" | "remove" | "destroy")) {
                return true;
            }
            let Some(id) = row["id"].as_str() else {
                return false;
            };
            if !super::property_events::relevant(row) {
                return false;
            }
            row["fields"]
                .as_object()
                .into_iter()
                .flat_map(|f| f.keys().map(String::as_str))
                .chain(
                    row.get("property")
                        .or_else(|| row.get("key"))
                        .and_then(Json::as_str),
                )
                .any(|key| {
                    matches!(key, "Parent" | "Anchored")
                        || (matches!(key, "Part0" | "Part1" | "Enabled")
                            && w.nodes.class_name(id).is_some_and(joint_graph::rigid_class))
                })
        };
        if !rows.iter().any(relevant) {
            return None;
        }
        if !super::session::json_fits(delta, 8 * 1024 * 1024) {
            return None;
        }
        let mut causes = HashMap::<String, Cause>::new();
        for row in rows {
            let seq = row["sequence"].as_u64().unwrap_or(0);
            if seq <= w.sequence {
                continue;
            }
            if matches!(row["kind"].as_str(), Some("create" | "remove" | "destroy")) {
                for id in row["id"]
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
                {
                    let cause = causes.entry(id.into()).or_default();
                    cause.all = cause.all.max(seq);
                }
            } else if super::property_events::relevant(row) {
                if let Some(id) = row["id"].as_str() {
                    for (key, _) in super::property_events::fields(row) {
                        if matches!(key, "Part0" | "Part1" | "Enabled" | "Anchored" | "Parent") {
                            let stamp = causes
                                .entry(id.into())
                                .or_default()
                                .fields
                                .entry(key.into())
                                .or_default();
                            *stamp = (*stamp).max(seq);
                        }
                    }
                }
            }
        }
        Some(Self(causes))
    }
    fn revision(&self, id: &str, keys: &[&str]) -> u64 {
        self.0.get(id).map_or(0, |c| {
            keys.iter()
                .filter_map(|k| c.fields.get(*k))
                .copied()
                .fold(c.all, u64::max)
        })
    }
}
fn shared(w: &World, id: &str) -> Arc<str> {
    w.nodes.shared_id(id).unwrap_or_else(|| Arc::from(id))
}
fn ancestry(w: &World, id: &str, causes: &Causes, keys: &[&str]) -> (u64, bool) {
    let mut sequence = causes.revision(id, keys);
    let mut local = w
        .nodes
        .peek(id)
        .is_some_and(|n| keys.iter().any(|k| n.local_overrides.contains_key(*k)));
    let mut cursor = Some(id.to_owned());
    let mut seen = HashSet::new();
    for _ in 0..1024 {
        let Some(id) = cursor else {
            break;
        };
        if !seen.insert(id.clone()) {
            break;
        }
        sequence = sequence.max(causes.revision(&id, &["Parent"]));
        local |= w
            .nodes
            .peek(&id)
            .is_some_and(|n| n.local_overrides.contains_key("Parent"));
        cursor = w.nodes.live_parent(&id);
    }
    (sequence, local)
}
#[derive(PartialEq, Eq)]
struct EdgeState {
    ends: [Option<Arc<str>>; 2],
    original: [Option<Arc<str>>; 2],
    active: bool,
    error: bool,
    local: bool,
    retired: bool,
}
struct Edge {
    state: EdgeState,
    sequence: u64,
}
struct Part {
    signature: [u8; 32],
    sequence: u64,
}
struct View {
    edges: BTreeMap<Arc<str>, Edge>,
    parts: BTreeMap<Arc<str>, Part>,
    groups: Vec<Vec<Arc<str>>>,
    membership: BTreeMap<Arc<str>, usize>,
}
pub(crate) struct Snapshot(Option<View>);
fn digest(ids: &[Arc<str>]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    for id in ids {
        h.update(&(id.len() as u64).to_le_bytes());
        h.update(id.as_bytes());
    }
    *h.finalize().as_bytes()
}
pub(crate) fn capture(w: &World, causes: &Causes) -> Snapshot {
    let ids = w
        .header_order()
        .filter(|id| {
            w.nodes.class_name(id).is_some_and(joint_graph::rigid_class)
                && (w.nodes.is_live(id) || !w.assembly_history.received_removed(id))
        })
        .take(MAX_EDGES + 1)
        .collect::<Vec<_>>();
    if ids.len() > MAX_EDGES {
        return Snapshot(None);
    }
    let mut identity_bytes = 0usize;
    for id in &ids {
        identity_bytes = identity_bytes.saturating_add(id.len());
        for key in ["Part0", "Part1"] {
            if let Ok(v) = world::get_property_from_world(w, id, key) {
                identity_bytes =
                    identity_bytes.saturating_add(v["$ref"].as_str().map_or(0, str::len));
            }
        }
        if identity_bytes > ID_BYTES {
            return Snapshot(None);
        }
    }
    let Ok(graph) = Graph::build_in_world(w, Scope::Rigid) else {
        return Snapshot(None);
    };
    let mut edges = BTreeMap::new();
    let mut adjacency = BTreeMap::<Arc<str>, Vec<Arc<str>>>::new();
    let mut graph_edges = graph
        .edges
        .into_iter()
        .map(|e| (e.id.clone(), e))
        .collect::<HashMap<_, _>>();
    for id in ids {
        let node = w.nodes.get(id).unwrap();
        let original = std::array::from_fn(|i| {
            let key = if i == 0 { "Part0" } else { "Part1" };
            node.projected
                .as_ref()
                .and_then(|p| p.property(key).ok().flatten())
                .or_else(|| node.properties.get(key).cloned())
                .and_then(|v| v["$ref"].as_str().map(|id| shared(w, id)))
        });
        let retired = node.destroyed
            && node.local_overrides.contains_key("Parent")
            && !w.assembly_history.received_removed(id);
        let (ends, active, error) = if let Some(e) = graph_edges.remove(id) {
            (
                [
                    e.a.as_deref().map(|id| shared(w, id)),
                    e.b.as_deref().map(|id| shared(w, id)),
                ],
                e.active,
                e.error.is_some(),
            )
        } else if retired {
            (original.clone(), false, false)
        } else {
            continue;
        };
        let (mut sequence, mut local) = ancestry(w, id, causes, &["Part0", "Part1", "Enabled"]);
        for part in ends.iter().chain(original.iter()).flatten() {
            adjacency.entry(part.clone()).or_default();
            let (seq, l) = ancestry(w, part, causes, &["Anchored"]);
            sequence = sequence.max(seq);
            local |= l;
        }
        if active {
            if let [Some(a), Some(b)] = &ends {
                adjacency.entry(a.clone()).or_default().push(b.clone());
                adjacency.entry(b.clone()).or_default().push(a.clone());
            }
        }
        edges.insert(
            shared(w, id),
            Edge {
                state: EdgeState {
                    ends,
                    original,
                    active,
                    error,
                    local,
                    retired,
                },
                sequence,
            },
        );
    }
    if adjacency.len() > MAX_PARTS || adjacency.keys().map(|id| id.len()).sum::<usize>() > ID_BYTES
    {
        return Snapshot(None);
    }
    let mut parts = BTreeMap::new();
    let mut membership = BTreeMap::new();
    let mut groups = Vec::new();
    for id in adjacency.keys() {
        let (sequence, local) = ancestry(w, id, causes, &["Anchored"]);
        let anchored = world::get_property_from_world(w, id, "Anchored").ok();
        parts.insert(
            id.clone(),
            Part {
                signature: *blake3::hash(
                    serde_json::json!([anchored, local]).to_string().as_bytes(),
                )
                .as_bytes(),
                sequence,
            },
        );
        if membership.contains_key(id) {
            continue;
        }
        let index = groups.len();
        let mut group = Vec::new();
        let mut queue = VecDeque::from([id.clone()]);
        membership.insert(id.clone(), index);
        while let Some(part) = queue.pop_front() {
            group.push(part.clone());
            for other in adjacency.get(&part).into_iter().flatten() {
                if !membership.contains_key(other) {
                    membership.insert(other.clone(), index);
                    queue.push_back(other.clone());
                }
            }
        }
        group.sort();
        groups.push(group);
    }
    Snapshot(Some(View {
        edges,
        parts,
        groups,
        membership,
    }))
}
impl AssemblyHistory {
    pub(crate) fn forget_removed(&mut self, id: &str) {
        if !self.0.removed.contains(id) {
            return;
        }
        let h = Rc::make_mut(&mut self.0);
        h.removed.remove(id);
        h.bytes -= id.len();
    }
    pub(crate) fn received_removed(&self, id: &str) -> bool {
        self.0.removed.contains(id)
    }
    pub(crate) fn confirm_removed(&mut self, id: Arc<str>, sequence: u64) {
        if self.0.removed.contains(&id) {
            return;
        }
        let h = Rc::make_mut(&mut self.0);
        if h.parts.len() + h.removed.len() >= MAX_PARTS || h.bytes + id.len() > ID_BYTES {
            h.overflow = h.overflow.max(sequence);
            return;
        }
        h.bytes += id.len();
        h.removed.insert(id);
    }
    pub(crate) fn current(&self, id: &str, sequence: u64) -> bool {
        self.0.overflow <= sequence && self.0.parts.get(id).is_none_or(|s| *s <= sequence)
    }
    pub(crate) fn record(&mut self, before: Snapshot, after: Snapshot, sequence: u64) {
        let (Some(before), Some(after)) = (before.0, after.0) else {
            Rc::make_mut(&mut self.0).overflow = sequence.max(self.0.overflow);
            return;
        };
        // Union old/new components once. This reaches both sides of a split and
        // every component involved in a merge without rescanning per member.
        let ids = before
            .parts
            .keys()
            .chain(after.parts.keys())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if ids.len() > MAX_PARTS {
            Rc::make_mut(&mut self.0).overflow = sequence.max(self.0.overflow);
            return;
        }
        let indexes = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.clone(), i))
            .collect::<BTreeMap<_, _>>();
        let mut parent = (0..ids.len()).collect::<Vec<_>>();
        fn root(parent: &mut [usize], mut i: usize) -> usize {
            while parent[i] != i {
                parent[i] = parent[parent[i]];
                i = parent[i];
            }
            i
        }
        for group in before.groups.iter().chain(after.groups.iter()) {
            if let Some(first) = group.first() {
                for id in group.iter().skip(1) {
                    let a = root(&mut parent, indexes[first]);
                    let b = root(&mut parent, indexes[id]);
                    parent[b] = a;
                }
            }
        }
        let mut stamps = vec![0; ids.len()];
        let mut forced = vec![false; ids.len()];
        for id in before.edges.keys().chain(
            after
                .edges
                .keys()
                .filter(|id| !before.edges.contains_key(*id)),
        ) {
            let old = before.edges.get(id);
            let new = after.edges.get(id);
            let retirement = old.is_some_and(|e| e.state.retired && e.sequence > 0);
            if old.map(|e| &e.state) == new.map(|e| &e.state) && !retirement {
                continue;
            }
            let seq = old
                .map_or(0, |e| e.sequence)
                .max(new.map_or(0, |e| e.sequence));
            let confirm_local = retirement
                || (old.is_some_and(|e| e.state.local) && new.is_none_or(|e| !e.state.local));
            for end in old
                .into_iter()
                .chain(new)
                .flat_map(|e| e.state.ends.iter().chain(e.state.original.iter()))
                .flatten()
            {
                let r = root(&mut parent, indexes[end]);
                stamps[r] = stamps[r].max(seq);
                forced[r] |= confirm_local;
            }
        }
        for (id, index) in &indexes {
            let old = before.parts.get(id);
            let new = after.parts.get(id);
            if old.map(|p| p.signature) != new.map(|p| p.signature) {
                let r = root(&mut parent, *index);
                stamps[r] = stamps[r].max(
                    old.map_or(0, |p| p.sequence)
                        .max(new.map_or(0, |p| p.sequence)),
                );
            }
        }
        let old_digests = before.groups.iter().map(|g| digest(g)).collect::<Vec<_>>();
        let new_digests = after.groups.iter().map(|g| digest(g)).collect::<Vec<_>>();
        for (id, index) in indexes {
            let r = root(&mut parent, index);
            if stamps[r] == 0 {
                continue;
            }
            let singleton = || digest(std::slice::from_ref(&id));
            let old = before
                .membership
                .get(&id)
                .map_or_else(singleton, |i| old_digests[*i]);
            let new = after
                .membership
                .get(&id)
                .map_or_else(singleton, |i| new_digests[*i]);
            if old == new && !forced[r] {
                continue;
            }
            let h = Rc::make_mut(&mut self.0);
            if !h.parts.contains_key(&id) {
                if h.parts.len() + h.removed.len() >= MAX_PARTS || h.bytes + id.len() > ID_BYTES {
                    h.overflow = h.overflow.max(stamps[r]);
                    continue;
                }
                h.bytes += id.len();
            }
            let stamp = h.parts.entry(id).or_default();
            *stamp = (*stamp).max(stamps[r]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn snapshot() -> Json {
        json!({"version":1,"sequence":0,"nodes":[
            {"id":"game","class_name":"DataModel"},
            {"id":"ws","class_name":"Workspace","parent":"game"},
            {"id":"p","class_name":"Part","parent":"ws","properties":{"Anchored":false}},
            {"id":"q","class_name":"Part","parent":"ws","properties":{"Anchored":false}},
            {"id":"j","class_name":"WeldConstraint","parent":"ws","properties":{"Part0":{"$ref":"p"},"Part1":{"$ref":"q"},"Enabled":true}}
        ]})
    }
    #[test]
    fn topology_stamps_share_until_written_and_allow_newer_reports() {
        let mut world = World::from_snapshot(&snapshot(), None).unwrap();
        let old = world.fork_for_actor("a");
        assert!(Rc::ptr_eq(
            &old.assembly_history.0,
            &world.assembly_history.0
        ));
        world.apply_delta(&json!({"version":1,"delta":true,"sequence":1,"actions":[{"kind":"set","id":"j","sequence":1,"fields":{"Enabled":false}}]}),None).unwrap();
        assert!(!world.assembly_history.current("p", 0));
        assert!(!world.assembly_history.current("q", 0));
        assert!(world.assembly_history.current("p", 1));
        assert!(old.assembly_history.current("p", 0));
        assert!(!Rc::ptr_eq(
            &old.assembly_history.0,
            &world.assembly_history.0
        ));
        let mut actor = old;
        actor.sync_actor_fork(&world);
        assert!(Rc::ptr_eq(
            &actor.assembly_history.0,
            &world.assembly_history.0
        ));
    }
    #[test]
    fn history_entry_and_identity_bounds_are_conservative() {
        let mut history = AssemblyHistory::default();
        for i in 0..MAX_PARTS + 10 {
            history.confirm_removed(Arc::from(format!("joint{i}")), i as u64 + 1);
        }
        assert_eq!(history.0.removed.len(), MAX_PARTS);
        assert!(history.0.bytes <= ID_BYTES);
        assert!(!history.current("p", 0));
        assert!(history.current("p", (MAX_PARTS + 10) as u64));
        let mut history = AssemblyHistory::default();
        history.confirm_removed(Arc::from("x".repeat(ID_BYTES + 1)), 3);
        assert!(history.0.removed.is_empty());
        assert!(!history.current("p", 2));
        assert!(history.current("p", 3));
        let mut data = snapshot();
        for i in 0..MAX_EDGES {
            data["nodes"]
                .as_array_mut()
                .unwrap()
                .push(json!({"id":format!("extra{i}"),"class_name":"WeldConstraint"}));
        }
        let world = World::from_snapshot(&data, None).unwrap();
        assert!(capture(&world, &Causes(HashMap::new())).0.is_none());
    }
    #[test]
    fn captures_keep_unrelated_shared_part_properties_packed() {
        use crate::{
            engine_world_projection::ProjectionContext, engine_world_view::SharedWorldView,
        };
        use rove_client_core::{schema::Schema, world::WorldStore};
        let mut data = snapshot();
        for i in 0..1000 {
            data["nodes"]
                .as_array_mut()
                .unwrap()
                .push(json!({"id":format!("untouched{i}"),"class_name":"Part","parent":"ws"}));
        }
        let store = WorldStore::from_snapshot(&data).unwrap();
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
        let state = capture(&world, &Causes(HashMap::new())).0.unwrap();
        assert_eq!(state.groups.len(), 1);
        assert_eq!(state.parts.len(), 2);
        for i in 0..1000 {
            assert!(world.nodes.peek(&format!("untouched{i}")).is_none());
        }
    }
    #[test]
    fn borrowed_graph_keeps_attachment_constraints_and_mode_errors() {
        let mut data = snapshot();
        data["nodes"].as_array_mut().unwrap().extend([
            json!({"id":"attachment","class_name":"Attachment","parent":"p"}),
            json!({"id":"force","class_name":"VectorForce","parent":"ws","properties":{"Attachment0":{"$ref":"attachment"},"Enabled":true,"RelativeTo":{"$enum":{"type":"ActuatorRelativeTo","value":2}}}})
        ]);
        let mut world = World::from_snapshot(&data, None).unwrap();
        let graph = Graph::build_in_world(&world, Scope::All).unwrap();
        assert!(graph.active("force").unwrap());
        assert!(graph.active("j").unwrap());
        assert_eq!(graph.connected("p", true).unwrap(), vec!["q"]);
        world.nodes.get_mut("force").unwrap().properties.insert(
            "RelativeTo".into(),
            json!({"$enum":{"type":"ActuatorRelativeTo","value":99}}),
        );
        assert!(Graph::build_in_world(&world, Scope::All)
            .unwrap()
            .active("force")
            .is_err());
    }
}
