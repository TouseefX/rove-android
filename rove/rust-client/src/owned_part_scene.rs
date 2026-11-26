//! Small physics view of the received scene. Does not retain a world snapshot.
use super::{owned_part_dynamics::Servo, physics_properties as received};
use rove_client_core::{
    physics::{received_rotation, OrientedBox, OrientedSurface},
    world::WorldView,
};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::OnceLock,
};

fn constructors() -> &'static Value {
    static CATALOG: OnceLock<Value> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!(
            "../../runtime/owned-physics-constructors-745.json"
        ))
        .expect("pinned owned physics constructors")
    })
}

pub(crate) fn field(view: &WorldView, id: &str, name: &str) -> Option<Value> {
    if let Some(v) = view.get_property(id, name).or_else(|| match name {
        "Size" => view.get_property(id, "size"),
        "Shape" => view.get_property(id, "shape"),
        _ => None,
    }) {
        return Some(v);
    }
    if let Some(v) = received::property(view, id, name) {
        return Some(v);
    }
    if !received::constructor_known(view, id) {
        return None;
    }
    let class = view.class_name(id)?;
    // Non-scriptable constructor fields also used by the Python adapter.
    if class == "Part" && name == "Shape" {
        return Some(json!(1));
    }
    if class == "Workspace" && name == "TouchesUseCollisionGroups" {
        return Some(json!(false));
    }
    constructors()["classes"][class]["properties"][name]
        .get("value")
        .cloned()
}
pub(crate) fn workspace(view: &WorldView, id: &str) -> Option<String> {
    let mut id = id;
    for _ in 0..256 {
        if view.class_name(id) == Some("Workspace") {
            return Some(id.into());
        }
        id = view.parent(id)?;
    }
    None
}
pub(crate) fn size(view: &WorldView, id: &str) -> Option<[f64; 3]> {
    received::vector(field(view, id, "Size")).filter(|v| v.iter().all(|v| *v > 0. && *v < 1e9))
}
pub(crate) fn is_box(view: &WorldView, id: &str) -> bool {
    view.class_name(id) == Some("Part")
        && field(view, id, "Shape").and_then(|v| v.as_i64()) == Some(1)
}
fn bounds(view: &WorldView, id: &str) -> Option<OrientedBox> {
    let frame = received::frame(view, id)?;
    OrientedBox::new(
        received::vector(frame.get("position").cloned())?,
        size(view, id)?.map(|v| v / 2.),
        received_rotation(&frame)?,
    )
    .ok()
}
fn default_group(view: &WorldView, id: &str) -> bool {
    if let Some(v) = view.get_property(id, "CollisionGroup") {
        return bytes(&v).is_some_and(|v| v == b"Default");
    }
    if let Some(v) = view.get_property(id, "CollisionGroupReplicate") {
        return bytes(&v).is_some_and(|v| v.len() == 1 || (v.len() > 1 && &v[1..] == b"Default"));
    }
    if let Some(v) = view.get_property(id, "CollisionGroupId") {
        return v.as_i64() == Some(0);
    }
    received::constructor_known(view, id)
}
fn bytes(v: &Value) -> Option<Vec<u8>> {
    if let Some(s) = v.as_str().filter(|s| s.len() <= 4096) {
        return Some(s.as_bytes().to_vec());
    }
    let s = v
        .get("$bytes_hex")?
        .as_str()
        .filter(|s| s.len() <= 8192 && s.len() % 2 == 0)?;
    s.as_bytes()
        .chunks_exact(2)
        .map(|c| Some(((c[0] as char).to_digit(16)? * 16 + (c[1] as char).to_digit(16)?) as u8))
        .collect()
}
// The isolated model currently handles Default against Default. Respect its
// actual mask; unfamiliar groups remain unsupported instead of assuming yes.
fn default_collidable(view: &WorldView, root: &str, service: Option<&str>) -> Option<bool> {
    let mut rows = Vec::new();
    if let Some(v) = view.get_property(root, "CollisionGroupData") {
        let raw = bytes(&v)?;
        if raw.is_empty() {
            return Some(true);
        }
        if raw.len() < 2 || raw[0] != 1 || !(1..=32).contains(&raw[1]) {
            return None;
        }
        let mut at = 2;
        for _ in 0..raw[1] {
            let header = raw.get(at..at + 7)?;
            if header[1] != 4 {
                return None;
            }
            let (id, mask, len) = (
                header[0],
                u32::from_le_bytes(header[2..6].try_into().ok()?),
                header[6] as usize,
            );
            at += 7;
            let name = std::str::from_utf8(raw.get(at..at + len)?).ok()?.to_owned();
            at += len;
            rows.push((id, name, mask));
        }
        if at != raw.len() {
            return None;
        }
    } else if let Some(v) = view
        .get_property(root, "CollisionGroups")
        .or_else(|| service.and_then(|id| view.get_property(id, "CollisionGroups")))
    {
        let raw = bytes(&v)?;
        let text = std::str::from_utf8(&raw).ok()?;
        if text.is_empty() {
            return Some(true);
        }
        for row in text.split('\\') {
            let f = row.split('^').collect::<Vec<_>>();
            if f.len() != 3 {
                return None;
            }
            let mask = f[2].parse::<i64>().ok()?;
            if !(-(1_i64 << 31)..(1_i64 << 32)).contains(&mask) {
                return None;
            }
            rows.push((f[1].parse::<u8>().ok()?, f[0].to_owned(), mask as u32));
        }
    } else {
        return received::constructor_known(view, root).then_some(true);
    }
    if rows.is_empty() || rows.len() > 32 {
        return None;
    }
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    let mut result = None;
    for (id, name, mask) in rows {
        if id >= 32
            || name.is_empty()
            || name.len() > 100
            || name.contains('\0')
            || !ids.insert(id)
            || !names.insert(name.clone())
            || (id == 0) != (name == "Default")
        {
            return None;
        }
        if id == 0 {
            result = Some(mask & 1 != 0);
        }
    }
    result
}
fn isa<'a>(mut class: &'a str, target: &str, parents: &'a HashMap<String, String>) -> bool {
    for _ in 0..128 {
        if class == target {
            return true;
        }
        let Some(p) = parents.get(class) else {
            return false;
        };
        class = p;
    }
    false
}
pub(crate) struct Item {
    pub id: String,
    pub workspace: String,
    pub bounds: Option<OrientedBox>,
    pub supported: bool,
    pub anchored: bool,
}
#[derive(Default)]
pub(crate) struct Scene {
    pub colliders: Vec<Item>,
    pub listeners: Vec<Item>,
    pub forces: HashMap<String, Vec<String>>,
    pub unsupported_forces: HashSet<String>,
    pub overflow: bool,
    default_masks: HashMap<String, Option<bool>>,
}
impl Scene {
    // Pose receipts do not change membership or collision policy. Refresh the
    // existing entries from this revision instead of rescanning the world.
    pub fn refresh_poses(&mut self, view: &WorldView, changed: &HashSet<String>) {
        for item in self.colliders.iter_mut().chain(self.listeners.iter_mut()) {
            if changed.contains(&item.id) {
                item.bounds = bounds(view, &item.id);
            }
        }
    }

