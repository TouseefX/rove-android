//! Validate a complete feedback observation before changing values or firing events.
use super::*;
use crate::rust_host::{actors, humanoid_state, signals};
struct Observation<'a> {
    id: &'a str,
    speed: Option<f64>,
    old_speed: Option<f64>,
    state: Option<i32>,
    old_state: Option<i32>,
    motion: Option<(&'a str, Json)>,
    move_input: Option<([f64; 3], u64)>,
    move_to_finished: Vec<(u64, bool)>,
}
fn prepare<'a>(ctx: &WorldRef, feedback: &'a Json) -> mlua::Result<Option<Observation<'a>>> {
    if !feedback.is_object() {
        return Err(err("physics feedback must be an object"));
    }
    let Some(id) = feedback.get("humanoid_id").and_then(Json::as_str) else {
        if ["running_speed", "state", "motion", "move_to_finished", "move_input", "move_sequence"]
            .iter()
            .any(|key| feedback.get(key).is_some())
        {
            return Err(err("physics observations require a Humanoid ID"));
        }
        return Ok(None);
    };
    let local_player = ctx
        .borrow()
        .snapshot
        .get("local_player")
        .and_then(Json::as_str)
        .map(str::to_owned)
        .ok_or_else(|| err("local player is unavailable for physics feedback"))?;
    let character = property(ctx, &local_player, "Character")?
        .get("$ref")
        .and_then(Json::as_str)
        .map(str::to_owned)
        .ok_or_else(|| err("local character is unavailable for physics feedback"))?;
    if !ctx.borrow().nodes.is_live(id)
        || class(ctx, id)? != "Humanoid"
        || !is_descendant_by_parent(ctx, id, &character)
    {
        return Err(err(
            "physics feedback Humanoid is not the current local character",
        ));
    }
    let speed = feedback
        .get("running_speed")
        .map(|value| {
            value
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 0.)
                .ok_or_else(|| err("physics feedback running_speed must be finite and nonnegative"))
        })
        .transpose()?;
    let state = feedback
        .get("state")
        .map(|value| {
            let number = humanoid_state::decode(&value["value"])?;
            let name = HUMANOID_STATES
                .iter()
                .find(|(_, v)| *v == number)
                .unwrap()
                .0;
            if value["name"] != name {
                return Err(err("physics feedback state name/value mismatch"));
            }
            Ok(number)
        })
        .transpose()?;
    let old_state = if state.is_some() {
        humanoid_state::observed(ctx, id)?
    } else {
        None
    };
    let old_speed = ctx
        .borrow()
        .nodes
        .get(id)
        .and_then(|n| n.local_overrides.get("__HumanoidRunningFeedback"))
        .and_then(Json::as_f64);
    let motion = feedback
        .get("motion")
        .map(|motion| {
            let root = motion
                .get("root_id")
                .and_then(Json::as_str)
                .ok_or_else(|| err("invalid physics motion feedback"))?;
            let velocity = motion
                .get("linear_velocity")
                .and_then(Json::as_array)
                .filter(|v| {
                    v.len() == 3
                        && v.iter().all(|n| {
                            n.as_f64()
                                .is_some_and(|x| x.is_finite() && x.abs() <= 1.0e6)
                        })
                })
                .ok_or_else(|| err("physics feedback velocity must be finite and bounded"))?;
            let primary = property(ctx, &character, "PrimaryPart")
                .ok()
                .and_then(|v| v.get("$ref").and_then(Json::as_str).map(str::to_owned));
            let valid = ctx.borrow().nodes.get(root).is_some_and(|n| {
                !n.destroyed
                    && n.parent.as_deref() == Some(character.as_str())
                    && (n.name == "HumanoidRootPart" || primary.as_deref() == Some(root))
            });
            if !is_basepart(ctx, root) || !valid {
                return Err(err(
                    "physics feedback root is not the current local character",
                ));
            }
            Ok((root, json!({"$vector3":velocity})))
        })
        .transpose()?;
    let move_input = feedback.get("move_input").map(|value| {
        let input = humanoid_move_observation::decode(value)?;
        let sequence = feedback["move_sequence"].as_u64()
            .ok_or_else(|| err("movement input feedback requires its command sequence"))?;
        Ok::<_, mlua::Error>((input, sequence))
    }).transpose()?.filter(|(_, sequence)| {
        let previous = property(ctx, id, "__HumanoidMoveSequence").ok().and_then(|v| v.as_u64()).unwrap_or(0);
        *sequence >= previous
    });
    let mut move_to_finished = Vec::new();
    if let Some(events) = feedback.get("move_to_finished") {
        let events = events.as_array().filter(|rows| rows.len() <= 256)
            .ok_or_else(|| err("invalid MoveToFinished feedback"))?;
        let mut previous = 0;
        for event in events {
            let sequence = event["sequence"].as_u64().filter(|n| *n > previous)
                .ok_or_else(|| err("MoveToFinished feedback must be ordered"))?;
            let reached = event["reached"].as_bool()
                .ok_or_else(|| err("MoveToFinished feedback requires a boolean"))?;
            previous = sequence;
            move_to_finished.push((sequence, reached));
        }
    }
    Ok(Some(Observation {
        id,
        speed,
        old_speed,
        state,
        old_state,
        motion,
        move_input,
        move_to_finished,
    }))
}
fn commit(lua: &Lua, ctx: &WorldRef, o: &Observation<'_>) -> mlua::Result<()> {
    if let Some(speed) = o.speed {
        world::set_engine_property(ctx, o.id, "__HumanoidRunningFeedback", json!(speed), None)?;
    }
    if let Some(state) = o.state {
        world::set_engine_property(ctx, o.id, "__HumanoidStateFeedback", json!(state), None)?;
    }
    if let Some((root, velocity)) = &o.motion {
        if ctx.borrow().nodes.is_live(root) {
            world::set_engine_property(
                ctx,
                root,
                "AssemblyLinearVelocity",
                velocity.clone(),
                None,
            )?;
        }
    }
    if let Some((input, sequence)) = o.move_input { humanoid_move_observation::apply(lua, ctx, o.id, input, sequence)?; }
    if let Some(speed) = o.speed.filter(|v| Some(*v) != o.old_speed) {
        signals::fire(
            lua,
            ctx,
            o.id,
            "Running",
            MultiValue::from_vec(vec![Value::Number(speed)]),
        )?;
    }
    if let Some(state) = o.state {
        humanoid_state::emit(lua, ctx, o.id, o.old_state, state)?;
    }
    for &(sequence, reached) in &o.move_to_finished {
        let cursor = ctx.borrow().nodes.get(o.id)
            .and_then(|node| node.local_overrides.get("__HumanoidMoveToFeedback"))
            .and_then(Json::as_u64).unwrap_or(0);
        if sequence > cursor {
            world::set_engine_property(ctx, o.id, "__HumanoidMoveToFeedback", json!(sequence), None)?;
            signals::fire(lua, ctx, o.id, "MoveToFinished",
                MultiValue::from_vec(vec![Value::Boolean(reached)]))?;
        }
    }
    Ok(())
}
/// Applies only observations validated against the current local character.
/// Session-level pose application is a separate operation.
pub fn apply(lua: &Lua, ctx: &WorldRef, feedback: &Json) -> mlua::Result<()> {
    let Some(observation) = prepare(ctx, feedback)? else {
        return Ok(());
    };
    commit(lua, ctx, &observation)?;
    if actors::has_live_vms(lua) {
        actors::for_each_actor(lua, |_, actor_lua, actor_world, _| {
            if actor_world.borrow().nodes.is_live(observation.id) {
                commit(actor_lua, actor_world, &observation)?;
            }
            Ok(())
        })?;
    }
    Ok(())
}
