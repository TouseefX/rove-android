//! A snapshot as one `Document` plus the rove-id ↔ `Ref` map.
//!
//! This is the M1 bridge in its offline form: no network, no second world, no
//! VM. What it builds is the shape `replicate::initial_client`
//! (`crates/rustudio-engine/src/replicate.rs:396`) builds for a local Play
//! session — a fresh `WeakDom`, every node linked into a map of `Ref`s,
//! services at the root — so the renderer, the scenes and (M2) the engine's
//! replication path see a client DataModel and nothing new.
//!
//! **Ordering.** A node whose parent is not in the document yet waits and is
//! attached the moment its parent exists (`replicate::arrive`, `replicate.rs:634`);
//! it is never parented twice. A node whose parent never arrives stays under
//! the document root and is counted (`orphans`); a `$ref` to a node the world
//! does not have is dropped and counted (`dangling_refs`) rather than left as a
//! `Ref` into nowhere, which the scene walk and the physics reader would
//! follow.
//!
//! **Cost.** Inserting every instance under the root and moving it into place
//! after is deliberate: a node's properties may name any other node
//! (`Player.Character` names the character), so every `Ref` has to exist before
//! any property is converted. Parenting is then one `transfer_within` per node
//! — no rollback, no second pass. M2's live case is different (instances arrive
//! a piece at a time); this is the one-shot snapshot path M1 measures.

use std::time::Instant;

use rbx_dom_weak::types::{Ref, Variant};
use rbx_dom_weak::{ustr, InstanceBuilder, WeakDom};
use rustudio_core::doc::{default_instance_name, Document};
use serde_json::Value as J;

use crate::snapshot::{self, Node, Snapshot, ValueConverter};

/// A client world: the DataModel and the bridge's own bookkeeping.
///
/// Not `Debug`: `Document` is not `Debug`, and a world is inspected through
/// `report`/`stats_json`, which are the strings the harness prints.
pub struct World {
    pub doc: Document,
    pub dom: WeakDom,
    /// rove id → this world's `Ref`.
    pub ids: std::collections::HashMap<String, Ref>,
    /// This world's `Ref` → rove id (the inverse, for remotes and physics).
    pub refs: std::collections::HashMap<Ref, String>,
    /// `Players.LocalPlayer`, once the snapshot's `local_player` resolved.
    pub local_player: Option<Ref>,
    /// What the build did: the numbers the harness prints.
    pub stats: Stats,
}

/// The build's own measurements. Every one is printed by the harness
/// (`--live-snapshot`) and written into `--result`, so a wrong world shows up
/// as a number rather than as a screenshot nobody can diff.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub nodes: usize,
    pub instances: usize,
    /// Root children, by class, in the order they were added.
    pub root_classes: Vec<String>,
    /// Nodes whose parent had not been attached yet when they were reached.
    pub out_of_order: usize,
    /// Nodes whose parent never arrived (kept under the root).
    pub orphans: usize,
    /// Property values with no tag: the property's own type decided them.
    pub scalar_values: usize,
    /// Property values that went through a `$` tag.
    pub tagged_values: usize,
    /// `$ref` properties dropped because the target is not in the world.
    pub dangling_refs: usize,
    /// `$ref` properties honoured (the mirror image of `dangling_refs`).
    pub refs_bound: usize,
    /// Property values the converter refused (each also in `refused`).
    pub refused_values: usize,
    /// The nodes `local_player` and the player's `Character` resolved to.
    pub local_player_nodes: usize,
    pub duration_ms: f64,
}

/// A built world and the one-paragraph report the harness prints.
pub struct Built {
    pub world: World,
    pub report: String,
}

