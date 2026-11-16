//! Health changes preserve their values across deferred root and Actor delivery.
use super::{
    property_events, signals,
    world::{self, WorldRef},
};
use mlua::{Lua, MultiValue, Value};
use serde_json::{json, Value as Json};
use std::collections::{HashMap, HashSet};
fn err(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}
fn number(value: &Json) -> Option<f64> {
    value.as_f64().filter(|v| v.is_finite())
}
fn known(ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<f64> {
    world::get_property(ctx, id, key)
        .ok()
        .as_ref()
        .and_then(number)
        .ok_or_else(|| err(format!("{key} is not a known finite health value")))
}
fn reference(value: &Json) -> Option<String> {
    value
        .as_str()
        .or_else(|| value.get("$ref").and_then(Json::as_str))
        .map(str::to_owned)
}
fn death_allowed(
    ctx: &WorldRef,
    id: &str,
    parents: &HashMap<String, Option<String>>,
    classes: &HashMap<String, String>,
) -> bool {
    if world::get_property(ctx, id, "__HumanoidStateEnabled_15")
        .ok()
        .and_then(|v| v.as_bool())
        == Some(false)
    {
        return false;
    }
    let world = ctx.borrow();
    let mut cursor = parents
        .get(id)
        .cloned()
        .unwrap_or_else(|| world.nodes.live_parent(id));
    let mut seen = HashSet::new();
    while let Some(id) = cursor {
        if !seen.insert(id.clone()) {
            break;
        }
        if classes
            .get(&id)
            .map(String::as_str)
            .or_else(|| world.nodes.class_name(&id))
            == Some("Workspace")
        {
            return true;
        }
        cursor = parents
            .get(&id)
            .cloned()
            .unwrap_or_else(|| world.nodes.live_parent(&id));
    }
    false
}
fn fire(lua: &Lua, ctx: &WorldRef, id: &str, health: f64, died: bool) -> mlua::Result<()> {
    signals::fire(
        lua,
        ctx,
        id,
        "HealthChanged",
        MultiValue::from_vec(vec![Value::Number(health)]),
    )?;
    if died {
        signals::fire(lua, ctx, id, "Died", MultiValue::new())?;
    }
    Ok(())
}
fn set_humanoid_health(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    key: &str,
    value: Value,
) -> mlua::Result<bool> {
    if !world::is_a(ctx, id, "Humanoid") || !matches!(key, "Health" | "MaxHealth") {
        return Ok(false);
    }
    if !ctx.borrow().nodes.is_live(id) {
        return Err(err("health receiver is destroyed"));
    }
    let requested = match value {
        Value::Number(v) => v,
        Value::Integer(v) => v as f64,
        _ => return Err(err("health must be a number")),
    };
    if !requested.is_finite() {
        return Err(err("health must be finite"));
    }
    let old = world::get_property(ctx, id, "Health")?
        .as_f64()
        .filter(|v| v.is_finite())
        .ok_or_else(|| err("Health is not a known finite health value"))?;
    let old_max = world::get_property(ctx, id, "MaxHealth")
        .ok()
        .and_then(|v| v.as_f64())
        .filter(|v| v.is_finite());
    let max = if key == "MaxHealth" {
        requested.max(0.)
    } else {
        match old_max {
            Some(value) => value.max(0.),
            None if old <= 0. || requested <= old => old.max(0.),
            None => return Err(err("healing requires a known MaxHealth")),
        }
    };
    let next = if old <= 0. {
        0.
    } else if key == "Health" {
        requested.clamp(0., max)
    } else {
        old.min(max)
    };
    let max_changed = key == "MaxHealth" && old_max != Some(max);
    if max_changed {
        world::set_property(lua, ctx, id, "MaxHealth", json!(max))?;
        if next != old {
            // The following Health mutation carries the single resulting event
            // to peer VMs; do not report the pre-clamp health first.
            if let Some(row) = ctx
                .borrow_mut()
                .effects
                .get_mut("mutations")
                .and_then(Json::as_array_mut)
                .and_then(|rows| rows.last_mut())
            {
                row["_humanoid_health_silent"] = json!(true);
            }
        }
    }
    if next != old {
        world::set_property(lua, ctx, id, "Health", json!(next))?;
    }
    if next != old || max_changed {
        let dying = old > 0. && next <= 0.;
        let died = dying && death_allowed(ctx, id, &HashMap::new(), &HashMap::new());
        if dying {
            // State enablement is local engine state, not a property journal
            // row. Capture the event decision before a peer sees later settings.
            if let Some(row) = ctx
                .borrow_mut()
                .effects
                .get_mut("mutations")
                .and_then(Json::as_array_mut)
                .and_then(|rows| rows.last_mut())
            {
                row["_humanoid_died"] = json!(died);
            }
        }
        fire(lua, ctx, id, next, died)?;
    }
    Ok(true)
}

fn protected(ctx: &WorldRef, id: &str) -> bool {
    let root = world::get_property(ctx, id, "RootPart")
        .ok()
        .as_ref()
        .and_then(reference);
    let owner = ctx.borrow().nodes.live_parent(id);
    let root = root.or_else(|| {
        owner.as_ref().and_then(|owner| {
            world::children(ctx, owner).into_iter().find(|part| {
                world::is_a(ctx, part, "BasePart")
                    && world::get_property(ctx, part, "Name")
                        .ok()
                        .is_some_and(|name| name == "HumanoidRootPart")
            })
        })
    });
    let world = ctx.borrow();
    let mut parents = HashSet::new();
    if let Some(root) = root {
        parents.insert(root);
    }
    // A sibling force field always protects; ancestor fields exclude Workspace.
    if let Some(owner) = &owner {
        parents.insert(owner.clone());
    }
    let mut cursor = owner;
    let mut seen = HashSet::new();
    while let Some(id) = cursor {
        if !seen.insert(id.clone()) {
            break;
        }
        if world.nodes.class_name(&id) != Some("Workspace") {
            parents.insert(id.clone());
        }
        cursor = world.nodes.live_parent(&id);
    }
    let protected = world.header_order().any(|key| {
        world.nodes.is_live(key)
            && world.nodes.class_name(key) == Some("ForceField")
            && world
                .nodes
                .live_parent(key)
                .is_some_and(|parent| parents.contains(&parent))
    });
    protected
}
fn take_damage(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    if !world::is_a(ctx, id, "Humanoid") || !ctx.borrow().nodes.is_live(id) {
        return Err(err("TakeDamage requires a live Humanoid"));
    }
    let amount = args.front().and_then(|v| match v {
        Value::Integer(v) => Some(*v as f64),
        Value::Number(v) => Some(*v),
        _ => None,
    });
    let amount = amount
        .filter(|v| v.is_finite())
        .filter(|_| args.len() == 1)
        .ok_or_else(|| err("TakeDamage expects one finite number"))?;
    if protected(ctx, id) {
        return Ok(MultiValue::new());
    }
    let health = known(ctx, id, "Health")?;
    if health <= 0. {
        return Ok(MultiValue::new());
    }
    let next = if amount >= 0. {
        (health - amount).max(0.)
    } else {
        let max = known(ctx, id, "MaxHealth")?.max(0.);
        if -amount >= max - health {
            max
        } else {
            health - amount
        }
    };
    set_humanoid_health(lua, ctx, id, "Health", Value::Number(next))?;
    Ok(MultiValue::new())
}
pub(super) fn install(ctx: &WorldRef) {
    for key in ["Health", "MaxHealth"] {
        world::register_keyed_setter(ctx, key, set_humanoid_health);
    }
    for key in ["HealthChanged", "Died"] {
        world::register_keyed_getter(ctx, key, |lua, ctx, id, key| {
            if world::is_a(ctx, id, "Humanoid") {
                Ok(Some(signals::signal(lua, ctx, id, key)?))
            } else {
                Ok(None)
            }
        });
    }
    world::register_method(ctx, "Humanoid", "TakeDamage", take_damage);
}
#[derive(Default)]
pub(super) struct Receipt(HashMap<usize, (String, f64, bool)>);
impl Receipt {
    pub(super) fn publish(&mut self, lua: &Lua, ctx: &WorldRef, index: usize) -> mlua::Result<()> {
        if let Some((id, health, died)) = self.0.remove(&index) {
            fire(lua, ctx, &id, health, died)?;
        }
        Ok(())
    }
}
#[derive(Default)]
struct Health {
    value: Option<f64>,
    max: Option<f64>,
}
pub(super) fn prepare(ctx: &WorldRef, actions: &[Json], previous: Option<u64>) -> Receipt {
    let fresh = |a: &Json| previous.is_none_or(|seq| a["sequence"].as_u64().unwrap_or(0) > seq);
    let mut receipt = Receipt::default();
    if !actions.iter().any(|a| {
        fresh(a)
            && property_events::relevant(a)
            && (a["fields"].get("Health").is_some()
                || a["fields"].get("MaxHealth").is_some()
                || matches!(
                    a.get("property")
                        .or_else(|| a.get("key"))
                        .and_then(Json::as_str),
                    Some("Health" | "MaxHealth")
                ))
    }) {
        return receipt;
    }
    let mut state = HashMap::<String, Health>::new();
    let mut parents = HashMap::new();
    let mut classes = HashMap::new();
    for (index, a) in actions.iter().enumerate().filter(|(_, a)| fresh(a)) {
        if a["kind"] == "create" {
            let n = &a["node"];
            if let (Some(id), Some(class)) = (n["id"].as_str(), n["class_name"].as_str()) {
                classes.insert(id.to_owned(), class.to_owned());
                parents.insert(id.to_owned(), reference(&n["parent"]));
                if class == "Humanoid" {
                    state.insert(
                        id.to_owned(),
                        Health {
                            value: number(&n["properties"]["Health"]),
                            max: number(&n["properties"]["MaxHealth"]),
                        },
                    );
                }
            }
            continue;
        }
        let Some(id) = a["id"].as_str() else { continue };
        if matches!(a["kind"].as_str(), Some("remove" | "destroy")) {
            parents.insert(id.to_owned(), None);
            continue;
        }
        if !property_events::relevant(a) {
            continue;
        }
        let fields = property_events::fields(a);
        if let Some(parent) = fields.get("Parent") {
            parents.insert(id.to_owned(), reference(parent));
        }
        if !fields.contains_key("Health") && !fields.contains_key("MaxHealth") {
            continue;
        }
        if !classes.get(id).map_or_else(
            || world::is_a(ctx, id, "Humanoid"),
            |class| class == "Humanoid",
        ) {
            continue;
        }
        let h = state.entry(id.to_owned()).or_insert_with(|| Health {
            value: known(ctx, id, "Health").ok(),
            max: known(ctx, id, "MaxHealth").ok(),
        });
        let old = h.value;
        let old_max = h.max;
        if let Some(value) = fields.get("MaxHealth") {
            h.max = number(value);
        }
        if let Some(value) = fields.get("Health") {
            h.value = number(value);
        }
        let Some(next) = h.value else { continue };
        let local = previous.is_none() && a["sequence"].as_u64().is_none();
        let max_changed = fields.contains_key("MaxHealth")
            && h.max != old_max
            && !(local && a["_humanoid_health_silent"] == true);
        let increasing_above_max = old
            .zip(h.max)
            .is_some_and(|(old, max)| old >= max && next > old);
        if (fields.contains_key("Health") && old != Some(next) && !increasing_above_max)
            || max_changed
        {
            let died = old.is_some_and(|old| old > 0.)
                && next <= 0.
                && (if local {
                    a["_humanoid_died"].as_bool()
                } else {
                    None
                })
                .unwrap_or_else(|| death_allowed(ctx, id, &parents, &classes));
            receipt.0.insert(index, (id.to_owned(), next, died));
        }
    }
    receipt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine_world_projection::ProjectionContext, engine_world_view::SharedWorldView};
    use rove_client_core::{schema::Schema, world::WorldStore};
    use std::{cell::RefCell, rc::Rc};
    #[test]
    fn health_and_forcefield_checks_do_not_materialize_unrelated_world_records() {
        let mut nodes = vec![
            json!({"id":"game","class_name":"DataModel","name":"game"}),
            json!({"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game"}),
            json!({"id":"char","class_name":"Model","name":"Character","parent":"ws"}),
            json!({"id":"hum","class_name":"Humanoid","name":"Humanoid","parent":"char","properties":{"Health":100,"MaxHealth":100}}),
            json!({"id":"shield","class_name":"ForceField","name":"ForceField","parent":"char"}),
        ];
        for i in 0..1000 {
            nodes.push(
                json!({"id":format!("p{i}"),"class_name":"Part","name":"Part","parent":"ws"}),
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
            world::World::from_snapshot(&json!({"version":1,"sequence":0,"nodes":[]}), Some(&view))
                .unwrap(),
        ));
        let before = ctx.borrow().nodes.materialized_count();
        let irrelevant = prepare(
            &ctx,
            &[json!({"kind":"set","id":"p0","sequence":1,"fields":{"Transparency":0.5}})],
            Some(0),
        );
        assert!(irrelevant.0.is_empty());
        assert_eq!(ctx.borrow().nodes.materialized_count(), before);
        let health = prepare(
            &ctx,
            &[json!({"kind":"set","id":"hum","sequence":1,"fields":{"Health":0}})],
            Some(0),
        );
        assert_eq!(health.0.get(&0), Some(&("hum".into(), 0., true)));
        assert_eq!(ctx.borrow().nodes.materialized_count(), before + 1);
        assert!(protected(&ctx, "hum"));
        assert_eq!(ctx.borrow().nodes.materialized_count(), before + 1);
    }
}
