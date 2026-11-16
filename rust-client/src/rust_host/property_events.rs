//! Ordered property receipts, including aliases derived from received part frames.
use super::{
    world::{self, WorldRef},
    world_aliases,
};
use mlua::Lua;
use serde_json::Value as Json;
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
};

#[derive(Default)]
pub(super) struct Receipt<'a>(HashMap<usize, Vec<Change<'a>>>);
struct Change<'a> {
    id: String,
    key: String,
    value: Option<&'a Json>,
}
impl Receipt<'_> {
    pub(super) fn mass_ids(&self) -> std::collections::HashSet<&str> {
        self.0.values().flatten().filter(|c|c.key == "Mass").map(|c|c.id.as_str()).collect()
    }
    pub(super) fn publish(&mut self, lua: &Lua, ctx: &WorldRef, index: usize) -> mlua::Result<()> {
        for change in self.0.remove(&index).unwrap_or_default() {
            world::emit_property_changed_value(lua, ctx, &change.id, &change.key, change.value)?;
        }
        Ok(())
    }
}

/// Match the transaction: explicit property/key overrides the same fields key.
pub(super) fn fields(action: &Json) -> BTreeMap<&str, &Json> {
    let mut fields = action["fields"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, value)| (key.as_str(), value))
        .collect::<BTreeMap<_, _>>();
    if let (Some(key), Some(value)) = (
        action
            .get("property")
            .or_else(|| action.get("key"))
            .and_then(Json::as_str),
        action.get("value"),
    ) {
        fields.insert(key, value);
    }
    fields
}
pub(super) fn relevant(action: &Json) -> bool {
    matches!(action["kind"].as_str(), Some("set" | "set_property"))
        && (action["fields"].as_object().is_some_and(|f| !f.is_empty())
            || action
                .get("property")
                .or_else(|| action.get("key"))
                .is_some())
}
fn action_has_mass_input(action: &Json) -> bool {
    action["fields"]
        .as_object()
        .is_some_and(|fields| fields.keys().any(|key| super::geometry::part_mass_properties::is_input(key)))
        || action
            .get("property")
            .or_else(|| action.get("key"))
            .and_then(Json::as_str)
            .is_some_and(super::geometry::part_mass_properties::is_input)
}

fn observed_union(observed: &HashMap<String, HashSet<String>>) -> HashSet<&str> {
    observed
        .values()
        .flat_map(|ids| ids.iter().map(String::as_str))
        .collect()
}

fn is_basepart_class(class: &str, cache: &mut HashMap<String, bool>) -> bool {
    *cache.entry(class.to_owned()).or_insert_with(|| {
        class == "BasePart"
            || world::class_chain(class)
                .iter()
                .any(|ancestor| ancestor == "BasePart")
    })
}

fn initial(ctx: &WorldRef, id: &str, key: &str, created: Option<&Json>) -> Option<Json> {
    if let Some(node) = created {
        return match key {
            "Name" => node.get("name").or_else(|| node.get("class_name")).cloned(),
            _ => node["properties"].get(key).cloned(),
        };
    }
    world::get_property(ctx, id, key).ok()
}