/// Builds the DataModel from a parsed snapshot.
pub fn build(snapshot: Snapshot, title: String) -> Result<Built, String> {
    let started = Instant::now();
    let mut stats = Stats { nodes: snapshot.nodes.len(), ..Stats::default() };
    let mut converter = ValueConverter::default();

    // The DataModel root, as Play's client copy has it.
    let mut dom = WeakDom::new(InstanceBuilder::new("DataModel").with_name("Game"));
    let root_ref = dom.root_ref();

    // Every instance exists before anything is converted, because a property
    // may name any other node.
    let mut ids: std::collections::HashMap<String, Ref> =
        std::collections::HashMap::with_capacity(snapshot.nodes.len());
    let mut refs: std::collections::HashMap<Ref, String> =
        std::collections::HashMap::with_capacity(snapshot.nodes.len());
    for node in &snapshot.nodes {
        let name = match &node.name {
            Some(name) => name.clone(),
            None => default_instance_name(&node.class_name).to_string(),
        };
        let builder = InstanceBuilder::new(node.class_name.as_str()).with_name(name);
        let r = dom.insert(root_ref, builder);
        ids.insert(node.id.clone(), r);
        refs.insert(r, node.id.clone());
    }

    // Properties, now that every Ref exists.
    for node in &snapshot.nodes {
        let r = ids[&node.id];
        let Some(object) = node.properties.as_object() else { continue };
        let mut values: Vec<(String, Variant)> = Vec::with_capacity(object.len());
        for (property, value) in object {
            // `$ref` is the bridge's, not the converter's: it needs the map.
            if let Some(target) = value.get("$ref").and_then(J::as_str) {
                match ids.get(target) {
                    Some(&target_ref) => {
                        values.push((property.clone(), Variant::Ref(target_ref)));
                        stats.refs_bound += 1;
                    }
                    None => {
                        stats.dangling_refs += 1;
                        let detail = format!(
                            "{}.{property} points at {target}, which the snapshot does not have",
                            node.class_name
                        );
                        converter.refused.push(detail.clone());
                        log::warn!("rustudio-live: {detail} — the property keeps its default");
                    }
                }
                continue;
            }
            match snapshot::convert_property(&mut converter, &node.class_name, property, value) {
                Ok(v) => values.push((property.clone(), v)),
                Err(e) => {
                    stats.refused_values += 1;
                    log::warn!("rustudio-live: {e} — the property keeps its default");
                }
            }
        }
        if let Some(instance) = dom.get_by_ref_mut(r) {
            for (property, value) in values {
                instance.properties.insert(ustr(&property), value);
            }
        }
    }

    // Parenting: parent-first where the snapshot allows it, deferred otherwise.
    // `attached` holds ids that have a place in the tree; `deferred` counts a
    // node the first time it was reached before its parent.
    let mut attached: std::collections::HashSet<&str> = std::collections::HashSet::with_capacity(snapshot.nodes.len());
    let mut deferred: std::collections::HashSet<&str> = Default::default();
    let mut missing_parent: Vec<&Node> = Vec::new();
    let mut progress = true;
    while progress {
        progress = false;
        for node in &snapshot.nodes {
            if attached.contains(node.id.as_str()) {
                continue;
            }
            let target = match &node.parent {
                None => root_ref,
                Some(parent) => match ids.get(parent) {
                    Some(&p) => {
                        if !attached.contains(parent.as_str()) {
                            deferred.insert(node.id.as_str());
                            continue;
                        }
                        p
                    }
                    None => {
                        // The parent is not in the world at all: the root is
                        // where it can still be seen and reported.
                        missing_parent.push(node);
                        attached.insert(node.id.as_str());
                        continue;
                    }
                },
            };
            dom.transfer_within(ids[&node.id], target);
            attached.insert(node.id.as_str());
            progress = true;
        }
    }
    stats.out_of_order = deferred.len();
    stats.orphans = missing_parent.len();

    let mut doc = Document::from_play_dom(dom.clone(), title);
    doc.revision += 1;
    // **Whose instances these are.** Every node in a snapshot came from the
    // server, and `Document::owner` needs to know that to tell a part the server
    // simulates from one *this* client's scripts made: without the stamp a
    // snapshot-built world says "everything here is local", so a client that
    // joined from a snapshot would integrate the whole map and delete what fell.
    // (`refs` is keyed by `Ref` and the `ids` map is the same pairing.)
    doc.replicated_peer.reserve(refs.len());
    for r in refs.keys() {
        doc.replicated_peer.insert(*r, crate::live::SERVER_PEER);
    }

    let local_player = snapshot.local_player().and_then(|id| ids.get(id).copied());
    if let Some(player) = local_player {
        stats.local_player_nodes = 1;
        // Players.LocalPlayer is a property of Players, and Play's client copy
        // has it: the camera, the GUI and M4's character binding read it there.
        if let Some(players) = dom.get_by_ref(player).map(|i| i.parent()) {
            if let Some(instance) = dom.get_by_ref_mut(players) {
                // A snapshot usually carries this ref itself (rove's does), so
                // binding it again must not count a third ref: `refs_bound` is
                // how many distinct properties point somewhere, and the test's
                // "LocalPlayer and Player.Character" is two.
                let bound_already = matches!(
                    instance.properties.get(&ustr("LocalPlayer")),
                    Some(Variant::Ref(r)) if *r == player
                );
                instance.properties.insert(ustr("LocalPlayer"), Variant::Ref(player));
                if !bound_already {
                    stats.refs_bound += 1;
                }
            }
        }
    }

    stats.instances = dom.descendants().count();
    stats.root_classes = dom
        .root()
        .children()
        .iter()
        .filter_map(|c| dom.get_by_ref(*c).map(|i| i.class.to_string()))
        .collect();
    stats.scalar_values = converter.scalar;
    stats.tagged_values = converter.converted;
    stats.duration_ms = started.elapsed().as_secs_f64() * 1000.0;

    // (a field-by-field construction: `stats` is measured, not copied)
    let world = World { doc, dom, ids, refs, local_player, stats };
    let report = world.report(&converter);
    Ok(Built { world, report })
}

