//! Authored transforms for received, client-owned, isolated parts.
//! This does not simulate assemblies or manufacture a server acknowledgment.
use crate::physics_properties as received;
use rove_client_core::{session::SessionRecord, world::WorldView};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

pub(crate) const LIMIT: usize = 64;

#[derive(Clone, Debug)]
pub(crate) struct PartFrame {
    pub id: u64,
    pub position: [f64; 3],
    pub rotation: [f64; 9],
    pub linear: [f64; 3],
    pub angular: [f64; 3],
}

#[derive(Default)]
pub(crate) struct OwnedPartPoses {
    pub python_submitted_model: bool,
    pending: BTreeMap<String, Value>,
    topology_ready: bool,
    characters: HashSet<String>,
    connected: HashSet<String>,
    accepted: u64,
    prepared: u64,
    coalesced: u64,
    rejected: BTreeMap<&'static str, u64>,
    velocity_sources: BTreeMap<&'static str, u64>,
    simulation: simulation::Simulation,
}

fn reference(value: Option<Value>) -> Option<String> {
    value?.get("$ref")?.as_str().map(str::to_owned)
}
fn vector<const N: usize>(value: &Value) -> Option<[f64; N]> {
    let values = value.get("$vector3").unwrap_or(value).as_array()?;
    if values.len() != N {
        return None;
    }
    let mut out = [0.; N];
    for (dst, src) in out.iter_mut().zip(values) {
        *dst = src.as_f64().filter(|v| v.is_finite() && v.abs() < 1e9)?;
    }
    Some(out)
}
fn owner(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| {
            value
                .as_f64()
                .filter(|n| n.is_finite() && *n > 0. && n.fract() == 0. && *n < u32::MAX as f64)
                .map(|n| n as u64)
        })
        .filter(|n| *n > 0 && *n < u32::MAX as u64)
}
fn is_a(class: &str, target: &str, parents: &HashMap<String, String>) -> bool {
    let mut current = class;
    for _ in 0..128 {
        if current == target {
            return true;
        }
        let Some(parent) = parents.get(current) else {
            return false;
        };
        current = parent;
    }
    false
}

impl OwnedPartPoses {
    pub fn observe_replication(&mut self, record: &SessionRecord) {
        self.simulation.observe_replication(record);
        if record
            .world_delta
            .lifecycle_actions()
            .is_some_and(|actions| {
                actions.iter().any(|a| {
                    matches!(a["kind"].as_str(), Some("create" | "remove"))
                        || [
                            "Character",
                            "Part0",
                            "Part1",
                            "Attachment0",
                            "Attachment1",
                            "Parent",
                        ]
                        .iter()
                        .any(|name| a["fields"].get(name).is_some())
                })
            })
        {
            self.topology_ready = false;
        }
    }

    fn topology(&mut self, view: &WorldView, parents: &HashMap<String, String>) {
        if self.topology_ready {
            return;
        }
        self.characters.clear();
        self.connected.clear();
        view.for_each_node(|node| {
            if node.class_name() == "Player" {
                if let Some(id) = reference(node.property("Character")) {
                    self.characters.insert(id);
                }
            }
            if is_a(node.class_name(), "JointInstance", parents)
                || node.class_name() == "WeldConstraint"
            {
                for name in ["Part0", "Part1"] {
                    if let Some(id) = reference(node.property(name)) {
                        self.connected.insert(id);
                    }
                }
            }
            // Constraints can also make this a multi-body assembly. Until that
            // solver exists, treat either endpoint as connected even if disabled.
            if is_a(node.class_name(), "Constraint", parents) {
                for name in ["Attachment0", "Attachment1"] {
                    if let Some(part) = reference(node.property(name))
                        .as_deref()
                        .and_then(|id| view.parent(id))
                    {
                        self.connected.insert(part.to_owned());
                    }
                }
            }
        });
        self.topology_ready = true;
    }

