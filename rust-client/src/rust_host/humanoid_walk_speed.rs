//! Keep a current-character speed write ordered with its movement commands.
use super::*;

pub(super) fn set(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    if !world::is_a(ctx, id, "Humanoid") {
        return Ok(false);
    }
    let speed = match value {
        Value::Number(n) => n,
        Value::Integer(n) => n as f64,
        _ => return Err(err("WalkSpeed expects a number")),
    };
    if !speed.is_finite() || !(0.0..=crate::live_physics::MAX_WALK_SPEED).contains(&speed) {
        return Err(err("WalkSpeed must be finite and between 0 and 10000"));
    }
    let player = ctx.borrow().snapshot["local_player"].as_str().map(str::to_owned);
    let target = player.and_then(|player| {
        let character = property(ctx, &player, "Character").ok()?["$ref"].as_str()?.to_owned();
        (ctx.borrow().nodes.parent(id).flatten().as_deref() == Some(&character))
            .then_some((player, character))
    });
    if target.is_some() {
        validate_intent_queue(lua, ctx)?;
    }
    let received_sequence = ctx.borrow().sequence;
    world::set_property(lua, ctx, id, "WalkSpeed", json!(speed))?;
    if let Some((player, character)) = target {
        queue_intent(lua, ctx, json!({
            "kind":"WalkSpeed","instance_id":id,"player_id":player,
            "character_id":character,"value":speed,"received_sequence":received_sequence
        }))?;
    }
    Ok(true)
}
