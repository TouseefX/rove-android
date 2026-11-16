//! Forward current local-character root velocity edits to the prediction owner.
use super::*;

pub(super) fn local_root(ctx: &WorldRef) -> Option<(String, String, String)> {
    find_root(ctx, None)
}

fn find_root(ctx: &WorldRef, candidate: Option<&str>) -> Option<(String, String, String)> {
    let player = ctx
        .borrow()
        .snapshot
        .get("local_player")?
        .as_str()?
        .to_owned();
    let character = property(ctx, &player, "Character").ok()?["$ref"]
        .as_str()?
        .to_owned();
    if candidate.is_some_and(|id| ctx.borrow().nodes.parent(id).flatten().as_deref() != Some(&character)) {
        return None;
    }
    let children = world::children(ctx, &character);
    let root = children
        .iter()
        .find(|child| world::is_a(ctx, child, "Humanoid"))
        .and_then(|humanoid| property(ctx, humanoid, "RootPart").ok())
        .and_then(|value| value["$ref"].as_str().map(str::to_owned))
        .or_else(|| {
            property(ctx, &character, "PrimaryPart")
                .ok()
                .and_then(|v| v["$ref"].as_str().map(str::to_owned))
        })
        .or_else(|| {
            children
                .iter()
                .find(|child| property(ctx, child, "Name").ok() == Some(json!("HumanoidRootPart")))
                .cloned()
        })?;
    (ctx.borrow().nodes.parent(&root).flatten().as_deref() == Some(&character))
        .then_some((player, character, root))
}

pub(super) fn binding(ctx: &WorldRef, id: &str) -> Option<(String, String)> {
    let (player, character, root) = find_root(ctx, Some(id))?;
    (root == id).then_some((player, character))
}

pub(super) fn set(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    if !is_basepart(ctx, id) {
        return Ok(false);
    }
    let velocity = vector(&value, "AssemblyLinearVelocity")?;
    let target = binding(ctx, id);
    if target.is_some() {
        validate_intent_queue(lua, ctx)?;
    }
    world::set_property(
        lua,
        ctx,
        id,
        "AssemblyLinearVelocity",
        json!({"$vector3":velocity}),
    )?;
    if let Some((player, character)) = target {
        queue_intent(
            lua,
            ctx,
            json!({"kind":"AssemblyLinearVelocity","instance_id":id,
            "root_id":id,"player_id":player,"character_id":character,"vector":velocity}),
        )?;
    }
    Ok(true)
}