fn public_alias(class: &str, key: &str) -> Option<&'static str> {
    world_aliases::canonical_key(class, key).or_else(|| {
        world_aliases::wire_key(class, key)
            .and_then(|wire| world_aliases::canonical_key(class, wire))
    })
}
fn stored(ctx: &WorldRef, id: &str, key: &str, created: Option<&Json>) -> Option<Json> {
    if let Some(node) = created {
        return node["properties"].get(key).cloned();
    }
    let world = ctx.borrow();
    let node = world.nodes.get(id)?;
    if let Some(projected) = &node.projected {
        projected.property(key).ok().flatten()
    } else {
        node.properties.get(key).cloned()
    }
}
struct Alias<'a> {
    // Match public reads: a local public override wins, followed by an explicit
    // stored public property, followed by the decoded received wire property.
    base: Option<Cow<'a, Json>>,
    current: Option<Cow<'a, Json>>,
    brick: Option<Json>,
    watch_brick: bool,
}
impl<'a> Alias<'a> {
    fn new(
        ctx: &WorldRef,
        id: &str,
        class: &str,
        public: &str,
        wire: &str,
        created: Option<&Json>,
        watch_brick: bool,
    ) -> Self {
        let base = stored(ctx, id, public, created);
        let current = if created.is_some() {
            base.clone().or_else(|| {
                stored(ctx, id, wire, created)
                    .and_then(|v| world_aliases::from_wire(class, wire, &v).ok().flatten())
            })
        } else {
            world::get_property(ctx, id, public).ok()
        };
        let brick = if watch_brick {
            current
                .as_ref()
                .and_then(|color| super::sequences::closest_brick_json(color).ok())
                .or_else(|| {
                    if created.is_some() {
                        stored(ctx, id, "BrickColor", created)
                    } else {
                        world::get_property(ctx, id, "BrickColor").ok()
                    }
                })
        } else {
            None
        };
        Self {
            base: base.map(Cow::Owned),
            current: current.map(Cow::Owned),
            brick,
            watch_brick,
        }
    }
    fn update(
        &mut self,
        class: &str,
        public: &str,
        wire: &str,
        fields: &BTreeMap<&str, &'a Json>,
        received: bool,
    ) -> bool {
        let explicit = fields.get(public).copied();
        let incoming = fields.get(wire).copied();
        if received {
            if let Some(value) = explicit {
                self.base = Some(Cow::Borrowed(value));
            }
        }
        let next = if received && incoming.is_some() {
            self.base.clone().or_else(|| {
                world_aliases::from_wire(class, wire, incoming.unwrap())
                    .ok()
                    .flatten()
                    .map(Cow::Owned)
            })
        } else if let Some(value) = explicit {
            Some(Cow::Borrowed(value))
        } else {
            // Local writes to hidden wire fields are not public overrides.
            return false;
        };
        let changed = match (&self.current, &next) {
            (Some(a), Some(b)) => !world::attribute_value_eq(a, b),
            (None, None) => false,
            _ => true,
        };
        if self.watch_brick && changed {
            self.brick = next
                .as_ref()
                .and_then(|color| super::sequences::closest_brick_json(color).ok());
        }
        self.current = next;
        changed
    }
}
// CFrame packet metadata is not part of the Luau value. Compare validated
// position/rotation components so an equal explicit frame after PV stays silent.
fn property_equal(key: &str, left: &Json, right: &Json) -> bool {
    if key == "CFrame" {
        if let (Ok(a), Ok(b)) = (
            super::movement::cframe_from_json(left),
            super::movement::cframe_from_json(right),
        ) {
            return a == b;
        }
    }
    world::attribute_value_eq(left, right)
}

fn physics_silent(action: &Json, key: &str) -> bool {
    let mask_key = match key {
        "Position" | "Orientation" | "Rotation" => "CFrame",
        "ReplicationPV"
        | "CFrame"
        | "AssemblyLinearVelocity"
        | "Velocity"
        | "AssemblyAngularVelocity"
        | "RotVelocity" => key,
        _ => return false,
    };
    let explicit = action
        .get("property")
        .or_else(|| action.get("key"))
        .and_then(Json::as_str);
    explicit != Some(mask_key)
        && action["_physics_fields"]
            .as_array()
            .is_some_and(|fields| fields.iter().any(|value| value.as_str() == Some(mask_key)))
}