impl World {
    /// The one paragraph the harness prints. `--live-snapshot` writes it to
    /// the log and into `--result`; CI greps it, a person reads it.
    pub fn report(&self, converter: &ValueConverter) -> String {
        let s = &self.stats;
        let mut out = format!(
            "rustudio-live: {} nodes -> {} instances in {:.1} ms; root: {}",
            s.nodes,
            s.instances,
            s.duration_ms,
            s.root_classes.join(", "),
        );
        if s.out_of_order > 0 {
            out.push_str(&format!("; {} arrived before their parent", s.out_of_order));
        }
        if s.orphans > 0 {
            out.push_str(&format!(
                "; {} instances had no parent in the snapshot and stay at the root",
                s.orphans
            ));
        }
        if s.dangling_refs > 0 {
            out.push_str(&format!("; {} property pointed at a node the world lacks", s.dangling_refs));
        }
        if s.refused_values > 0 {
            out.push_str(&format!("; {} value(s) kept their default", s.refused_values));
        }
        out.push_str(&format!(
            "; values: {} tagged, {} scalar; refs bound: {}",
            s.tagged_values, s.scalar_values, s.refs_bound
        ));
        if let Some(player) = self.local_player {
            let name = self.dom.get_by_ref(player).map(|i| i.name.clone()).unwrap_or_default();
            out.push_str(&format!("; LocalPlayer: {name}"));
        }
        if !converter.refused.is_empty() {
            out.push_str(&format!("; refused: {}", converter.refused.join(" | ")));
        }
        out
    }

    /// The same numbers, keyed for `--result`.
    pub fn stats_json(&self) -> J {
        let s = &self.stats;
        serde_json::json!({
            "nodes": s.nodes,
            "instances": s.instances,
            "root_classes": s.root_classes,
            "out_of_order": s.out_of_order,
            "orphans": s.orphans,
            "dangling_refs": s.dangling_refs,
            "refused_values": s.refused_values,
            "refs_bound": s.refs_bound,
            "tagged_values": s.tagged_values,
            "scalar_values": s.scalar_values,
            "local_player_nodes": s.local_player_nodes,
            "duration_ms": s.duration_ms,
            "local_player": self.local_player.map(|r| format!("{r:?}")),
        })
    }

    /// A stored property as the document holds it — not the class default
    /// `Document::prop` would answer with when the write was skipped.
    pub fn stored(&self, id: &str, property: &str) -> Option<Variant> {
        let r = *self.ids.get(id)?;
        self.dom.get_by_ref(r)?.properties.get(&ustr(property)).cloned()
    }

    /// A node's parent, as stored.
    pub fn parent_of(&self, id: &str) -> Option<Ref> {
        self.dom.get_by_ref(*self.ids.get(id)?).map(|i| i.parent())
    }
}

/// A snapshot read from a file, built: the harness's one call.
pub fn load(path: &std::path::Path, title: String) -> Result<Built, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
    let snapshot = snapshot::parse(&text)?;
    build(snapshot, title)
}
