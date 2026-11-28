//! rove's live stream applied to a rustudio world.
//!
//! The offline half ([`crate::world`]) builds a whole world from a snapshot in
//! one pass. This is the other case: instances arrive a piece at a time, in
//! receipt order, and each piece is a small edit to a world that is already
//! being drawn.
//!
//! The seam is rove's own vocabulary, not a copy of it. `rove-client-core`'s
//! `SessionCore` takes the packets a live session receives (a schema, the
//! globals, replication) and publishes them as journal records after decoding;
//! the records carry world actions in the shape
//!
//! ```json
//! {"sequence": 7, "kind": "create", "id": "3:12", "node": {…}}
//! {"sequence": 8, "kind": "set", "id": "3:12", "fields": {"Size": {"$vector3": …}}}
//! {"sequence": 9, "kind": "remove", "id": "3:12"}
//! {"sequence": 10, "kind": "event", "id": "3:12", "event": "ChildAdded", …}
//! ```
//!
//! that rove's `WorldStore` consumes. So the live path never re-decodes the
//! wire (that would be a second decoder, drifting from rove's), and it never
//! converts a value twice: [`crate::snapshot::ValueConverter`] is the one
//! reader of rove's `$`-tagged values, whether they come from a snapshot or
//! from a packet.
//!
//! What this module does only: **Refs**. rove names instances `scope:index`;
//! the client names them `Ref`, and the mapping has to live somewhere. Here,
//! in one place, with both directions and the counts that say whether it is
//! keeping up.

/// The peer id a joined world's creates are stamped with: rove's server is peer
/// `1` in the session ids it hands out (`rove/rust-client/src/live_physics.rs`,
/// `SERVER_NETWORK_OWNER`), and the client's own scope is a larger word.
pub const SERVER_PEER: u64 = 1;

use std::collections::{HashMap, HashSet};

use rbx_dom_weak::types::{Ref, Variant};
use rbx_dom_weak::{ustr, InstanceBuilder};
use rustudio_core::doc::Document;
use serde_json::Value as J;

use crate::snapshot::ValueConverter;

/// What a batch of rove actions did to the world, for the harness to print.
#[derive(Debug, Clone, Default)]
pub struct ApplyStats {
    /// Actions read from rove's journal.
    pub actions: usize,
    /// `create` actions that made an instance.
    pub created: usize,
    /// `create` actions whose rove id was already in the world (a re-send, not
    /// a second instance: rove can repeat a create after a reconnect).
    pub duplicate_creates: usize,
    /// `set` actions that wrote at least one property.
    pub sets: usize,
    /// Properties written by `set` actions.
    pub properties: usize,
    /// `set` actions naming an id the world does not have (arrived after the
    /// instance was removed, or before its create was journaled).
    pub unknown_ids: usize,
    /// `remove` actions that destroyed a subtree.
    pub removed: usize,
    /// Instances destroyed, the subtree included.
    pub instances_removed: usize,
    /// Parent edges adopted (`fields.Parent`, and a create's parent).
    pub parents: usize,
    /// Creates whose class the reflection database does not know. The instance
    /// is still made; this is the count that explains a world of Models with
    /// no parts in them.
    pub unknown_classes: usize,
    /// Events carried by the journal (the Lua dispatch is the engine's).
    pub events: usize,
    /// Values the converter refused, so the property kept its default.
    pub refused_values: usize,
    /// Wall time this batch took.
    pub duration_ms: f64,
}

impl ApplyStats {
    pub fn add(&mut self, other: &ApplyStats) {
        self.actions += other.actions;
        self.created += other.created;
        self.duplicate_creates += other.duplicate_creates;
        self.sets += other.sets;
        self.properties += other.properties;
        self.unknown_ids += other.unknown_ids;
        self.removed += other.removed;
        self.instances_removed += other.instances_removed;
        self.parents += other.parents;
        self.unknown_classes += other.unknown_classes;
        self.events += other.events;
        self.refused_values += other.refused_values;
        self.duration_ms += other.duration_ms;
    }

    /// The one line the harness prints (and `--result` carries).
    pub fn line(&self) -> String {
        format!(
            "{} actions: {} created ({} repeat), {} sets ({} properties), \
             {} removed ({} instances), {} parents, {} events; {} unknown classes, \
             {} unknown ids, {} values kept their default, in {:.2} ms",
            self.actions,
            self.created,
            self.duplicate_creates,
            self.sets,
            self.properties,
            self.removed,
            self.instances_removed,
            self.parents,
            self.events,
            self.unknown_classes,
            self.unknown_ids,
            self.refused_values,
            self.duration_ms,
        )
    }

    pub fn json(&self) -> J {
        serde_json::json!({
            "actions": self.actions,
            "created": self.created,
            "duplicate_creates": self.duplicate_creates,
            "sets": self.sets,
            "properties": self.properties,
            "unknown_ids": self.unknown_ids,
            "removed": self.removed,
            "instances_removed": self.instances_removed,
            "parents": self.parents,
            "unknown_classes": self.unknown_classes,
            "events": self.events,
            "refused_values": self.refused_values,
            "duration_ms": self.duration_ms,
        })
    }
}

/// The id to `Ref` mapping a live session adds to a built world, and the one
/// place actions are applied.
///
/// It borrows nothing: the document is passed in on every call, because a live
/// session's world is re-made by Play/Stop and must not be held here.
pub struct Live {
    ids: HashMap<String, Ref>,
    refs: HashMap<Ref, String>,
    /// rove ids whose create arrived before their parent's. A create's parent
    /// may be journaled after the child (replication is ordered by receipt,
    /// not by tree), so the child is made under the root and moved when the
    /// parent arrives — the same deferral the snapshot build does.
    waiting_for_parent: HashMap<String, String>,
    /// Events too old to dispatch (their instances are gone).
    dropped_events: usize,
    /// Every `Class.Property` the converter refused, counted. A join whose
    /// DataModel is missing pieces says so here by name rather than as a
    /// number: "{} refused" is a fault, `Part.Shape` refused 412 times is a
    /// thing to go and fix.
    refused_properties: std::collections::BTreeMap<String, usize>,
    /// Creates whose class the reflection database does not know (or which
    /// named none). Every one of these made an instance nothing can draw or
    /// simulate, so the count is the answer to "why is the world half here".
    refused_classes: std::collections::BTreeMap<String, usize>,
    /// Properties written, by `Class.Property`. Not a fault list — this is the
    /// evidence that properties *are* replicating: a report with `Part.CFrame
    /// ×1900` in it is a world being updated, and the same report without one
    /// is a world that arrived and stopped.
    written_properties: std::collections::BTreeMap<String, usize>,
    /// `$ref` targets that were not in the world when the property arrived and
    /// were dropped: a dangling reference, which is how a Model loses its
    /// PrimaryPart. Only a reference that *cannot* be kept is counted here —
    /// one whose target is simply late is held in `pending_refs` instead.
    dangling_refs: std::collections::BTreeMap<String, usize>,
    /// A `$ref` that arrived before its target, kept until the target does:
    /// `(owner id, property) → target id`. Roblox's own stream does this all
    /// the time — a `Model.PrimaryPart` is set in the same delta that creates
    /// the part, and `Player.Character` names a model the server has not sent
    /// yet — and dropping the property is what leaves a Model with no
    /// PrimaryPart and a player with no character, on a world where every
    /// value arrived.
    pending_refs: std::collections::BTreeMap<(String, String), String>,
    /// `$ref`s resolved by the batch that made their target, for the report.
    refs_bound: usize,
    /// The network peer these deltas came from (the server's session id), stamped
    /// onto every instance created here (`Document::replicated_peer`).
    peer: u64,
}

