//! A character pivot carries the actual root frame after model offsets.
use super::*;

pub(super) fn prepare(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    next: &[Record],
) -> mlua::Result<Option<Json>> {
    // BasePart:PivotTo has already removed PivotOffset from the requested pivot.
    if world::is_a(ctx, id, "BasePart") {
        return match next.iter().find(|r| r.id == id && r.kind == Kind::Part) {
            Some(record) => character_transform::prepare(lua, ctx, id, &record.frame.value()),
            None => Ok(None),
        };
    }
    let Some(player) = ctx
        .borrow()
        .snapshot
        .get("local_player")
        .and_then(Json::as_str)
        .map(str::to_owned)
    else {
        return Ok(None);
    };
    let character = world::get_property(ctx, &player, "Character")
        .ok()
        .and_then(|v| v["$ref"].as_str().map(str::to_owned));
    if character.as_deref() != Some(id) {
        return Ok(None);
    }
    let children = world::children(ctx, id);
    let root = children
        .iter()
        .find(|child| world::is_a(ctx, child, "Humanoid"))
        .and_then(|humanoid| world::get_property(ctx, humanoid, "RootPart").ok())
        .and_then(|v| v["$ref"].as_str().map(str::to_owned))
        .or_else(|| primary(ctx, id).ok().flatten())
        .or_else(|| {
            children
                .iter()
                .find(|child| {
                    world::get_property(ctx, child, "Name").ok().as_ref()
                        == Some(&json!("HumanoidRootPart"))
                })
                .cloned()
        });
    let Some(root) = root else {
        return Ok(None);
    };
    let Some(record) = next.iter().find(|r| r.id == root && r.kind == Kind::Part) else {
        return Ok(None);
    };
    if record.frame.p.iter().any(|v| v.abs() >= 1e9) {
        return Err(err("character pivot exceeds supported physics coordinates"));
    }
    validate_intent_queue(lua,ctx)?;
    Ok(Some(
        json!({"kind":"PivotTo","instance_id":id,"player_id":player,
        "root_id":root,"cframe":record.frame.json()}),
    ))
}