pub(super) fn prepare<'a>(
    lua: &Lua,
    ctx: &WorldRef,
    actions: &'a [Json],
    previous: Option<u64>,
) -> Receipt<'a> {
    let fresh = |a: &Json| previous.is_none_or(|seq| a["sequence"].as_u64().unwrap_or(0) > seq);
    let mut receipt = Receipt::default();
    if !actions.iter().any(|a| fresh(a) && relevant(a)) {
        return receipt;
    }
    let has_colors = actions.iter().any(|action| {
        fresh(action)
            && relevant(action)
            && (action["fields"].get("Color").is_some()
                || action["fields"].get("Color3uint8").is_some()
                || matches!(
                    action
                        .get("property")
                        .or_else(|| action.get("key"))
                        .and_then(Json::as_str),
                    Some("Color" | "Color3uint8")
                ))
    });
    let observed = if has_colors {
        super::signals::observed_ids(lua, ctx, &["Changed", "property:BrickColor"])
    } else {
        HashMap::new()
    };
    let observed_color_ids = observed_union(&observed);
    let mass_observed = if actions
        .iter()
        .any(|action| fresh(action) && relevant(action) && action_has_mass_input(action))
    {
        super::signals::observed_ids(lua, ctx, &["Changed", "property:Mass"])
    } else {
        HashMap::new()
    };
    let observed_mass_ids = observed_union(&mass_observed);
    let mut masses = HashMap::<String, super::geometry::part_mass_properties::Observation>::new();
    // Retain only touched values, not a second copy of each node's property map.
    let mut state = HashMap::<String, HashMap<String, Option<Cow<'_, Json>>>>::new();
    let mut created = HashMap::<&str, &Json>::new();
    let mut aliases = HashMap::<String, HashMap<&'static str, Alias<'_>>>::new();
    let mut basepart_classes = HashMap::<String, bool>::new();
    for (index, action) in actions.iter().enumerate().filter(|(_, a)| fresh(a)) {
        if action["kind"] == "create" {
            if let Some(id) = action["node"]["id"].as_str() {
                created.insert(id, &action["node"]);
                state.remove(id);
                aliases.remove(id);
                masses.remove(id);
            }
            continue;
        }
        if !relevant(action) {
            continue;
        }
        let Some(id) = action["id"].as_str() else {
            continue;
        };
        let class = created
            .get(id)
            .and_then(|n| n["class_name"].as_str())
            .map(str::to_owned)
            .or_else(|| ctx.borrow().nodes.class_name(id).map(str::to_owned));
        let Some(class) = class else { continue };
        let is_basepart = is_basepart_class(&class, &mut basepart_classes);
        let values = state.entry(id.to_owned()).or_default();
        let mut changes = Vec::new();
        let fields = fields(action);
        let received = previous.is_some() || action["sequence"].as_u64().is_some();
        let received_frame = fields
            .get("CFrame")
            .copied()
            .filter(|_| received && is_basepart);
        let mut public_aliases = BTreeMap::new();
        for key in fields.keys() {
            if let Some(public) = public_alias(&class, key) {
                public_aliases.insert(public, world_aliases::wire_key(&class, public).unwrap());
            }
        }
        for (&key, &value) in &fields {
            if key == "Mass" && is_basepart {
                continue; // Compare the resolved mass once below, including derived changes.
            }
            if public_alias(&class, key).is_some() {
                continue;
            }
            // A received canonical frame clears local aliases. Equal-revision
            // explicit aliases are also subordinate to CFrame in public reads.
            if received_frame.is_some() && matches!(key, "Position" | "Orientation" | "Rotation") {
                continue;
            }
            if matches!(key, "Parent" | "ClassName")
                || (class == "Player" && key == "Character")
                || key == world::EXPLICIT_PIVOT
                || (action["_pivot_cframe_only"].as_bool() == Some(true)
                    && matches!(key, "Position" | "Orientation" | "Rotation"))
                || (key == "Name" && !value.is_string())
            {
                continue;
            }
            // A physics-only field is silent to property listeners. Avoid
            // decoding its previous projected value, but keep the comparison
            // state current for a later explicit change in this same batch.
            if received && is_basepart && physics_silent(action, key) {
                values.insert(key.to_owned(), Some(Cow::Borrowed(value)));
                continue;
            }
            let old = values
                .entry(key.to_owned())
                .or_insert_with(|| initial(ctx, id, key, created.get(id).copied()).map(Cow::Owned));
            if !old
                .as_ref()
                .is_some_and(|old| property_equal(key, old, value))
            {
                changes.push(Change {
                    id: id.to_owned(),
                    key: key.to_owned(),
                    value: Some(value),
                });
            }
            *old = Some(Cow::Borrowed(value));
        }
        if let Some(frame) = received_frame {
            for key in ["Position", "Orientation", "Rotation"] {
                let silent_physics = physics_silent(action, "CFrame");
                let old = values.entry(key.to_owned()).or_insert_with(|| {
                    if silent_physics {
                        None
                    } else {
                        created
                            .get(id)
                            .and_then(|node| node["properties"].get("CFrame"))
                            .map(|frame| super::movement::part_alias_from_frame(frame, key).ok())
                            .unwrap_or_else(|| initial(ctx, id, key, created.get(id).copied()))
                            .map(Cow::Owned)
                    }
                });
                let next = super::movement::part_alias_from_frame(frame, key).ok();
                let changed = !silent_physics && match (old.as_deref(), next.as_ref()) {
                    (Some(a), Some(b)) => !world::attribute_value_eq(a, b),
                    (None, None) => false,
                    _ => true,
                };
                if changed && action["_pivot_cframe_only"].as_bool() != Some(true) {
                    changes.push(Change {
                        id: id.to_owned(),
                        key: key.to_owned(),
                        value: None,
                    });
                }
                *old = next.map(Cow::Owned);
            }
        }
        for (public, wire) in public_aliases {
            let watch_brick = public == "Color" && observed_color_ids.contains(id);
            let alias = aliases
                .entry(id.to_owned())
                .or_default()
                .entry(public)
                .or_insert_with(|| {
                    Alias::new(
                        ctx,
                        id,
                        &class,
                        public,
                        wire,
                        created.get(id).copied(),
                        watch_brick,
                    )
                });
            let old_brick = alias.brick.clone();
            if alias.update(&class, public, wire, &fields, received) {
                changes.push(Change {
                    id: id.to_owned(),
                    key: public.to_owned(),
                    value: None,
                });
            }
            if watch_brick && old_brick != alias.brick {
                changes.push(Change {
                    id: id.to_owned(),
                    key: "BrickColor".into(),
                    value: None,
                });
            }
        }
        if is_basepart
            && observed_mass_ids.contains(id)
            && fields
                .keys()
                .any(|k| super::geometry::part_mass_properties::is_input(k))
        {
            if !masses.contains_key(id) {
                let observation = created
                    .get(id)
                    .map(|n| super::geometry::part_mass_properties::Observation::created(n))
                    .or_else(|| {
                        super::geometry::part_mass_properties::Observation::capture(ctx, id).ok()
                    });
                if let Some(observation) = observation {
                    masses.insert(id.to_owned(), observation);
                }
            }
            if let Some(mass) = masses.get_mut(id) {
                let old = mass.read().ok();
                mass.update(&fields, action["sequence"].as_u64());
                let next = mass.read().ok();
                if next.is_some() && old != next {
                    changes.push(Change {
                        id: id.to_owned(),
                        key: "Mass".into(),
                        value: None,
                    });
                }
            }
        }
        // A physics observation advances the comparison state above, including
        // derived pose aliases, but does not emit BasePart property signals.
        // Keeping state is important for a later explicit edit in this batch.
        if received && is_basepart {
            if let Some(physics) = action["_physics_fields"].as_array() {
                let explicit = action
                    .get("property")
                    .or_else(|| action.get("key"))
                    .and_then(Json::as_str);
                let silent = |key: &str| {
                    explicit != Some(key) && physics.iter().any(|v| v.as_str() == Some(key))
                };
                changes.retain(|change| {
                    // Only the fixed motion set is eligible. A malformed mask
                    // cannot suppress unrelated properties such as Color.
                    !match change.key.as_str() {
                        "Position" | "Orientation" | "Rotation" => silent("CFrame"),
                        key @ ("ReplicationPV"
                        | "CFrame"
                        | "AssemblyLinearVelocity"
                        | "Velocity"
                        | "AssemblyAngularVelocity"
                        | "RotVelocity") => silent(key),
                        _ => false,
                    }
                });
            }
        }
        // The public color changed above; its palette projection changes only
        // when the nearest named color changes. Do not scan the palette without receivers.
        if !changes.is_empty() {
            changes.sort_by(|a, b| a.key.cmp(&b.key));
            receipt.0.insert(index, changes);
        }
    }
    receipt
}