impl Live {
    pub fn new() -> Live {
        Live {
            ids: HashMap::new(),
            refs: HashMap::new(),
            waiting_for_parent: HashMap::new(),
            dropped_events: 0,
            written_properties: std::collections::BTreeMap::new(),
            refused_properties: std::collections::BTreeMap::new(),
            refused_classes: std::collections::BTreeMap::new(),
            dangling_refs: std::collections::BTreeMap::new(),
            pending_refs: std::collections::BTreeMap::new(),
            refs_bound: 0,
            peer: SERVER_PEER,
        }
    }

    /// Start from a world the snapshot build produced, so a joined session's
    /// first delta applies to the instances the join already brought.
    pub fn from_world(world: &crate::world::World) -> Live {
        Live {
            ids: world.ids.clone(),
            refs: world.refs.clone(),
            waiting_for_parent: HashMap::new(),
            dropped_events: 0,
            written_properties: std::collections::BTreeMap::new(),
            refused_properties: std::collections::BTreeMap::new(),
            refused_classes: std::collections::BTreeMap::new(),
            dangling_refs: std::collections::BTreeMap::new(),
            pending_refs: std::collections::BTreeMap::new(),
            refs_bound: 0,
            peer: SERVER_PEER,
        }
    }

    /// The `Ref` rove's id means in this world, if it has arrived.
    pub fn referent(&self, id: &str) -> Option<Ref> {
        self.ids.get(id).copied()
    }

    /// The rove id a `Ref` means (`$ref`, remotes and physics read this way).
    pub fn id_of(&self, r: Ref) -> Option<&str> {
        self.refs.get(&r).map(String::as_str)
    }