    pub fn observe(&mut self, intents: &[Value]) {
        for intent in intents.iter().filter(|v| v["kind"] == "OwnedPartStop") {
            if let Some(id) = intent["instance_id"].as_str() {
                self.pending.remove(id);
                self.simulation.discard(id);
            }
        }
        for intent in intents.iter().filter(|v| v["kind"] == "OwnedPartCFrame") {
            let Some(id) = intent["instance_id"].as_str() else {
                self.reject("invalid_part_intent");
                continue;
            };
            if !self.pending.contains_key(id) && self.pending.len() >= LIMIT {
                self.reject("part_pose_queue_full");
                continue;
            }
            self.coalesced +=
                u64::from(self.pending.insert(id.to_owned(), intent.clone()).is_some());
            self.accepted += 1;
        }
    }

    pub fn prepare(
        &mut self,
        view: &WorldView,
        parents: &HashMap<String, String>,
        scope: u64,
        legacy_pv_enabled: bool,
    ) -> Vec<PartFrame> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        self.topology(view, parents);
        let pending = std::mem::take(&mut self.pending);
        let mut frames = Vec::new();
        for (id, intent) in pending {
            let result = if legacy_pv_enabled {
                self.frame(view, parents, scope, &id, &intent)
            } else {
                Err("negotiated_physics_format_unsupported")
            };
            match result {
                Ok((frame, source)) => {
                    self.prepared += 1;
                    *self.velocity_sources.entry(source).or_default() += 1;
                    frames.push(frame);
                }
                Err(reason) => self.reject(reason),
            }
        }
        frames
    }

    fn frame(
        &self,
        view: &WorldView,
        parents: &HashMap<String, String>,
        scope: u64,
        id: &str,
        intent: &Value,
    ) -> Result<(PartFrame, &'static str), &'static str> {
        let node = view.read_only_node(id).ok_or("part_removed")?;
        if !is_a(node.class_name(), "BasePart", parents) || node.class_name() == "Terrain" {
            return Err("not_movable_basepart");
        }
        let revision = intent["received_sequence"]
            .as_u64()
            .ok_or("missing_receipt_sequence")?;
        if revision > view.sequence()
            || node.created_sequence() != intent["created_sequence"].as_u64()
            || node.created_sequence().is_none()
        {
            return Err("part_generation_changed");
        }
        if owner(&intent["received_owner"]) != Some(scope)
            || node.property("NetworkOwnerV3").as_ref().and_then(owner) != Some(scope)
        {
            return Err("ownership_not_current");
        }
        if received::property(view, id, "Anchored") != Some(json!(false)) {
            return Err("part_not_known_unanchored");
        }
        if self.connected.contains(id)
            || [
                "PhysicsRepRootPart",
                "PhysicsRepRootRef",
                "AssemblyRootPart",
            ]
            .iter()
            .any(|name| {
                node.property(name)
                    .is_some_and(|v| !v.is_null() && v["$ref"].as_str() != Some(id))
            })
        {
            return Err("connected_assembly_unsupported");
        }
        if [
            "CFrame",
            "ReplicationPV",
            "Position",
            "Orientation",
            "Rotation",
            "Parent",
            "NetworkOwnerV3",
            "Anchored",
            "PhysicsRepRootPart",
            "PhysicsRepRootRef",
            "AssemblyRootPart",
        ]
        .iter()
        .any(|name| {
            node.metadata_field("property_revisions", name)
                .and_then(|v| v.as_u64())
                .is_some_and(|n| n > revision)
        }) {
            return Err("newer_received_state");
        }
        let mut cursor = id;
        let mut in_workspace = false;
        for _ in 0..256 {
            if self.characters.contains(cursor) {
                return Err("character_pose_excluded");
            }
            if view.class_name(cursor) == Some("Workspace") {
                in_workspace = true;
                break;
            }
            cursor = view.parent(cursor).ok_or("outside_received_workspace")?;
        }
        if !in_workspace {
            return Err("outside_received_workspace");
        }
        let raw = &intent["cframe"]["$cframe"];
        let position = vector(&raw["position"]).ok_or("invalid_part_pose")?;
        let rotation = vector(&raw["rotation"])
            .filter(|v| crate::physics_sender::validate_rotation(*v))
            .ok_or("invalid_part_pose")?;
        let (scope_word, index) = id.split_once(':').ok_or("invalid_part_reference")?;
        let scope_word = scope_word
            .parse::<u32>()
            .map_err(|_| "invalid_part_reference")?;
        let index = index.parse::<u32>().map_err(|_| "invalid_part_reference")?;
        if scope_word == 0 || scope_word == u32::MAX {
            return Err("invalid_part_reference");
        }
        let (linear, angular, source) = velocities(view, id)?;
        Ok((
            PartFrame {
                id: (scope_word as u64) << 32 | index as u64,
                position,
                rotation,
                linear,
                angular,
            },
            source,
        ))
    }

    pub(crate) fn reject(&mut self, reason: &'static str) {
        *self.rejected.entry(reason).or_default() += 1;
    }
    pub fn advance(
        &mut self,
        view: &WorldView,
        parents: &HashMap<String, String>,
        scope: u64,
        legacy: bool,
        now: f64,
    ) -> simulation::Step {
        self.topology(view, parents);
        let mut simulation = std::mem::take(&mut self.simulation);
        let result = if self.python_submitted_model {
            simulation.advance_submitted(self, view, parents, scope, legacy)
        } else {
            simulation.advance(self, view, parents, scope, legacy, now)
        };
        self.simulation = simulation;
        result
    }
    pub fn report(&self) -> Value {
        json!({"authored_pose_writes":self.accepted,"records_prepared":self.prepared,
            "pending_parts":self.pending.len(),"coalesced":self.coalesced,"rejected":self.rejected,
            "velocity_sources":self.velocity_sources,"local_dynamics":self.simulation.report(self.python_submitted_model)})
    }
}