#[cfg(test)]
mod tests {
    use super::world::World;
    use super::*;
    use crate::{engine_world_projection::ProjectionContext, engine_world_view::SharedWorldView};
    use rove_client_core::{schema::Schema, world::WorldStore};
    use serde_json::json;
    use std::{cell::RefCell, rc::Rc};
    #[test]
    fn property_planning_only_materializes_the_touched_record() {
        let mut nodes = vec![
            json!({"id":"game","class_name":"DataModel","name":"game"}),
            json!({"id":"folder","class_name":"Folder","name":"folder"}),
        ];
        for i in 0..1000 {
            nodes.push(
                json!({"id":format!("p{i}"),"class_name":"Part","name":"part","parent":"folder"}),
            );
        }
        let store =
            WorldStore::from_snapshot(&json!({"version":1,"sequence":0,"nodes":nodes})).unwrap();
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
        let ctx = Rc::new(RefCell::new(
            World::from_snapshot(&json!({"version":1,"sequence":0,"nodes":[]}), Some(&view))
                .unwrap(),
        ));
        let lua = Lua::new();
        let before = ctx.borrow().nodes.materialized_count();
        let unrelated_actions = [
            json!({"kind":"attribute","sequence":1,"id":"p0","attribute_update":{"name":"Score","known":true,"value":1}}),
        ];
        let unrelated = prepare(&lua, &ctx, &unrelated_actions, Some(0));
        assert!(unrelated.0.is_empty());
        assert_eq!(ctx.borrow().nodes.materialized_count(), before);
        let actions = [
            json!({"kind":"set","sequence":1,"id":"p0","fields":{"Transparency":0.5}}),
            json!({"kind":"set","sequence":2,"id":"p0","property":"Transparency","value":0.5}),
        ];
        let properties = prepare(&lua, &ctx, &actions, Some(0));
        assert_eq!(properties.0.len(), 1);
        assert_eq!(properties.0[&0][0].key, "Transparency");
        assert_eq!(properties.0[&0][0].value, Some(&json!(0.5)));
        assert!(std::ptr::eq(
            properties.0[&0][0].value.unwrap(),
            &actions[0]["fields"]["Transparency"]
        ));
        assert_eq!(ctx.borrow().nodes.materialized_count(), before + 1);
        let alias_actions = [
            json!({"kind":"set","sequence":1,"id":"p1","fields":{"Color3uint8":{"$color3":[1,0,0]}}}),
            json!({"kind":"set","sequence":2,"id":"p1","fields":{"Color3uint8":{"$color3":[1.0,0.0,0.0]}}}),
        ];
        let aliases = prepare(&lua, &ctx, &alias_actions, Some(0));
        assert_eq!(aliases.0.len(), 1);
        assert_eq!(aliases.0[&0][0].key, "Color");
        assert!(aliases.0[&0][0].value.is_none());
        assert_eq!(ctx.borrow().nodes.materialized_count(), before + 2);
    }

