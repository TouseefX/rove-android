//! Material catalog queries over the current world, without retaining a second catalog.
use super::{
    enums,
    world::{self, WorldRef},
};
use mlua::{Lua, MultiValue, Value};
use serde_json::{json, Value as Json};
#[path = "material_service_fields.rs"]
pub(super) mod fields;
const MAX_VARIANTS: usize = 4096;
const MAX_ANCESTORS: usize = 1024;
fn error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}
fn receiver(ctx: &WorldRef, id: &str) -> mlua::Result<()> {
    if !world::is_a(ctx, id, "MaterialService") || !ctx.borrow().nodes.is_live(id) {
        return Err(error("material method requires a live MaterialService"));
    }
    Ok(())
}
fn material(value: &Value) -> mlua::Result<i64> {
    let Value::UserData(item) = value else {
        return Err(error("expected Enum.Material"));
    };
    if !enums::enum_item_matches(item, "Material")? {
        return Err(error("expected Enum.Material"));
    }
    Ok(enums::enum_item_value(item)? as i64)
}
fn name(value: &Value) -> mlua::Result<String> {
    let Value::String(s) = value else {
        return Err(error("material variant name must be a string"));
    };
    if s.as_bytes().len() > 65_536 {
        return Err(error("material variant name exceeds the byte limit"));
    }
    Ok(s.to_str()?.to_string())
}
pub(crate) fn override_name(ctx: &WorldRef, id: &str, material: i64) -> mlua::Result<String> {
    receiver(ctx, id)?;
    let key = fields::override_field(material)
        .ok_or_else(|| error("base material overrides are unavailable for this material"))?;
    match world::get_property(ctx, id, key) {
        Ok(Json::String(name)) => Ok(name),
        Ok(_) => Err(error("received material override name must be a string")),
        Err(_) if ctx.borrow().nodes.get(id).is_some_and(|n| !n.server_owned) => Ok(String::new()),
        Err(e) => Err(e),
    }
}
fn descendant(ctx: &WorldRef, id: &str, service: &str) -> mlua::Result<bool> {
    let w = ctx.borrow();
    let mut cursor = w.nodes.live_parent(id);
    for _ in 0..MAX_ANCESTORS {
        let Some(parent) = cursor else {
            return Ok(false);
        };
        if parent == service {
            return Ok(true);
        }
        cursor = w.nodes.live_parent(&parent);
    }
    Err(error("material variant ancestry exceeds the query limit"))
}
pub(crate) fn find_variant(
    ctx: &WorldRef,
    service: &str,
    material: i64,
    name: &str,
) -> mlua::Result<Option<String>> {
    receiver(ctx, service)?;
    let ids = {
        let w = ctx.borrow();
        // Headers are enough to reject unrelated world objects. Only candidate
        // variants acquire property wrappers, in the existing received order.
        w.header_order()
            .filter(|id| w.nodes.class_name(id) == Some("MaterialVariant") && w.nodes.is_live(id))
            .take(MAX_VARIANTS + 1)
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    if ids.len() > MAX_VARIANTS {
        return Err(error("material variant query limit exceeded"));
    }
    for id in ids {
        if !ctx.borrow().nodes.get(&id).is_some_and(|n| n.name == name)
            || !descendant(ctx, &id, service)?
        {
            continue;
        }
        let base = world::get_property(ctx, &id, "BaseMaterial")?;
        let value = base
            .get("$enum")
            .filter(|e| e["type"] == "Material")
            .and_then(|e| e["value"].as_i64())
            .ok_or_else(|| error("variant BaseMaterial is unavailable"))?;
        if value == material {
            return Ok(Some(id));
        }
    }
    Ok(None)
}
fn get_override(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    if args.len() != 1 {
        return Err(error("GetBaseMaterialOverride expects one material"));
    }
    let value = override_name(ctx, id, material(&args[0])?)?;
    Ok(MultiValue::from_vec(vec![Value::String(
        lua.create_string(value)?,
    )]))
}
fn set_override(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    receiver(ctx, id)?;
    if args.len() != 2 {
        return Err(error("SetBaseMaterialOverride expects a material and name"));
    }
    let material = material(&args[0])?;
    let key = fields::override_field(material)
        .ok_or_else(|| error("base material overrides are unavailable for this material"))?;
    let value = name(&args[1])?;
    // Use ordinary mutations so other Actor VMs and subsequent receipts see
    // exactly the same state. No VM-only override map is needed.
    write_override(lua, ctx, id, key, &value)?;
    Ok(MultiValue::new())
}
fn write_override(lua: &Lua, ctx: &WorldRef, id: &str, key: &str, value: &str) -> mlua::Result<()> {
    receiver(ctx, id)?;
    if !fields::OVERRIDE_FIELDS.contains(&key) || value.len() > 65_536 {
        return Err(error("invalid material override mutation"));
    }
    let cursor = ctx
        .borrow()
        .effects
        .get("mutations")
        .and_then(Json::as_array)
        .map_or(0, Vec::len);
    world::set_property(lua, ctx, id, key, json!(value))?;
    if let Some(rows) = ctx
        .borrow_mut()
        .effects
        .get_mut("mutations")
        .and_then(Json::as_array_mut)
    {
        for row in rows
            .iter_mut()
            .skip(cursor)
            .filter(|r| r["kind"] == "set" && r["id"] == id && r["property"] == key)
        {
            row["_material_override"] = json!(true);
        }
    }
    Ok(())
}
/// Replay only method-produced, validated override rows. Ordinary script writes
/// still go through the non-scriptable property guard and normal revision fences.
pub(crate) fn replay_override(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    key: &str,
    row: &Json,
) -> mlua::Result<bool> {
    if row["_material_override"] != true {
        return Ok(false);
    }
    let value = row["value"]
        .as_str()
        .ok_or_else(|| error("material override name must be a string"))?;
    write_override(lua, ctx, id, key, value)?;
    Ok(true)
}
fn get_variant(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    if args.len() != 2 {
        return Err(error("GetMaterialVariant expects a material and name"));
    }
    let found = find_variant(ctx, id, material(&args[0])?, &name(&args[1])?)?;
    Ok(MultiValue::from_vec(vec![match found {
        Some(id) => world::instance(lua, ctx, &id)?,
        None => Value::Nil,
    }]))
}
pub(super) fn install(ctx: &WorldRef) {
    world::register_method(ctx, "MaterialService", "GetMaterialVariant", get_variant);
    world::register_method(
        ctx,
        "MaterialService",
        "GetBaseMaterialOverride",
        get_override,
    );
    world::register_method(
        ctx,
        "MaterialService",
        "SetBaseMaterialOverride",
        set_override,
    );
    for &key in fields::OVERRIDE_FIELDS {
        world::register_keyed_getter(ctx, key, |_, ctx, id, _| {
            if world::is_a(ctx, id, "MaterialService") {
                return Err(error(
                    "material override fields are not scriptable; use GetBaseMaterialOverride",
                ));
            }
            Ok(None)
        });
        world::register_keyed_setter(ctx, key, |_, ctx, id, _, _| {
            if world::is_a(ctx, id, "MaterialService") {
                return Err(error(
                    "material override fields are not scriptable; use SetBaseMaterialOverride",
                ));
            }
            Ok(false)
        });
    }
    world::register_keyed_setter(ctx, "BaseMaterial", |lua, ctx, id, key, value| {
        if !world::is_a(ctx, id, "MaterialVariant") {
            return Ok(false);
        }
        material(&value)?;
        world::set_property(lua, ctx, id, key, super::values::to_json(lua, &value)?)?;
        Ok(true)
    });
}

/// Resolve explicit physical data for an authored part. Missing received
/// configuration remains unknown; an absent local service has no overrides.
pub(crate) fn local_physical(
    ctx: &WorldRef,
    material: i64,
    variant: &str,
) -> mlua::Result<Option<Json>> {
    catalog_physical(ctx, material, variant, false)
}

/// Resolve a received part only from known configuration. Unlike an authored
/// world, a partially received catalog cannot prove that a named variant is
/// absent. None means a known fallback to built-in properties, not unknown data.
pub(crate) fn received_physical(
    ctx: &WorldRef,
    material: i64,
    variant: &str,
) -> mlua::Result<Option<Json>> {
    catalog_physical(ctx, material, variant, true)
}

fn catalog_physical(
    ctx: &WorldRef,
    material: i64,
    variant: &str,
    received: bool,
) -> mlua::Result<Option<Json>> {
    let service = {
        let w = ctx.borrow();
        w.services
            .get("MaterialService")
            .filter(|id| w.nodes.is_live(id))
            .cloned()
            .or_else(|| {
                w.nodes
                    .first_live_of_class("MaterialService")
                    .map(|n| n.id.clone())
            })
    };
    let Some(service) = service else {
        return if !received && variant.is_empty() {
            Ok(None)
        } else {
            Err(error("material variant catalog is unavailable"))
        };
    };
    receiver(ctx, &service)?;
    let complete = ctx.borrow().snapshot["initial_replication_complete"] == true;
    if !variant.is_empty() {
        if let Some(id) = find_variant(ctx, &service, material, variant)? {
            let value = world::get_property(ctx, &id, "CustomPhysicalProperties")?;
            if !value.is_null() {
                return Ok(Some(value));
            }
        } else if received && !complete {
            return Err(error("named material variant has not been received"));
        }
    }
    if fields::override_field(material).is_none() {
        return Ok(None);
    }
    let base = if received {
        // A locally synthesized service does not establish the server's empty
        // override. An explicit method write or received field does.
        let key = fields::override_field(material).unwrap();
        world::get_property(ctx, &service, key)?
            .as_str()
            .ok_or_else(|| error("received material override name must be a string"))?
            .to_owned()
    } else {
        override_name(ctx, &service, material)?
    };
    if base.is_empty() {
        return Ok(None);
    }
    if let Some(id) = find_variant(ctx, &service, material, &base)? {
        let value = world::get_property(ctx, &id, "CustomPhysicalProperties")?;
        if !value.is_null() {
            return Ok(Some(value));
        }
    } else if received && !complete {
        return Err(error("base material variant has not been received"));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine_world_projection::ProjectionContext, engine_world_view::SharedWorldView};
    use rove_client_core::{schema::Schema, world::WorldStore};
    use std::{cell::RefCell, collections::HashMap, rc::Rc};
    fn node(id: &str, parent: &str) -> Json {
        json!({"id":id,"class_name":"MaterialVariant","name":"Dense","parent":parent,"properties":{
            "BaseMaterial":{"$enum":{"type":"Material","value":512}},"CustomPhysicalProperties":{"$physical_properties":[2,0.5,0.25,1,1,0]}
        }})
    }
    #[test]
    fn shared_material_queries_do_not_materialize_unrelated_world_properties() {
        let mut nodes = vec![
            json!({"id":"service","class_name":"MaterialService","name":"MaterialService","properties":{"WoodName":"Dense"}}),
            json!({"id":"folder","class_name":"Folder","name":"Pack","parent":"service"}),
            node("variant", "folder"),
        ];
        for i in 0..1000 {
            nodes.push(json!({"id":format!("untouched{i}"),"class_name":"Part","name":"Part"}));
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
        assert_eq!(
            find_variant(&ctx, "service", 512, "Dense")
                .unwrap()
                .as_deref(),
            Some("variant")
        );
        assert_eq!(override_name(&ctx, "service", 512).unwrap(), "Dense");
        assert_eq!(
            local_physical(&ctx, 512, "").unwrap().unwrap()["$physical_properties"][0],
            json!(2)
        );
        let w = ctx.borrow();
        for i in 0..1000 {
            assert!(w.nodes.peek(&format!("untouched{i}")).is_none());
        }
        assert!(w.nodes.peek("folder").is_none());
        assert!(w.effects.get("mutations").is_none());
    }
    #[test]
    fn duplicate_variants_use_received_order_and_ignore_other_materials() {
        let mut wrong = node("wrong", "service");
        wrong["properties"]["BaseMaterial"]["$enum"]["value"] = json!(816);
        let nodes = vec![
            json!({"id":"service","class_name":"MaterialService","name":"MaterialService"}),
            wrong,
            node("first", "service"),
            node("second", "service"),
        ];
        let ctx = Rc::new(RefCell::new(
            world::World::from_snapshot(&json!({"version":1,"sequence":0,"nodes":nodes}), None)
                .unwrap(),
        ));
        assert_eq!(
            find_variant(&ctx, "service", 512, "Dense")
                .unwrap()
                .as_deref(),
            Some("first")
        );
        assert_eq!(
            find_variant(&ctx, "service", 816, "Dense")
                .unwrap()
                .as_deref(),
            Some("wrong")
        );
    }
    #[test]
    fn oversized_catalog_is_rejected_without_mutations() {
        let mut nodes =
            vec![json!({"id":"service","class_name":"MaterialService","name":"MaterialService"})];
        for i in 0..=MAX_VARIANTS {
            nodes.push(node(&format!("v{i}"), "service"));
        }
        let ctx = Rc::new(RefCell::new(
            world::World::from_snapshot(&json!({"version":1,"sequence":0,"nodes":nodes}), None)
                .unwrap(),
        ));
        assert!(find_variant(&ctx, "service", 512, "Dense")
            .unwrap_err()
            .to_string()
            .contains("query limit"));
        assert!(ctx.borrow().effects.get("mutations").is_none());
    }
    #[test]
    fn override_replay_rejects_unrelated_fields_types_and_receivers() {
        let lua = Lua::new();
        let ctx = Rc::new(RefCell::new(
            world::World::from_snapshot(
                &json!({"version":1,"sequence":0,"nodes":[
                    {"id":"service","class_name":"MaterialService","name":"MaterialService"},
                    {"id":"part","class_name":"Part","name":"Part"}
                ]}),
                None,
            )
            .unwrap(),
        ));
        assert!(
            !replay_override(&lua, &ctx, "service", "WoodName", &json!({"value":"Dense"})).unwrap()
        );
        assert!(replay_override(
            &lua,
            &ctx,
            "service",
            "Name",
            &json!({"_material_override":true,"value":"Dense"})
        )
        .is_err());
        assert!(replay_override(
            &lua,
            &ctx,
            "part",
            "WoodName",
            &json!({"_material_override":true,"value":"Dense"})
        )
        .is_err());
        assert!(replay_override(
            &lua,
            &ctx,
            "service",
            "WoodName",
            &json!({"_material_override":true,"value":9})
        )
        .is_err());
        assert!(ctx.borrow().effects.get("mutations").is_none());
    }
}

pub(super) fn named_custom(ctx: &WorldRef, material: i64, name: &str) -> mlua::Result<bool> {
    let service = {
        let w = ctx.borrow();
        w.services
            .get("MaterialService")
            .filter(|id| w.nodes.is_live(id))
            .cloned()
            .or_else(|| {
                w.nodes
                    .first_live_of_class("MaterialService")
                    .map(|n| n.id.clone())
            })
    };
    let Some(service) = service else {
        return Ok(false);
    };
    let Some(id) = find_variant(ctx, &service, material, name)? else {
        return Ok(false);
    };
    Ok(!world::get_property(ctx, &id, "CustomPhysicalProperties")?.is_null())
}