fn compact_velocity(value: &Value) -> Option<[f64; 3]> {
    if value.is_null() {
        return Some([0.; 3]);
    }
    let pair = value.as_array().filter(|v| v.len() == 2)?;
    let header = u8::try_from(pair[0].as_u64()?).ok()?;
    let word = u32::try_from(pair[1].as_u64()?).ok()?;
    if header == 0 {
        return (word == 0).then_some([0.; 3]);
    }
    let exponent = i32::from(header >> 4) - 1;
    if !(0..=14).contains(&exponent) {
        return None;
    }
    let signed = |v: u32| {
        if v & 0x800 != 0 {
            v as i32 - 4096
        } else {
            v as i32
        }
    };
    let q = [
        signed(word >> 20),
        signed((word >> 8) & 4095),
        signed(((word & 255) << 4) | u32::from(header & 15)),
    ];
    if q.contains(&-2048) {
        return None;
    }
    Some(q.map(|v| v as f64 * 2_f64.powi(exponent) / 2047.))
}

fn velocities(
    view: &WorldView,
    id: &str,
) -> Result<([f64; 3], [f64; 3], &'static str), &'static str> {
    if let Some(pv) = view.get_property(id, "ReplicationPV") {
        let pv = pv.get("$cframe").unwrap_or(&pv);
        if let Some(v) = pv
            .get("decoded_velocities")
            .and_then(Value::as_array)
            .filter(|v| v.len() == 2)
        {
            return Ok((
                vector(&v[0]).ok_or("received_velocity_invalid")?,
                vector(&v[1]).ok_or("received_velocity_invalid")?,
                "received_replication_pv",
            ));
        }
        if pv.get("velocity_mode").is_some_and(|v| v != "compact") {
            return Err("received_velocity_format_unsupported");
        }
        let v = pv["packed_velocities"]
            .as_array()
            .filter(|v| v.len() == 2)
            .ok_or("received_velocity_unavailable")?;
        return Ok((
            compact_velocity(&v[0]).ok_or("received_velocity_invalid")?,
            compact_velocity(&v[1]).ok_or("received_velocity_invalid")?,
            "received_replication_pv",
        ));
    }
    if !received::constructor_known(view, id) {
        return Err("received_velocity_unavailable");
    }
    static INITIAL: OnceLock<Value> = OnceLock::new();
    let catalog = INITIAL.get_or_init(|| {
        serde_json::from_str(include_str!(
            "../../runtime/physics-initial-velocities-745.json"
        ))
        .expect("pinned initial velocities")
    });
    let v = &catalog[view.class_name(id).ok_or("part_removed")?];
    Ok((
        vector(&v[0]).ok_or("constructor_velocity_unavailable")?,
        vector(&v[1]).ok_or("constructor_velocity_unavailable")?,
        "local_constructor_initial_state",
    ))
}

#[cfg(test)]
#[path = "owned_part_poses_tests.rs"]
mod tests;

#[path = "owned_part_simulation.rs"]
mod simulation;
