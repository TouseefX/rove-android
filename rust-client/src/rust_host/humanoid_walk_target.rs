//! Destination property edits end an existing MoveTo; they do not start one.
use super::*;

pub(super) fn part(ctx: &WorldRef, value: &Value) -> mlua::Result<Option<String>> {
    if matches!(value, Value::Nil) { return Ok(None); }
    world::instance_id(value)
        .filter(|id| world::instance_belongs_to(value, ctx)
            && world::is_a(ctx, id, "BasePart")
            && ctx.borrow().nodes.get(id).is_some_and(|node| !node.destroyed))
        .map(Some)
        .ok_or_else(|| err("WalkToPart must be a live BasePart in this world or nil"))
}

pub(super) fn point(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    if !world::is_a(ctx, id, "Humanoid") { return Ok(false); }
    let point = vector(&value, "WalkToPoint")?;
    set(lua, ctx, id, "WalkToPoint", json!({"$vector3":point}))
}

pub(super) fn target_part(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    if !world::is_a(ctx, id, "Humanoid") { return Ok(false); }
    let value = part(ctx, &value)?.map_or(Json::Null, |part| json!({"$ref":part}));
    set(lua, ctx, id, "WalkToPart", value)
}

fn set(lua: &Lua, ctx: &WorldRef, id: &str, key: &str, value: Json) -> mlua::Result<bool> {
    let changed = world::get_property(ctx, id, key).ok()
        .is_none_or(|old| !world::attribute_value_eq(&old, &value));
    let player = ctx.borrow().snapshot["local_player"].as_str().map(str::to_owned);
    let binding = player.and_then(|player| {
        let character = property(ctx, &player, "Character").ok()?["$ref"].as_str()?.to_owned();
        (changed && ctx.borrow().nodes.parent(id).flatten().as_deref() == Some(&character))
            .then_some((player, character))
    });
    if binding.is_some() { validate_intent_queue(lua, ctx)?; }
    world::set_property(lua, ctx, id, key, value)?;
    if let Some((player, character)) = binding {
        queue_intent(lua, ctx, json!({"kind":"CancelMoveTo","instance_id":id,
            "player_id":player,"character_id":character,"property":key}))?;
    }
    Ok(true)
}