    #[test]
    fn silent_physics_advances_comparison_state_without_materializing_old_pose() {
        let frame = |x| json!({"$cframe":{"position":[x,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}});
        let store = WorldStore::from_snapshot(&json!({
            "version":1,"sequence":0,"nodes":[
                {"id":"part","class_name":"Part","name":"Part","parent":null,
                 "properties":{"CFrame":frame(0.0)}}]
        })).unwrap();
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
        let ctx = Rc::new(RefCell::new(
            World::from_snapshot(&json!({"version":1,"sequence":0,"nodes":[]}), Some(&view))
                .unwrap(),
        ));
        let lua = Lua::new();
        let before = ctx.borrow().nodes.materialized_count();
        let physics_frame = frame(8.0);
        let physics = json!({
            "kind":"set","sequence":1,"id":"part","fields":{"CFrame":physics_frame},
            "_physics_fields":["CFrame"]
        });
        let receipt = prepare(&lua, &ctx, std::slice::from_ref(&physics), Some(0));
        assert!(receipt.0.is_empty());
        assert_eq!(ctx.borrow().nodes.materialized_count(), before);

        let same_frame_actions = [
            physics.clone(),
            json!({"kind":"set","sequence":2,"id":"part","property":"CFrame","value":frame(8.0)}),
        ];
        let same_frame_receipt = prepare(&lua, &ctx, &same_frame_actions, Some(0));
        assert!(!same_frame_receipt.0.contains_key(&1));
        assert_eq!(ctx.borrow().nodes.materialized_count(), before);

        let explicit_frame = frame(10.0);
        let actions = [
            physics,
            json!({"kind":"set","sequence":2,"id":"part","property":"CFrame","value":explicit_frame}),
        ];
        let receipt = prepare(&lua, &ctx, &actions, Some(0));
        let explicit_changes = &receipt.0[&1];
        assert!(explicit_changes.iter().any(|change| change.key == "CFrame"));
        assert!(explicit_changes.iter().any(|change| change.key == "Position"));
        assert_eq!(ctx.borrow().nodes.materialized_count(), before);
    }
}
