//! Send an explicit local-root transform through the existing pivot command.
use super::*;

pub(super) fn prepare(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    frame: &CFrameValue,
) -> mlua::Result<Option<Json>> {
    let Some((player, character)) = character_velocity::binding(ctx, id) else {
        let intent = owned_part_transform::prepare(ctx, id, frame)?;
        if intent.is_some() { validate_intent_queue(lua, ctx)?; }
        return Ok(intent);
    };
    prepare_bound(lua, ctx, id, frame, player, character).map(Some)
}

pub(super) fn prepare_batch(lua: &Lua, ctx: &WorldRef, writes: &[world::PoseWrite]) -> mlua::Result<Vec<Json>> {
    let mut intents = Vec::new();
    for write in writes {
        // Joint rebasing also writes C0/C1 offsets, which are not part poses.
        let Some(value) = write.fields.get("CFrame") else { continue; };
        let frame = cframe_from_json(value)?;
        if let Some(intent) = prepare(lua, ctx, &write.id, &frame)? { intents.push(intent); }
    }
    if !intents.is_empty() && !replaying_intents(lua) {
        let counter = lua.app_data_ref::<IntentSequence>().ok_or_else(|| err("movement intent sequence is not installed"))?;
        let existing = ctx.borrow().effects.get("movement_intents")
            .and_then(Json::as_array).map_or(0, Vec::len);
        if existing.saturating_add(intents.len()) > counter.1.get()
            || counter.0.get().checked_add(intents.len() as u64).is_none() {
            return Err(err("local movement intent batch exceeds queue capacity"));
        }
    }
    Ok(intents)
}

fn prepare_bound(lua: &Lua, ctx: &WorldRef, id: &str, frame: &CFrameValue,
    player: String, character: String) -> mlua::Result<Json> {
    if !frame.rotation_known
        || frame.position.iter().any(|v| !v.is_finite() || v.abs() >= 1e9)
        || !crate::physics_sender::validate_rotation(frame.rotation) {
        return Err(err("character root transform requires a finite supported position and valid rotation"));
    }
    validate_intent_queue(lua, ctx)?;
    Ok(json!({"kind":"PivotTo","instance_id":character,
        "player_id":player,"root_id":id,"cframe":encode_frame(frame)}))
}
