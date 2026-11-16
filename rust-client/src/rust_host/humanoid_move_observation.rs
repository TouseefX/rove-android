//! Intended walking input is separate from physical velocity.
use super::*;

pub(super) fn decode(value: &Json) -> mlua::Result<[f64; 3]> {
    let input = crate::physics_properties::vector(Some(value.clone()))
        .filter(|v| v[1] == 0. && v[0].hypot(v[2]) <= 1.000001)
        .ok_or_else(|| err("movement input observation must be a finite horizontal vector of length at most one"))?;
    Ok(crate::movement_math::horizontal_input(input))
}

pub(super) fn from_intent(ctx: &WorldRef, intent: &Json) -> mlua::Result<Option<(String, [f64; 3])>> {
    if intent["kind"] != "Move" { return Ok(None); }
    let Some(receiver) = intent["instance_id"].as_str() else { return Ok(None); };
    let humanoid = if world::is_a(ctx, receiver, "Humanoid") {
        Some(receiver.to_owned())
    } else if world::is_a(ctx, receiver, "Player") {
        property(ctx, receiver, "Character").ok()
            .and_then(|v| v["$ref"].as_str().map(str::to_owned))
            .and_then(|character| world::children(ctx, &character).into_iter()
                .find(|id| world::is_a(ctx, id, "Humanoid")))
    } else { None };
    let Some(humanoid) = humanoid else { return Ok(None); };
    let direction = crate::physics_properties::vector(intent.get("vector").cloned())
        .filter(|v| v.iter().all(|n| n.abs() <= 1.0e6))
        .ok_or_else(|| err("invalid Move input observation"))?;
    let direction = if intent["relative_to_camera"].as_bool() == Some(true) {
        crate::live_physics::camera_relative_direction(direction, intent.get("camera"))
            .ok_or_else(|| err("invalid Move camera observation"))?
    } else { direction };
    Ok(Some((humanoid, crate::movement_math::horizontal_input(direction))))
}

pub(super) fn apply(lua: &Lua, ctx: &WorldRef, id: &str, input: [f64; 3], sequence: u64) -> mlua::Result<()> {
    let length = input[0].hypot(input[2]);
    let direction = if length > 0. { input.map(|v| v / length) } else { [0.; 3] };
    let next = json!({"$vector3":direction});
    let changed = world::get_property(ctx, id, "MoveDirection").ok()
        .is_none_or(|old| !world::attribute_value_eq(&old, &next));
    world::set_engine_property(ctx, id, "__HumanoidMoveInput", json!(input), None)?;
    world::set_engine_property(ctx, id, "__HumanoidMoveSequence", json!(sequence), None)?;
    world::set_engine_property(ctx, id, "MoveDirection", next, None)?;
    if changed { world::emit_property_changed_direct(lua, ctx, id, "MoveDirection")?; }
    Ok(())
}

fn velocity(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    if !args.is_empty() { return Err(err("GetMoveVelocity expects no arguments")); }
    let input = match property(ctx, id, "__HumanoidMoveInput") {
        Ok(value) => decode(&value)?,
        Err(_) => {
            let direction = property(ctx, id, "MoveDirection").ok()
                .and_then(|v| crate::physics_properties::vector(Some(v)));
            if direction != Some([0.; 3]) {
                return Err(err("intended movement input is unavailable until Move or physics feedback"));
            }
            [0.; 3]
        }
    };
    let velocity = if input == [0.; 3] { input } else {
        let speed = property(ctx, id, "WalkSpeed")?.as_f64()
            .filter(|v| v.is_finite() && (0.0..=crate::live_physics::MAX_WALK_SPEED).contains(v))
            .ok_or_else(|| err("WalkSpeed is unavailable or outside the supported range"))?;
        input.map(|v| v * speed)
    };
    Ok(MultiValue::from_vec(vec![world::value_from_json(lua, ctx, &json!({"$vector3":velocity}))?]))
}

pub(super) fn install(ctx: &WorldRef) {
    world::register_method(ctx, "Humanoid", "GetMoveVelocity", velocity);
    world::register_setter(ctx, "MoveDirection", |_, ctx, id, _| {
        if world::is_a(ctx, id, "Humanoid") { Err(err("MoveDirection is read-only")) } else { Ok(false) }
    });
}