    pub fn build(view: &WorldView, parents: &HashMap<String, String>) -> Self {
        let mut out = Self::default();
        let mut listeners = HashSet::new();
        let mut service = None;
        let mut roots = Vec::new();
        view.for_each_node(|n| {
            if n.class_name() == "PhysicsService" {
                service = Some(n.id().to_owned());
            }
            if n.class_name() == "Workspace" {
                roots.push(n.id().to_owned());
            }
            if n.class_name() == "NoCollisionConstraint"
                && n.property("Enabled") != Some(json!(false))
            {
                for key in ["Part0", "Part1"] {
                    if let Some(id) = n
                        .property(key)
                        .and_then(|v| v["$ref"].as_str().map(str::to_owned))
                    {
                        out.unsupported_forces.insert(id);
                    }
                }
            }
            if n.class_name() == "TouchTransmitter" {
                if let Some(p) = view.parent(n.id()) {
                    listeners.insert(p.to_owned());
                }
            }
            if isa(n.class_name(), "BodyMover", parents)
                || isa(n.class_name(), "Constraint", parents)
            {
                if let Some(p) = view.parent(n.id()) {
                    if n.class_name() == "BodyVelocity" {
                        out.forces.entry(p.into()).or_default().push(n.id().into());
                    } else {
                        out.unsupported_forces.insert(p.into());
                    }
                }
            }
        });
        for root in roots {
            let collidable = default_collidable(view, &root, service.as_deref());
            out.default_masks.insert(root, collidable);
        }
        if listeners.len() > 4096 {
            out.overflow = true;
            return out;
        }
        view.for_each_node(|n| {
            if !isa(n.class_name(), "BasePart", parents) {
                return;
            }
            let Some(root) = workspace(view, n.id()) else {
                return;
            };
            let anchored = field(view, n.id(), "Anchored") == Some(json!(true));
            let bounds = bounds(view, n.id());
            let supported = is_box(view, n.id());
            if listeners.contains(n.id())
                && anchored
                && field(view, n.id(), "CanTouch") == Some(json!(true))
            {
                out.listeners.push(Item {
                    id: n.id().into(),
                    workspace: root.clone(),
                    bounds,
                    supported,
                    anchored,
                });
            }
            if field(view, n.id(), "CanCollide") == Some(json!(true)) {
                if out.colliders.len() >= 65536 {
                    out.overflow = true;
                    return;
                }
                out.colliders.push(Item {
                    id: n.id().into(),
                    workspace: root,
                    bounds,
                    supported: supported && default_group(view, n.id()),
                    anchored,
                });
            }
        });
        out
    }
    pub fn motion_geometry(
        &self,
        view: &WorldView,
        id: &str,
        root: &str,
        p: [f64; 3],
        v: [f64; 3],
        size: [f64; 3],
        dt: f64,
        gravity: f64,
    ) -> Result<Vec<OrientedSurface>, &'static str> {
        if self.overflow {
            return Err("scene_geometry_budget");
        }
        if field(view, id, "CanCollide") == Some(json!(false)) {
            return Ok(Vec::new());
        }
        if !default_group(view, id) {
            return Err("collision_group_unsupported");
        }
        let defaults_collide = self
            .default_masks
            .get(root)
            .copied()
            .flatten()
            .ok_or("collision_matrix_unavailable")?;
        let radius = size.into_iter().fold(0., f64::max) * 0.5
            + size.into_iter().fold(f64::INFINITY, f64::min).max(1.);
        let end = std::array::from_fn::<_, 3, _>(|i| {
            p[i] + v[i] * dt - if i == 1 { 0.5 * gravity * dt * dt } else { 0. }
        });
        let mut output = Vec::new();
        for item in &self.colliders {
            if item.id == id || item.workspace != root {
                continue;
            }
            let b = item.bounds.ok_or("collision_geometry_unavailable")?;
            let extent: [f64; 3] = std::array::from_fn(|i| {
                (0..3)
                    .map(|j| b.rotation[i * 3 + j].abs() * b.half_size[j])
                    .sum()
            });
            if (0..3).any(|i| {
                b.center[i] + extent[i] < p[i].min(end[i]) - radius
                    || b.center[i] - extent[i] > p[i].max(end[i]) + radius
            }) {
                continue;
            }
            if !item.anchored || !item.supported {
                return Err("nearby_collision_geometry_unsupported");
            }
            if !defaults_collide {
                continue;
            }
            output.push(OrientedSurface {
                node_id: item.id.clone(),
                bounds: b,
            });
        }
        Ok(output)
    }
    pub fn servo(
        &self,
        view: &WorldView,
        id: &str,
        size: [f64; 3],
    ) -> Result<Option<Servo>, &'static str> {
        if self.unsupported_forces.contains(id) {
            return Err("force_class_unsupported");
        }
        let Some(forces) = self.forces.get(id) else {
            return Ok(None);
        };
        if forces.len() != 1 {
            return Err("multiple_velocity_servos_unsupported");
        }
        let force = &forces[0];
        let scalar = |name| {
            field(view, force, name)
                .and_then(|v| v.as_f64())
                .ok_or("velocity_servo_property_unavailable")
        };
        let target = received::vector(field(view, force, "Velocity"))
            .ok_or("velocity_servo_property_unavailable")?;
        let max_force = received::vector(field(view, force, "MaxForce"))
            .ok_or("velocity_servo_property_unavailable")?;
        // Same mass admission as Python: a received mass, or a complete
        // default-material block's measured constructor mass scaled by volume.
        let mass = if let Some(mass) = view
            .get_property(id, "Mass")
            .and_then(|v| v.as_f64())
            .filter(|v| v.is_finite() && *v > 0.)
        {
            mass
        } else {
            if !received::constructor_known(view, id)
                || view.get_property(id, "Material").is_some()
                || view.get_property(id, "MaterialVariant").is_some()
                || field(view, id, "CustomPhysicalProperties") != Some(Value::Null)
            {
                return Err("body_velocity_mass_unavailable");
            }
            let p = &constructors()["classes"]["Part"]["properties"];
            let mass = p["Mass"]["value"]
                .as_f64()
                .ok_or("constructor_mass_unavailable")?;
            let dimensions = received::vector(Some(p["Size"]["value"].clone()))
                .ok_or("constructor_mass_unavailable")?;
            mass * size.into_iter().product::<f64>() / dimensions.into_iter().product::<f64>()
        };
        Ok(Some(Servo {
            target,
            max_force,
            gain: scalar("P")?,
            mass,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rove_client_core::world::WorldStore;
    #[test]
    fn owned_scene_constructor_subset_matches_its_pinned_source() {
        let source: Value =
            serde_json::from_str(include_str!("../../runtime/class-constructors-745.json"))
                .unwrap();
        for (class, entry) in constructors()["classes"].as_object().unwrap() {
            for (name, property) in entry["properties"].as_object().unwrap() {
                assert_eq!(property, &source["classes"][class]["properties"][name]);
            }
        }
    }
    #[test]
    fn owned_scene_default_collision_mask_is_received_not_assumed() {
        for (property, value, expected) in [
            ("CollisionGroups", json!("Default^0^-1"), Some(true)),
            ("CollisionGroups", json!("Default^0^0"), Some(false)),
            ("CollisionGroups", json!("Unknown^0^-1"), None),
            (
                "CollisionGroupData",
                json!({"$bytes_hex":"01010004000000000744656661756c74"}),
                Some(false),
            ),
            (
                "CollisionGroupData",
                json!({"$bytes_hex":"01010004ffffffff0744656661756c74"}),
                Some(true),
            ),
            ("CollisionGroupData", json!({"$bytes_hex":"01010004"}), None),
        ] {
            let store = WorldStore::from_snapshot(&json!({"version":1,"nodes":[
                {"id":"root","class_name":"Workspace","properties":{property:value}}
            ]}))
            .unwrap();
            assert_eq!(
                default_collidable(&store.view(), "root", None),
                expected,
                "{property}: {value}"
            );
        }
    }
    #[test]
    fn owned_scene_defaults_require_a_complete_creation_and_keep_aliases() {
        let store=WorldStore::from_snapshot(&json!({"version":1,"nodes":[
            {"id":"partial","class_name":"Part","properties":{}},
            {"id":"full","class_name":"Part","received_creation_complete":true,"properties":{"size":[8,2,4],"CanTouch":false}}
        ]})).unwrap();
        let view = store.view();
        assert_eq!(field(&view, "partial", "CanTouch"), None);
        assert_eq!(field(&view, "full", "CanTouch"), Some(json!(false)));
        assert_eq!(field(&view, "full", "Shape"), Some(json!(1)));
        assert_eq!(size(&view, "full"), Some([8., 2., 4.]));
    }
}
