//! Sample one active target after root and Actor edits have committed.
use super::*;
use std::cell::RefCell;

#[derive(Clone)]
struct Watch {
    humanoid: String,
    part: String,
    sequence: u64,
    received: bool,
    last: Json,
}
#[derive(Default)]
struct State(RefCell<Option<Watch>>);
pub(super) fn install(lua: &Lua) { lua.set_app_data(State::default()); }

pub(super) fn accepted(lua: &Lua, ctx: &WorldRef, intent: &Json, sequence: u64) {
    if world::uses_actor_namespace(ctx) { return; }
    let Some(player) = ctx.borrow().snapshot["local_player"].as_str().map(str::to_owned) else { return; };
    let Some(character) = property(ctx, &player, "Character").ok()
        .and_then(|v| v["$ref"].as_str().map(str::to_owned)) else { return; };
    let Some(id) = intent["instance_id"].as_str() else { return; };
    if id != player && (ctx.borrow().nodes.parent(id).flatten().as_deref() != Some(&character)
        || !world::is_a(ctx, id, "Humanoid")) { return; }
    let state = lua.app_data_ref::<State>().unwrap();
    match intent["kind"].as_str() {
        Some("MoveTo") => {
            *state.0.borrow_mut() = intent["part_id"].as_str().map(|part| Watch {
                humanoid: id.into(), part: part.into(), sequence,
                received: ctx.borrow().nodes.get(part).is_some_and(|node| node.server_owned),
                last: json!({"use_received":true}),
            });
        }
        Some("Move" | "CancelMoveTo") => { state.0.borrow_mut().take(); }
        _ => {}
    }
}

fn workspace(ctx: &WorldRef, id: &str) -> Option<String> {
    let mut at = id.to_owned();
    for _ in 0..64 {
        let w = ctx.borrow();
        let node = w.nodes.get(&at).filter(|node| !node.destroyed)?;
        if node.class_name == "Workspace" { return Some(at); }
        at = w.nodes.parent(&at).flatten()?;
    }
    None
}

pub(crate) fn flush(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    let Some(state) = lua.app_data_ref::<State>() else { return Ok(()); };
    let Some(watch) = state.0.borrow().clone() else { return Ok(()); };
    drop(state);
    let player = ctx.borrow().snapshot["local_player"].as_str().map(str::to_owned);
    let character = player.as_deref().and_then(|p| property(ctx,p,"Character").ok())
        .and_then(|v| v["$ref"].as_str().map(str::to_owned));
    let finished = property(ctx, &watch.humanoid, "__HumanoidMoveToFeedback").ok()
        .and_then(|v| v.as_u64()).is_some_and(|s| s >= watch.sequence);
    if finished || character.is_none() || !ctx.borrow().nodes.is_live(&watch.humanoid)
        || ctx.borrow().nodes.parent(&watch.humanoid).flatten() != character {
        lua.app_data_ref::<State>().unwrap().0.borrow_mut().take();
        return Ok(());
    }
    let expected = workspace(ctx, character.as_deref().unwrap());
    let actual = workspace(ctx, &watch.part);
    let live = ctx.borrow().nodes.is_live(&watch.part);
    let local = !watch.received || world::raw_property_origin(ctx, &watch.part, "CFrame").ok().as_deref() == Some("local");
    let mut sample = if live && actual.is_some() && actual == expected && !local {
        json!({"use_received":true})
    } else {
        let frame = if live && actual.is_some() && actual == expected {
            property(ctx, &watch.part, "CFrame").ok().and_then(|v| cframe_from_json(&v).ok())
                .filter(|v| v.rotation_known && v.position.iter().all(|x| x.is_finite() && x.abs()<1e9)
                    && crate::physics_sender::validate_rotation(v.rotation))
                .map(|v| encode_frame(&v))
        } else { None };
        let revision = ["CFrame","ReplicationPV","Position","Orientation","Rotation"].iter()
            .filter_map(|key| world::get_property_revision(ctx,&watch.part,key)).max().unwrap_or(0);
        json!({"use_received":false,"cframe":frame,"received":watch.received,"workspace_id":expected,"pose_revision":revision})
    };
    if sample == watch.last || remaining_intents(lua, ctx) == 0 { return Ok(()); }
    let saved = sample.clone();
    sample["kind"] = json!("MoveToTarget");
    sample["instance_id"] = json!(watch.humanoid);
    sample["part_id"] = json!(watch.part);
    sample["request_sequence"] = json!(watch.sequence);
    sample["received_sequence"] = json!(ctx.borrow().sequence);
    queue_intent(lua, ctx, sample)?;
    if let Some(active) = lua.app_data_ref::<State>().unwrap().0.borrow_mut().as_mut() {
        active.last = saved;
    }
    Ok(())
}