    /// Whether every `Ref` this half holds still means an instance in `doc`.
    ///
    /// The check a session makes when it starts playing the world the join
    /// built: the editor's half and the document it writes into are two things
    /// that can come apart (a Play that swapped the document, a document
    /// swapped back), and a map that points at another document's refs writes
    /// every later delta into nothing — the world arrives once and the stream
    /// stops meaning anything. Being wrong about this is a game that freezes
    /// the moment it is entered, with every packet still arriving.
    pub fn matches(&self, doc: &Document) -> bool {
        self.ids.values().all(|r| doc.dom.get_by_ref(*r).is_some())
            && self.refs.keys().all(|r| doc.dom.get_by_ref(*r).is_some())
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// How many creates are still waiting for the parent they name.
    pub fn waiting(&self) -> usize {
        self.waiting_for_parent.len()
    }

    pub fn dropped_events(&self) -> usize {
        self.dropped_events
    }

    /// `Class.Property` values the converter refused, most-frequent first.
    ///
    /// A live world that is missing pieces is diagnosable from this list: the
    /// top entry is almost always one property in one class, and knowing which
    /// is the difference between "the join is broken" and "`Part.Shape` is not
    /// converted yet".
    /// Owned strings on purpose: a caller reports these while holding `&mut`
    /// elsewhere (the overlay prints them), and `&str` borrowed from the ledger
    /// would make that borrow conflict for no reason.
    pub fn refused_properties(&self) -> Vec<(String, usize)> {
        let mut out: Vec<(String, usize)> = self
            .refused_properties
            .iter()
            .map(|(name, count)| (name.clone(), *count))
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    /// Properties written, most-frequent first, by `Class.Property`.
    ///
    /// This is the positive counterpart to `refused_properties`: "are properties
    /// arriving at all" is answered by `Part.CFrame ×1900` being here, and by
    /// nothing else in the report.
    pub fn written_properties(&self) -> Vec<(String, usize)> {
        let mut out: Vec<(String, usize)> = self
            .written_properties
            .iter()
            .map(|(name, count)| (name.clone(), *count))
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    /// Creates refused because the node named no class.
    pub fn refused_classes(&self) -> Vec<(String, usize)> {
        self.refused_classes
            .iter()
            .map(|(name, count)| (name.clone(), *count))
            .collect()
    }

    /// References that were held for a target that arrived later, and are now
    /// written: the positive counterpart to `dangling_refs`.
    pub fn refs_bound(&self) -> usize {
        self.refs_bound
    }

    /// References still held, waiting for their target: `(owner, property)`.
    pub fn refs_pending(&self) -> usize {
        self.pending_refs.len()
    }

    /// Properties dropped because the `$ref` target was not in the world yet,
    /// by `Class.Property`.
    pub fn dangling_refs(&self) -> Vec<(String, usize)> {
        self.dangling_refs
            .iter()
            .map(|(name, count)| (name.clone(), *count))
            .collect()
    }

    /// One batch of journal records' world actions, applied to `doc`.
    ///
    /// `actions` is what `SessionRecord::world_delta` derefs to (`actions`), in
    /// receipt order; the journal's own sequence decides the order, so a caller
    /// must pass them in the order the records gave them and not re-sort by
    /// anything else.
    pub fn apply(&mut self, doc: &mut Document, actions: &[J], conv: &mut ValueConverter) -> ApplyStats {
        let started = std::time::Instant::now();
        let mut stats = ApplyStats::default();
        for action in actions {
            stats.actions += 1;
            match action.get("kind").and_then(J::as_str) {
                Some("create") => self.create(doc, action, conv, &mut stats),
                Some("set") => self.set(doc, action, conv, &mut stats),
                Some("remove") => self.remove(doc, action, &mut stats),
                Some("event") => stats.events += 1,
                // rove's vocabulary has exactly these four; anything else is a
                // version skew worth counting rather than ignoring.
                _ => stats.unknown_ids += 1,
            }
        }
        // A `$ref` whose target arrived in this batch, then a create that named
        // a parent which has now arrived.
        self.bind_pending_refs(doc, &mut stats);
        self.attach_waiting(doc, &mut stats);
        stats.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
        stats
    }

    fn create(&mut self, doc: &mut Document, action: &J, conv: &mut ValueConverter, stats: &mut ApplyStats) {
        let Some(id) = action.get("id").and_then(J::as_str) else {
            stats.unknown_ids += 1;
            return;
        };
        if self.ids.contains_key(id) {
            // rove re-sends a create it has already journaled (after a
            // reconnect); the world already has the instance.
            stats.duplicate_creates += 1;
            return;
        }
        let Some(node) = action.get("node") else {
            stats.unknown_ids += 1;
            return;
        };
        let Some(class) = node.get("class_name").and_then(J::as_str) else {
            stats.unknown_ids += 1;
            *self.refused_classes.entry("<no class_name>".to_owned()).or_default() += 1;
            return;
        };
        // The parent, if this delta brought one and the world has it; else the
        // root, with a note for `attach_waiting`.
        let wanted_parent = node.get("parent").and_then(parent_id);
        let root_ref = doc.dom.root_ref();
        // The parent has to be *in the DOM*, not merely in the id map. A remove
        // in an earlier batch drops the id, but a re-sent create for a child of
        // a parent the journal has re-created in a new generation can name an id
        // whose ref this side still holds, and `WeakDom::insert` **panics** with
        // "cannot insert into parent that does not exist" (dom.rs:313) rather
        // than refusing — the phone's crash report, 22:30:22, was this line.
        let parent_ref = wanted_parent
            .as_deref()
            .and_then(|p| self.ids.get(p).copied())
            .filter(|r| doc.dom.get_by_ref(*r).is_some())
            .unwrap_or(root_ref);
        let name = node
            .get("properties")
            .and_then(|p| p.get("Name"))
            .and_then(name_of)
            .or_else(|| node.get("name").and_then(name_of))
            .unwrap_or_default();
        // A class the reflection database does not know cannot be rendered,
        // simulated or edited — `Scene` has no geometry for it and the physics
        // has no shape. It is still inserted (the server said it exists, and a
        // later delta may make it real), but it is *named* in the report,
        // because "the welds are missing" is answered by "Motor6D was created
        // 214 times" and by nothing else.
        if !rustudio_core::doc::class_known(class) {
            *self.refused_classes.entry(class.to_owned()).or_default() += 1;
            stats.unknown_classes += 1;
        }
        let instance = InstanceBuilder::new(class);
        let instance = if name.is_empty() {
            instance
        } else {
            instance.with_name(name)
        };
        let r = doc.dom.insert(parent_ref, instance);
        self.ids.insert(id.to_string(), r);
        self.refs.insert(r, id.to_string());
        // **Whose instance this is.** Every create that arrives here came over
        // the wire, and the peer that sent it owns its physics unless it hands
        // the assembly to us (`NetworkOwnerV3`). `Document::owner` reads this to
        // decide what a local solver may integrate — the alternative is a client
        // that simulates the whole of somebody else's place, where unanchored
        // parts fall out of a world nobody dropped them in.
        doc.replicated_peer.insert(r, self.peer);
        stats.created += 1;
        // A new instance is new to every reader of the document, not only to the
        // one that made it: `Scene::sync` learns about changes from the journal's
        // marks, so a create without a mark is an instance in the world that
        // nothing draws. One mark for one subtree — never `mark_all`, which
        // rebuilds the whole place.
        doc.mark_tree(r);
        if parent_ref != doc.dom.root_ref() {
            stats.parents += 1;
        } else if let Some(parent) = wanted_parent {
            self.waiting_for_parent.insert(id.to_string(), parent);
        }

        // The create's properties: the converter is the only reader of rove's
        // tagged values, and the class decides an untagged one's type.
        if let Some(properties) = node.get("properties").and_then(J::as_object) {
            let mut values: Vec<(String, Variant)> = Vec::with_capacity(properties.len());
            for (property, value) in properties {
                if property == "Name" {
                    continue;
                }
                match self.convert(property, class, value, conv) {
                    Ok(v) => values.push((property.clone(), v)),
                    // A reference into the same delta: the target is made by a
                    // create later in this batch, so it is held, not refused.
                    // `Model.PrimaryPart` and a weld's `Part1` arrive this way.
                    Err(ConvertError::Ref(target)) => {
                        values.push((property.clone(), Variant::Ref(Ref::none())));
                        self.hold_ref(id, property, &target);
                    }
                    Err(ConvertError::Value(_)) => {
                        self.refuse(class, property, stats);
                    }
                }
            }
            if let Some(instance) = doc.dom.get_by_ref_mut(r) {
                for (property, value) in values {
                    instance.properties.insert(ustr(&property), value);
                    stats.properties += 1;
                }
            }
        }
    }

    fn set(&mut self, doc: &mut Document, action: &J, conv: &mut ValueConverter, stats: &mut ApplyStats) {
        let Some(id) = action.get("id").and_then(J::as_str) else {
            stats.unknown_ids += 1;
            return;
        };
        let Some(&r) = self.ids.get(id) else {
            stats.unknown_ids += 1;
            return;
        };
        let Some(fields) = action.get("fields").and_then(J::as_object) else {
            stats.sets += 1;
            return;
        };
        let class = doc
            .dom
            .get_by_ref(r)
            .map(|i| i.class.to_string())
            .unwrap_or_else(|| "Instance".to_string());
        stats.sets += 1;
        for (property, value) in fields {
            if property == "Parent" {
                if let Some(parent) = parent_id(value) {
                    self.reparent(doc, id, &parent, stats);
                }
                continue;
            }
            match self.convert(property, &class, value, conv) {
                Ok(v) => {
                    let key = ustr(property);
                    if let Some(instance) = doc.dom.get_by_ref_mut(r) {
                        instance.properties.insert(key, v);
                        stats.properties += 1;
                        *self
                            .written_properties
                            .entry(format!("{class}.{property}"))
                            .or_default() += 1;
                    }
                    // The mark is what tells the scene, the physics and the
                    // Properties panel that this instance changed. Without it a
                    // moving part is a part that never moves on screen.
                    doc.mark_prop(r, key);
                }
                // `$ref`: a property that points at another instance (a
                // Player's Character, a seat's Occupant). Resolved here, where
                // the id map is, and dropped — counted — when the target is
                // not in the world yet.
                Err(ConvertError::Ref(target)) => match self.ids.get(&target).copied() {
                    Some(target_ref) => {
                        let key = ustr(property);
                        if let Some(instance) = doc.dom.get_by_ref_mut(r) {
                            instance.properties.insert(key, Variant::Ref(target_ref));
                            stats.properties += 1;
                            // A late resolution of something held earlier is a
                            // property written now, and the report's positive
                            // count should say so.
                            *self
                                .written_properties
                                .entry(format!("{class}.{property}"))
                                .or_default() += 1;
                        }
                        doc.mark_prop(r, key);
                    }
                    // The target had not arrived when the property naming it
                    // did: `Player.Character` is set in the delta that creates
                    // the model, a `Model.PrimaryPart` in the one that creates
                    // the part. Held, not dropped — dropping it is what leaves a
                    // Model with no PrimaryPart and a player with no character
                    // on a world where every value arrived.
                    None => {
                        self.hold_ref(id, property, &target);
                        // `Ref::none()` while it waits: not a ref into
                        // nowhere (the scene walk and the physics reader would
                        // follow one) and not nil-as-a-real-target either — it
                        // is what an unset reference is in this DOM.
                        if let Some(instance) = doc.dom.get_by_ref_mut(r) {
                            instance
                                .properties
                                .insert(ustr(property.as_str()), Variant::Ref(Ref::none()));
                        }
                    }
                }
                Err(ConvertError::Value(_)) => {
                    // `class` is owned here (it is the value read from the
                    // document above), so the ledger takes a borrow of it.
                    self.refuse(&class, property, stats);
                }
            }
        }
    }

    fn remove(&mut self, doc: &mut Document, action: &J, stats: &mut ApplyStats) {
        let Some(id) = action.get("id").and_then(J::as_str) else {
            stats.unknown_ids += 1;
            return;
        };
        let Some(&r) = self.ids.get(id) else {
            stats.unknown_ids += 1;
            return;
        };
        // The subtree goes with it; the id map must not keep the refs of
        // instances that no longer exist, or a later `$ref` binds a dead ref.
        // `descendants` yields `&Instance` (the ref is `referent()`), and a
        // whole map is dropped per batch rather than per instance.
        // `descendants_of` *panics* when the ref is not in the DOM, and this is
        // the one place a live stream can hand it a ref that is not: the server
        // may send a remove for an instance a previous remove in the same batch
        // already destroyed, and a writer that panicked on that took the
        // editor's thread down with it (a phone log, 21:10:21).
        let doomed: Vec<Ref> = match doc.dom.get_by_ref(r) {
            Some(_) => doc.dom.descendants_of(r).map(|i| i.referent()).collect(),
            None => {
                // Gone already: forget the id so a later `$ref` cannot bind it,
                // and count it as the duplicate it is.
                if let Some(id) = self.refs.remove(&r) {
                    self.ids.remove(&id);
                    self.waiting_for_parent.remove(&id);
                }
                stats.unknown_ids += 1;
                return;
            }
        };
        for d in &doomed {
            if let Some(id) = self.refs.remove(d) {
                self.ids.remove(&id);
                self.waiting_for_parent.remove(&id);
            }
        }
        stats.removed += 1;
        // The subtree minus the instance itself, which `destroy` takes with it.
        stats.instances_removed += doomed.len().saturating_sub(1);
        if !doomed.is_empty() && r != doc.dom.root_ref() {
            // Marked before the destruction: a reader that looks the ref up to
            // find out what was there finds nothing once it is gone, so the
            // scene is told first and re-checks the subtree while it can.
            doc.mark_tree(r);
            doc.dom.destroy(r);
        }
    }

    /// `fields.Parent`, when the parent has arrived; else remembered.
    fn reparent(&mut self, doc: &mut Document, id: &str, parent: &str, stats: &mut ApplyStats) {
        let Some(&r) = self.ids.get(id) else {
            stats.unknown_ids += 1;
            return;
        };
        match self.ids.get(parent).copied() {
            // `transfer_within` panics on a parent that is not in the DOM, and
            // on a *destroyed* child — both reachable here, because a re-sent
            // create and a remove can name the same id in one session
            // (dom.rs:313 again). Anything already gone becomes a waiting edge,
            // which is where a parent that has not arrived yet goes too.
            Some(parent_ref)
                if doc.dom.get_by_ref(parent_ref).is_some()
                    && doc.dom.get_by_ref(r).is_some() =>
            {
                if r != parent_ref && r != doc.dom.root_ref() {
                    doc.dom.transfer_within(r, parent_ref);
                    // A move changes the tree, not a property: the old parent
                    // has lost a child and the new one has gained one.
                    doc.mark_tree(r);
                    stats.parents += 1;
                    self.waiting_for_parent.remove(id);
                }
            }
            _ => {
                self.waiting_for_parent.insert(id.to_string(), parent.to_string());
            }
        }
    }

    /// A create that named a parent which was not in the world yet, moved now
    /// that it is. Bounded by the number of creates, so one pass is enough for
    /// a chain that arrived in reverse.
    fn attach_waiting(&mut self, doc: &mut Document, stats: &mut ApplyStats) {
        if self.waiting_for_parent.is_empty() {
            return;
        }
        let mut settled: Vec<(String, String)> = Vec::new();
        for (id, parent) in &self.waiting_for_parent {
            if self.ids.contains_key(parent) {
                settled.push((id.clone(), parent.clone()));
            }
        }
        for (id, parent) in settled {
            self.reparent(doc, &id, &parent, stats);
            self.waiting_for_parent.remove(&id);
        }
    }

    /// Count a value the converter would not take — unless the property is one
    /// no client can read in the first place.
    ///
    /// `Player.User` (a `User`, `RobloxSecurity`), `Player.GameplayPaused` and
    /// `Player.CharacterRegionId` arrive because the server serialises its own
    /// Player. Their types have no reader on this side and there is nothing to
    /// read: they are not a conversion the client got wrong, and the phone's
    /// report listed two of them among 75 "refused properties" on a world that
    /// was arriving fine. A ledger full of expected refusals is a ledger nobody
    /// reads, which is the one fault worse than a missing entry.
    fn refuse(&mut self, class: &str, property: &str, stats: &mut ApplyStats) {
        if crate::snapshot::not_client_readable(class, property) {
            return;
        }
        stats.refused_values += 1;
        *self
            .refused_properties
            .entry(format!("{class}.{property}"))
            .or_default() += 1;
    }

    /// Remember a `$ref` whose target has not arrived, keyed by the owner and
    /// the property so a later batch can write it (`bind_pending_refs`).
    fn hold_ref(&mut self, owner: &str, property: &str, target: &str) {
        self.pending_refs
            .insert((owner.to_string(), property.to_string()), target.to_string());
    }

    /// Write every held `$ref` whose target is now in the world.
    ///
    /// Runs after each batch's creates, before `attach_waiting` — a reference is
    /// cheaper to settle than a parent edge and a parent may be what makes the
    /// target real. Bounded by the number of held references, and each one is
    /// removed as it settles, so this cannot grow into a per-frame walk of the
    /// world.
    fn bind_pending_refs(&mut self, doc: &mut Document, stats: &mut ApplyStats) {
        if self.pending_refs.is_empty() {
            return;
        }
        let mut settled: Vec<((String, String), Ref)> = Vec::new();
        let mut gone: Vec<(String, String)> = Vec::new();
        for ((owner, property), target) in &self.pending_refs {
            // The owner was removed while the reference was waiting: nothing
            // will ever be written for it, so it is dropped rather than held
            // for the rest of the session.
            if !self.ids.contains_key(owner) {
                gone.push((owner.clone(), property.clone()));
                continue;
            }
            let Some(target_ref) = self.ids.get(target).copied() else {
                continue;
            };
            if doc.dom.get_by_ref(target_ref).is_some() {
                settled.push(((owner.clone(), property.clone()), target_ref));
            }
        }
        for (key, target_ref) in settled {
            let (owner, property) = key.clone();
            let Some(&owner_ref) = self.ids.get(&owner) else {
                continue;
            };
            if doc.dom.get_by_ref(owner_ref).is_some() {
                if let Some(instance) = doc.dom.get_by_ref_mut(owner_ref) {
                    instance.properties.insert(ustr(&property), Variant::Ref(target_ref));
                }
                doc.mark_prop(owner_ref, ustr(&property));
                let class = doc
                    .dom
                    .get_by_ref(owner_ref)
                    .map(|i| i.class.to_string())
                    .unwrap_or_else(|| "Instance".to_string());
                *self
                    .written_properties
                    .entry(format!("{class}.{property}"))
                    .or_default() += 1;
                stats.properties += 1;
                self.refs_bound += 1;
            }
            self.pending_refs.remove(&key);
        }
        for key in gone {
            self.pending_refs.remove(&key);
        }
    }

    fn convert(
        &mut self,
        property: &str,
        class: &str,
        value: &J,
        conv: &mut ValueConverter,
    ) -> Result<Variant, ConvertError> {
        // **A null ref is a ref to nothing.** `Player.Character` is set to
        // `nil` on death and on respawn (`{"$ref": null}` on the wire), as are a
        // dying tool's `Part1` and a Model's `PrimaryPart` when its part goes:
        // three of the phone's log's refused properties were this, and each one
        // left the property at its default (a player still pointing at the
        // character that no longer exists). `Ref::none()` is what Roblox means by
        // it, and it is a value, not a refusal.
        match value.get("$ref") {
            Some(J::Null) => return Ok(Variant::Ref(Ref::none())),
            Some(v) => {
                if let Some(target) = v.as_str() {
                    return Err(ConvertError::Ref(target.to_string()));
                }
            }
            None => {}
        }
        conv.value(property, class, value)
            .map_err(ConvertError::Value)
    }

    /// Events this batch carried, for the engine to dispatch to `LocalScript`s.
    /// Returned as rove's own JSON so the dispatch reads the same shapes as
    /// rove's `WorldStore` does.
    pub fn events<'a>(&self, actions: &'a [J]) -> Vec<&'a J> {
        actions
            .iter()
            .filter(|a| a.get("kind").and_then(J::as_str) == Some("event"))
            .collect()
    }
}

impl Default for Live {
    fn default() -> Self {
        Live::new()
    }
}

enum ConvertError {
    /// The value points at another instance (`$ref`).
    Ref(String),
    /// The converter refused it; the property keeps its default.
    Value(String),
}

/// A parent in any of rove's spellings: a string, `{"$ref": id}`, or `null`.
fn parent_id(v: &J) -> Option<String> {
    match v {
        J::String(s) => Some(s.clone()),
        J::Object(o) => o.get("$ref").and_then(J::as_str).map(str::to_string),
        _ => None,
    }
}

/// A name from a property value: a plain string or rove's `{"$string": …}`.
fn name_of(v: &J) -> Option<String> {
    match v {
        J::String(s) => Some(s.clone()),
        J::Object(o) => o
            .get("$string")
            .or_else(|| o.get("$shared_string"))
            .and_then(J::as_str)
            .map(str::to_string),
        _ => None,
    }
}

/// Ids in the world that no longer answer to a live `Ref`: the check a
/// disconnect uses to decide whether the map can be dropped.
pub fn stale_ids(world_ids: &HashSet<String>, live: &Live) -> usize {
    world_ids.iter().filter(|id| live.referent(id).is_none()).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbx_dom_weak::types::Vector3;

    fn empty_world() -> (Document, Live) {
        let snapshot = crate::snapshot::parse(
            r#"{"version":1,"sequence":0,"job_id":"live","services":{},"class_parents":{},"defaults":{},"default_origins":{},"nodes":[]}"#,
        )
        .expect("an empty snapshot parses");
        let built = crate::world::build(snapshot, "Live".into()).expect("an empty world builds");
        let live = Live::from_world(&built.world);
        (built.world.doc, live)
    }

    fn action(v: J) -> J {
        v
    }

    #[test]
    fn a_create_makes_an_instance_under_its_parent() {
        let (mut doc, mut live) = empty_world();
        let mut conv = ValueConverter::default();
        let actions = vec![
            action(serde_json::json!({
                "sequence": 1, "kind": "create", "id": "3:1",
                "node": {"id": "3:1", "class_name": "Part", "parent": null,
                         "properties": {"Name": "Floor", "Size": {"$vector3": [4, 1, 2]}}}
            })),
        ];
        let stats = live.apply(&mut doc, &actions, &mut conv);
        assert_eq!(stats.created, 1);
        assert_eq!(stats.properties, 1, "Size (Name sets the instance's name)");
        assert_eq!(live.len(), 1);
        let r = live.referent("3:1").expect("the id maps to a Ref");
        let instance = doc.dom.get_by_ref(r).expect("it exists");
        assert_eq!(instance.name.as_str(), "Floor");
        assert_eq!(instance.class.as_str(), "Part");
        assert_eq!(
            instance.properties.get(&ustr("Size")),
            Some(&Variant::Vector3(rbx_dom_weak::types::Vector3::new(4.0, 1.0, 2.0)))
        );
    }

    #[test]
    fn a_child_created_before_its_parent_moves_when_the_parent_arrives() {
        let (mut doc, mut live) = empty_world();
        let mut conv = ValueConverter::default();
        // The child's create is journaled first and names a parent the world
        // does not have yet.
        let first = vec![action(serde_json::json!({
            "sequence": 1, "kind": "create", "id": "3:2",
            "node": {"id": "3:2", "class_name": "Part", "parent": "3:1",
                     "properties": {"Name": "Child"}}
        }))];
        let stats = live.apply(&mut doc, &first, &mut conv);
        assert_eq!(stats.created, 1);
        assert_eq!(live.waiting(), 1, "it is waiting for 3:1");
        let child = live.referent("3:2").unwrap();
        assert_eq!(
            doc.dom.get_by_ref(child).unwrap().parent(),
            doc.dom.root_ref(),
            "an orphan waits at the root, as the snapshot build does"
        );

        // The parent arrives.
        let second = vec![action(serde_json::json!({
            "sequence": 2, "kind": "create", "id": "3:1",
            "node": {"id": "3:1", "class_name": "Model", "parent": null,
                     "properties": {"Name": "Model"}}
        }))];
        let stats = live.apply(&mut doc, &second, &mut conv);
        assert_eq!(stats.parents, 1, "the waiting child was moved");
        assert_eq!(live.waiting(), 0);
        let parent = live.referent("3:1").unwrap();
        assert_eq!(doc.dom.get_by_ref(child).unwrap().parent(), parent);
    }

    #[test]
    fn a_set_writes_the_properties_it_names_and_adopts_a_parent() {
        let (mut doc, mut live) = empty_world();
        let mut conv = ValueConverter::default();
        let creates = vec![
            action(serde_json::json!({"sequence": 1, "kind": "create", "id": "3:1",
                "node": {"id": "3:1", "class_name": "Part", "properties": {"Name": "Floor"}}})),
            action(serde_json::json!({"sequence": 2, "kind": "create", "id": "3:2",
                "node": {"id": "3:2", "class_name": "Model", "properties": {"Name": "Model"}}})),
        ];
        live.apply(&mut doc, &creates, &mut conv);

        let sets = vec![action(serde_json::json!({
            "sequence": 3, "kind": "set", "id": "3:1",
            "fields": {"Anchored": true, "Parent": "3:2"}
        }))];
        let stats = live.apply(&mut doc, &sets, &mut conv);
        assert_eq!(stats.sets, 1);
        assert_eq!(stats.parents, 1);
        assert_eq!(stats.properties, 1, "Anchored; Parent is an edge, not a value");
        let r = live.referent("3:1").unwrap();
        assert_eq!(
            doc.dom.get_by_ref(r).unwrap().properties.get(&ustr("Anchored")),
            Some(&Variant::Bool(true))
        );
        assert_eq!(
            doc.dom.get_by_ref(r).unwrap().parent(),
            live.referent("3:2").unwrap()
        );
    }

    #[test]
    fn a_ref_field_binds_when_its_target_has_arrived_and_is_held_otherwise() {
        let (mut doc, mut live) = empty_world();
        let mut conv = ValueConverter::default();
        live.apply(
            &mut doc,
            &[action(serde_json::json!({"sequence": 1, "kind": "create", "id": "3:1",
                "node": {"id": "3:1", "class_name": "Model", "properties": {"Name": "M"}}}))],
            &mut conv,
        );
        // Target absent: **held**, not counted as an unknown id and not written
        // as a ref into nowhere. This is the case that used to drop a
        // `Player.Character` and every `Model.PrimaryPart` whose part was
        // created by a later action, and the report from the phone listed 101
        // of them. `bind_pending_refs` writes it when the target arrives.
        let stats = live.apply(
            &mut doc,
            &[action(serde_json::json!({"sequence": 2, "kind": "set", "id": "3:1",
                "fields": {"PrimaryPart": {"$ref": "3:9"}}}))],
            &mut conv,
        );
        assert_eq!(stats.unknown_ids, 0, "a late target is not an unknown id");
        assert_eq!(live.refs_pending(), 1, "it is waiting for 3:9");
        let model = live.referent("3:1").expect("the model");
        // The property is `Ref::none()` while it waits: the DOM's own "unset",
        // so nothing follows it into an instance that does not exist yet.
        assert_eq!(
            doc.prop(model, "PrimaryPart"),
            Some(Variant::Ref(Ref::none())),
            "held as an unset reference, not a ref into nowhere"
        );
        // And when the target arrives, the same batch writes it.
        let stats = live.apply(
            &mut doc,
            &[action(serde_json::json!({"sequence": 3, "kind": "create", "id": "3:9",
                "node": {"id": "3:9", "class_name": "Part", "parent": "3:1",
                         "properties": {"Name": "Base"}}}))],
            &mut conv,
        );
        assert_eq!(live.refs_pending(), 0, "settled");
        assert_eq!(live.refs_bound(), 1);
        assert!(stats.properties >= 1, "the held property was written: {stats:?}");
        let part = live.referent("3:9").expect("the part");
        assert_eq!(doc.prop(model, "PrimaryPart"), Some(Variant::Ref(part)));
    }

    #[test]
    fn a_remove_destroys_the_subtree_and_forgets_its_ids() {
        let (mut doc, mut live) = empty_world();
        let mut conv = ValueConverter::default();
        live.apply(
            &mut doc,
            &[
                action(serde_json::json!({"sequence": 1, "kind": "create", "id": "3:1",
                    "node": {"id": "3:1", "class_name": "Model", "properties": {}}})),
                action(serde_json::json!({"sequence": 2, "kind": "create", "id": "3:2",
                    "node": {"id": "3:2", "class_name": "Part", "parent": "3:1", "properties": {}}})),
            ],
            &mut conv,
        );
        let stats = live.apply(
            &mut doc,
            &[action(serde_json::json!({"sequence": 3, "kind": "remove", "id": "3:1"}))],
            &mut conv,
        );
        assert_eq!(stats.removed, 1);
        assert_eq!(stats.instances_removed, 1, "the child went with the model");
        assert!(live.referent("3:1").is_none());
        assert!(live.referent("3:2").is_none(), "no dead refs stay in the map");
        assert_eq!(live.len(), 0);
    }

    /// A remove for an instance a previous remove already destroyed must be
    /// counted, not fatal.
    ///
    /// `descendants_of` **panics** when the ref is not in the DOM, and a live
    /// stream can hand one a dead ref: the server may remove a subtree and then
    /// name a child of it, or send the same remove twice around a region
    /// rebuild. On a phone this took the editor's thread down at 21:10:21,
    /// seconds after the world had been arriving perfectly.
    #[test]
    fn a_remove_for_something_already_gone_does_not_panic() {
        let (mut doc, mut live) = empty_world();
        let mut conv = ValueConverter::default();
        live.apply(
            &mut doc,
            &[
                action(serde_json::json!({"sequence": 1, "kind": "create", "id": "3:1",
                    "node": {"id": "3:1", "class_name": "Model", "properties": {}}})),
                action(serde_json::json!({"sequence": 2, "kind": "create", "id": "3:2",
                    "node": {"id": "3:2", "class_name": "Part", "parent": "3:1",
                             "properties": {}}})),
            ],
            &mut conv,
        );
        let first = live.apply(
            &mut doc,
            &[action(serde_json::json!({"sequence": 3, "kind": "remove", "id": "3:1"}))],
            &mut conv,
        );
        assert_eq!(first.removed, 1);
        assert_eq!(first.unknown_ids, 0);
        // The child is gone with its parent, and the server names it anyway.
        let stats = live.apply(
            &mut doc,
            &[action(serde_json::json!({"sequence": 4, "kind": "remove", "id": "3:2"}))],
            &mut conv,
        );
        assert_eq!(stats.removed, 0, "there was nothing left to remove");
        assert_eq!(stats.unknown_ids, 1, "and it is counted, not ignored");
        assert_eq!(live.len(), 0, "the id map holds no dead refs");
    }

    /// A `set` on a part's `CFrame` reaches the document, and is recorded.
    ///
    /// This is the positive half of the missing-parts question. `refused` being
    /// zero said the converter accepted every property, but nothing checked that
    /// an accepted value was *written* — and a report listing only refusals
    /// could not tell "properties are replicating" from "the world arrived and
    /// stopped". A character that stands still is the second.
    #[test]
    fn a_set_writes_the_property_and_is_counted_by_class() {
        let (mut doc, mut live) = empty_world();
        let mut conv = ValueConverter::default();
        live.apply(
            &mut doc,
            &[action(serde_json::json!({"sequence": 1, "kind": "create", "id": "3:1",
                "node": {"id": "3:1", "class_name": "Part",
                         "properties": {"Name": "Floor"}}}))],
            &mut conv,
        );
        let r = live.referent("3:1").expect("the part is in the world");
        let before = doc.prop(r, "CFrame");
        // Built in two statements so the nested tags do not become a thicket of
        // braces that a reader has to count.
        let cframe = serde_json::json!({
            "$cframe": {
                "position": [0.0, 10.0, 0.0],
                "rotation": [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            }
        });
        let set = action(serde_json::json!({
            "sequence": 2,
            "kind": "set",
            "id": "3:1",
            "fields": { "CFrame": cframe },
        }));
        let stats = live.apply(&mut doc, &[set], &mut conv);
        assert_eq!(stats.properties, 1, "one property written: {stats:?}");
        assert_eq!(stats.refused_values, 0);
        let after = doc.prop(r, "CFrame");
        assert_ne!(
            format!("{before:?}"),
            format!("{after:?}"),
            "the CFrame in the document changed"
        );
        let written = live.written_properties();
        assert!(
            written.iter().any(|(name, count)| name == "Part.CFrame" && *count == 1),
            "the write is attributed to Part.CFrame: {written:?}"
        );
    }

    /// A create whose `CFrame` is the *compact* wire form — a basis selector,
    /// not nine floats — converts instead of being refused.
    ///
    /// This is the phone's `Part.CFrame ×57`: rove's type-27 payload is
    /// `{"position": …, "rotation_id": n}`, and only the nine-float shape was
    /// read. A world of parts at a default CFrame is a world that draws as void.
    #[test]
    fn a_selector_cframe_in_a_create_is_written_not_refused() {
        let (mut doc, mut live) = empty_world();
        let mut conv = ValueConverter::default();
        let cframe = serde_json::json!({
            "$cframe": {"position": [12.0, 3.0, -4.0], "rotation_id": 2}
        });
        let create = action(serde_json::json!({
            "sequence": 1,
            "kind": "create",
            "id": "3:1",
            "node": {"id": "3:1", "class_name": "Part",
                     "properties": {"Name": "Floor", "CFrame": cframe}},
        }));
        let stats = live.apply(&mut doc, &[create], &mut conv);
        assert_eq!(stats.refused_values, 0, "no refusal: {stats:?}");
        assert!(live.refused_properties().is_empty(), "{:?}", live.refused_properties());
        let r = live.referent("3:1").expect("the part is in the world");
        match doc.prop(r, "CFrame") {
            Some(Variant::CFrame(cf)) => {
                assert!((cf.position.x - 12.0).abs() < 1e-6, "{:?}", cf.position);
                assert!((cf.position.y - 3.0).abs() < 1e-6);
                assert!((cf.position.z + 4.0).abs() < 1e-6);
            }
            other => panic!("the part has the CFrame: {other:?}"),
        }
    }

    /// A `Model.PrimaryPart` naming a part created later in the same batch is
    /// held and then written — the shape that left every model in the phone's
    /// join without a PrimaryPart.
    #[test]
    fn a_ref_to_a_later_create_is_bound_when_the_target_arrives() {
        let (mut doc, mut live) = empty_world();
        let mut conv = ValueConverter::default();
        let model = serde_json::json!({
            "sequence": 1, "kind": "create", "id": "3:1",
            "node": {"id": "3:1", "class_name": "Model",
                     "properties": {"Name": "Kit", "PrimaryPart": {"$ref": "3:2"}}},
        });
        let part = serde_json::json!({
            "sequence": 2, "kind": "create", "id": "3:2",
            "node": {"id": "3:2", "class_name": "Part", "parent": "3:1",
                     "properties": {"Name": "Base"}},
        });
        let stats = live.apply(
            &mut doc,
            &[action(model), action(part)],
            &mut conv,
        );
        assert_eq!(stats.refused_values, 0, "{stats:?}");
        assert!(live.refused_properties().is_empty(), "{:?}", live.refused_properties());
        let model = live.referent("3:1").expect("the model");
        let part = live.referent("3:2").expect("the part");
        match doc.prop(model, "PrimaryPart") {
            Some(Variant::Ref(target)) => assert_eq!(target, part, "PrimaryPart bound"),
            other => panic!("PrimaryPart is a ref to the part: {other:?}"),
        }
        assert_eq!(live.refs_pending(), 0, "nothing is left waiting");
    }

    /// **A create from the server is stamped with the peer that sent it**, which
    /// is what `Document::owner` reads to decide what a local solver may
    /// integrate: a replicated part is somebody else's until a
    /// `NetworkOwnerV3` hands its assembly over.
    #[test]
    fn a_replicated_create_is_stamped_with_the_server_peer() {
        let (mut doc, mut live) = empty_world();
        let mut conv = ValueConverter::default();
        let part = serde_json::json!({
            "sequence": 1, "kind": "create", "id": "3:1",
            "node": {"id": "3:1", "class_name": "Part", "parent": null,
                     "properties": {"Name": "Floor", "Anchored": true}},
        });
        let stats = live.apply(&mut doc, &[action(part)], &mut conv);
        assert_eq!(stats.created, 1, "{stats:?}");
        let r = live.referent("3:1").expect("the part");
        assert_eq!(
            doc.replicated_peer.get(&r).copied(),
            Some(SERVER_PEER),
            "the part came from the server"
        );
        // And a document like this has an owner: `None` means "an edit place,
        // simulate everything".
        assert!(doc.owner().is_some(), "a replicated world has an owner");
    }

    /// **`NetworkOwnerV3` is read, not refused** — it is the one property that
    /// decides whose solver moves a part, and it is not in `api-dump.json` at
    /// all (`BasePart.NetworkOwnerV3 ×2` in the phone's refusal list).
    #[test]
    fn network_ownership_is_decoded_and_says_whose_part_it_is() {
        let (mut doc, mut live) = empty_world();
        let mut conv = ValueConverter::default();
        // Our own scope, as `self.core.scope()` reports it.
        doc.self_peer = Some(0x2a);
        let owned = serde_json::json!({
            "sequence": 1, "kind": "create", "id": "3:1",
            "node": {"id": "3:1", "class_name": "Part", "parent": null,
                     "properties": {"Name": "Chassis", "Anchored": false,
                                    "NetworkOwnerV3": 42}},
        });
        let server = serde_json::json!({
            "sequence": 2, "kind": "create", "id": "3:2",
            "node": {"id": "3:2", "class_name": "Part", "parent": null,
                     "properties": {"Name": "MapPart", "Anchored": false,
                                    "NetworkOwnerV3": 1}},
        });
        let nil = serde_json::json!({
            "sequence": 3, "kind": "create", "id": "3:3",
            "node": {"id": "3:3", "class_name": "Part", "parent": null,
                     "properties": {"Name": "Kept", "Anchored": false,
                                    "NetworkOwnerV3": null}},
        });
        let stats = live.apply(&mut doc, &[owned, server, nil], &mut conv);
        assert_eq!(stats.refused_values, 0, "no refusal: {stats:?}");
        assert!(
            live.refused_properties().is_empty(),
            "{:?}",
            live.refused_properties()
        );
        let owner = doc.owner().expect("a replicated world has an owner");
        let mine = live.referent("3:1").expect("ours");
        let theirs = live.referent("3:2").expect("the server's");
        let kept = live.referent("3:3").expect("nil is a value, not a refusal");
        assert!(owner.owns(&doc, mine, mine), "42 is our scope: ours to move");
        assert!(!owner.owns(&doc, theirs, theirs), "1 is the server's: only drawn");
        assert!(!owner.owns(&doc, kept, kept), "nil is the server's too");
        assert_eq!(doc.prop(kept, "NetworkOwnerV3"), Some(Variant::Int64(-1)));
        // A part this client's own scripts made is nobody's but ours, whatever
        // any `NetworkOwnerV3` says — nothing will replicate its pose.
        let made = doc.dom.insert(doc.dom.root_ref(), InstanceBuilder::new("Part"));
        assert!(owner.owns(&doc, made, made), "locally made: ours");
    }

    /// A snapshot-built world is the server's, instance for instance: without
    /// that stamp a join that started from a snapshot would read every part as
    /// this client's own (`Document::owner` says `None` for a document with no
    /// replicated instance at all) and simulate the whole map.
    #[test]
    fn a_snapshot_world_is_stamped_as_the_servers() {
        let snapshot = crate::snapshot::parse(
            r#"{"version":1,"sequence":0,"job_id":"live","services":{},
                "class_parents":{},"defaults":{},"default_origins":{},
                "nodes":[{"id":"3:1","class_name":"Part","parent":null,
                          "properties":{"Name":"MapPart"}}]}"#,
        )
        .expect("parses");
        let built = crate::world::build(snapshot, "Live".into()).expect("builds");
        // (the `ids` map in hand first: `doc` moves out of `world` below)
        let part = built.world.ids["3:1"];
        let doc = built.world.doc;
        assert_eq!(doc.replicated_peer.get(&part), Some(&SERVER_PEER));
        let owner = doc.owner().expect("a server's world");
        assert!(
            !owner.owns(&doc, part, part),
            "the server's part is not this client's to simulate"
        );
    }

    /// The three property types the phone's report counted as refused, each in
    /// rove's live spelling: `Vector3Value.Value` tagged, a part's
    /// `CustomPhysicalProperties` as the array `CurrentPhysicalProperties`
    /// carries, and a label's `FontFace` with numeric weight and style.
    #[test]
    fn the_refused_types_convert_in_the_live_spelling() {
        let (mut doc, mut live) = empty_world();
        let mut conv = ValueConverter::default();
        let creates = vec![
            action(serde_json::json!({
                "sequence": 1, "kind": "create", "id": "3:1",
                "node": {"id": "3:1", "class_name": "Vector3Value", "parent": null,
                         "properties": {"Name": "Size", "Value": {"$vector3": [1.0, 2.0, 3.0]}}},
            })),
            action(serde_json::json!({
                "sequence": 2, "kind": "create", "id": "3:2",
                "node": {"id": "3:2", "class_name": "Part", "parent": null,
                         "properties": {"Name": "Heavy", "Anchored": true,
                                        "CustomPhysicalProperties": [3.0, 0.1, 0.2, 1.0, 1.0]}},
            })),
            action(serde_json::json!({
                "sequence": 3, "kind": "create", "id": "3:3",
                "node": {"id": "3:3", "class_name": "TextLabel", "parent": null,
                         "properties": {"Name": "Label",
                                        "FontFace": {"family": "rbxasset://fonts/families/SourceSansPro.json",
                                                     "weight": 700, "style": 1}}},
            })),
        ];
        let stats = live.apply(&mut doc, &creates, &mut conv);
        assert_eq!(stats.refused_values, 0, "no refusal: {stats:?}");
        assert!(
            live.refused_properties().is_empty(),
            "{:?}",
            live.refused_properties()
        );
        let value = live.referent("3:1").expect("the value object");
        assert_eq!(
            doc.prop(value, "Value"),
            Some(Variant::Vector3(Vector3::new(1.0, 2.0, 3.0)))
        );
        let heavy = live.referent("3:2").expect("the part");
        match doc.prop(heavy, "CustomPhysicalProperties") {
            Some(Variant::PhysicalProperties(rbx_dom_weak::types::PhysicalProperties::Custom(c))) => {
                assert!((c.density() - 3.0).abs() < 1e-6, "{c:?}");
                assert!((c.friction() - 0.1).abs() < 1e-6, "{c:?}");
            }
            other => panic!("the part has its custom physics: {other:?}"),
        }
        let label = live.referent("3:3").expect("the label");
        match doc.prop(label, "FontFace") {
            Some(Variant::Font(f)) => assert_eq!(f.family, "rbxasset://fonts/families/SourceSansPro.json"),
            other => panic!("the label has its font: {other:?}"),
        }
    }

    /// A `$ref` whose target never arrives stays held (and is reported as
    /// pending), not flung into the refusal list.
    #[test]
    fn a_ref_with_no_target_yet_is_held_not_refused() {
        let (mut doc, mut live) = empty_world();
        let mut conv = ValueConverter::default();
        let set = serde_json::json!({
            "sequence": 1, "kind": "create", "id": "3:9",
            "node": {"id": "3:9", "class_name": "Player",
                     "properties": {"Name": "me", "Character": {"$ref": "3:10"}}},
        });
        let stats = live.apply(&mut doc, &[action(set)], &mut conv);
        assert_eq!(stats.refused_values, 0, "{stats:?}");
        assert!(live.refused_properties().is_empty(), "{:?}", live.refused_properties());
        assert_eq!(live.refs_pending(), 1, "held until the character arrives");
    }

    /// A create naming a parent that is in the id map but *not* in the DOM must
    /// not panic (dom.rs:313, the phone's crash report): it goes to the root and
    /// waits, as a parent that has not arrived does.
    #[test]
    fn a_create_under_a_removed_parent_does_not_panic() {
        let (mut doc, mut live) = empty_world();
        let mut conv = ValueConverter::default();
        live.apply(
            &mut doc,
            &[action(serde_json::json!({"sequence": 1, "kind": "create", "id": "3:1",
                "node": {"id": "3:1", "class_name": "Model", "properties": {}}}))],
            &mut conv,
        );
        live.apply(
            &mut doc,
            &[action(serde_json::json!({"sequence": 2, "kind": "remove", "id": "3:1"}))],
            &mut conv,
        );
        // Re-created and removed again: the id map and the DOM disagree for a
        // moment, which is what a re-sent pair of packets does.
        live.apply(
            &mut doc,
            &[action(serde_json::json!({"sequence": 3, "kind": "create", "id": "3:5",
                "node": {"id": "3:5", "class_name": "Part", "parent": "3:1",
                         "properties": {}}}))],
            &mut conv,
        );
        assert!(live.referent("3:5").is_some(), "the part is still made");
        assert_eq!(live.waiting(), 1, "and it waits for the parent to come back");
    }

    #[test]
    fn a_repeat_create_is_not_a_second_instance() {
        let (mut doc, mut live) = empty_world();
        let mut conv = ValueConverter::default();
        let create = action(serde_json::json!({"sequence": 1, "kind": "create", "id": "3:1",
            "node": {"id": "3:1", "class_name": "Part", "properties": {}}}));
        live.apply(&mut doc, &[create.clone()], &mut conv);
        let stats = live.apply(&mut doc, &[create], &mut conv);
        assert_eq!(stats.duplicate_creates, 1);
        assert_eq!(stats.created, 0);
        assert_eq!(live.len(), 1);
    }

    #[test]
    fn the_id_map_still_means_this_document_after_the_session_starts() {
        // The check a join makes when it enters the world it has been building:
        // every `Ref` the editor's half holds has to be an instance in *this*
        // document. A map that names another document's refs (a Play that
        // swapped the document, a document swapped back) writes every later
        // delta into nothing — the world arrives once and then stands still.
        let (mut doc, mut live) = empty_world();
        let mut conv = ValueConverter::default();
        live.apply(
            &mut doc,
            &[action(serde_json::json!({"sequence": 1, "kind": "create", "id": "3:1",
                "node": {"id": "3:1", "class_name": "Part", "properties": {"Name": "Floor"}}}))],
            &mut conv,
        );
        assert_eq!(live.len(), 1);
        assert!(live.matches(&doc), "the id means an instance of this document");

        // Another document, made the way a Play session makes one: new refs,
        // nothing of the live world in it.
        let other = Document::empty();
        assert!(!live.matches(&other), "the same ref means nothing in another document");
        assert!(other.dom.get_by_ref(live.referent("3:1").unwrap()).is_none());
    }

    #[test]
    fn a_set_for_an_unknown_id_is_counted_not_applied() {
        let (mut doc, mut live) = empty_world();
        let mut conv = ValueConverter::default();
        let stats = live.apply(
            &mut doc,
            &[action(serde_json::json!({"sequence": 1, "kind": "set", "id": "9:9",
                "fields": {"Anchored": true}}))],
            &mut conv,
        );
        assert_eq!(stats.unknown_ids, 1);
        assert_eq!(stats.properties, 0);
    }
}
