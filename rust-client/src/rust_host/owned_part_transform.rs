//! Preserve receipt evidence when a script moves a received physics part.
//! The network owner validates authority and topology again before encoding.
use super::*;
#[derive(Default)]
struct Tracked(std::collections::HashSet<String>);

/// A later edit in the same script step can invalidate an earlier pose.
pub(crate) fn discard_invalid(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    let mut tracked = lua.remove_app_data::<Tracked>().unwrap_or_default();
    let mut stopped = Vec::new();
    let mut world = ctx.borrow_mut();
    let mut intents = world
        .effects
        .remove("movement_intents")
        .unwrap_or_else(|| json!([]));
    if let Some(rows) = intents.as_array_mut() {
        if !tracked.0.is_empty() || rows.iter().any(|row| row["kind"] == "OwnedPartCFrame") {
            let graph = crate::rust_host::joint_graph::Graph::build_in_world(
                &world,
                crate::rust_host::joint_graph::Scope::All,
            );
            let valid = |id: &str| {
                let Ok(graph) = &graph else {
                    return false;
                };
                if graph
                    .incident
                    .get(id)
                    .is_some_and(|edges| !edges.is_empty())
                {
                    return false;
                }
                let Some(node) = world.nodes.get(id) else {
                    return false;
                };
                if node.destroyed || node.local_overrides.get("Anchored") == Some(&json!(true)) {
                    return false;
                }
                if [
                    "Size",
                    "Shape",
                    "CanCollide",
                    "CanTouch",
                    "CollisionGroup",
                    "Material",
                    "MaterialVariant",
                    "CustomPhysicalProperties",
                    "AssemblyLinearVelocity",
                    "AssemblyAngularVelocity",
                ]
                .iter()
                .any(|name| node.local_overrides.contains_key(*name))
                {
                    return false;
                }
                // Local mover changes need their own simulation command. Stop
                // an older received controller rather than silently using it.
                if world
                    .nodes
                    .child_ids(id)
                    .iter()
                    .filter_map(|id| world.nodes.get(id))
                    .any(|n| {
                        n.class_name == "BodyVelocity"
                            && (n.destroyed
                                || !n.server_owned
                                || ["P", "MaxForce", "Velocity", "Parent"]
                                    .iter()
                                    .any(|k| n.local_overrides.contains_key(*k)))
                    })
                {
                    return false;
                }
                let mut cursor = Some(id.to_owned());
                for _ in 0..256 {
                    let Some(id) = cursor else {
                        return true;
                    };
                    let Some(node) = world.nodes.get(&id) else {
                        return false;
                    };
                    if node.destroyed || node.local_overrides.contains_key("Parent") {
                        return false;
                    }
                    if node.class_name == "Workspace"
                        && node.local_overrides.contains_key("Gravity")
                    {
                        return false;
                    }
                    cursor = node.parent.clone();
                }
                false
            };
            tracked.0.retain(|id| {
                if valid(id) {
                    true
                } else {
                    stopped.push(id.clone());
                    false
                }
            });
            rows.retain(|row| {
                if row["kind"] != "OwnedPartCFrame" {
                    return true;
                }
                let Some(id) = row["instance_id"].as_str() else {
                    return false;
                };
                if !valid(id) || (!tracked.0.contains(id) && tracked.0.len() >= 64) {
                    return false;
                }
                tracked.0.insert(id.into());
                true
            });
        }
    }
    if intents.as_array().is_some_and(|a| !a.is_empty()) {
        world.effects.insert("movement_intents".into(), intents);
    }
    drop(world);
    lua.set_app_data(tracked);
    for id in stopped {
        queue_intent(lua, ctx, json!({"kind":"OwnedPartStop","instance_id":id}))?;
    }
    Ok(())
}

pub(super) fn prepare(ctx: &WorldRef, id: &str, frame: &CFrameValue) -> mlua::Result<Option<Json>> {
    let world = ctx.borrow();
    let Some(node) = world
        .nodes
        .get(id)
        .filter(|n| n.server_owned && !n.destroyed)
    else {
        return Ok(None);
    };
    let Some(projected) = &node.projected else {
        return Ok(None);
    };
    let raw = projected.raw();
    let Some(created) = raw.created_sequence() else {
        return Ok(None);
    };
    let Some(owner) = raw.property("NetworkOwnerV3") else {
        return Ok(None);
    };
    // Local reparenting or anchoring must never manufacture send authority.
    if node.local_overrides.contains_key("Parent")
        || node.local_overrides.get("Anchored") == Some(&json!(true))
    {
        return Ok(None);
    }
    if !frame.rotation_known
        || !crate::physics_sender::validate_rotation(frame.rotation)
        || frame
            .position
            .iter()
            .any(|v| !v.is_finite() || v.abs() >= 1e9)
    {
        return Err(err(
            "received part transform requires a finite position and valid rotation",
        ));
    }
    Ok(Some(json!({"kind":"OwnedPartCFrame","instance_id":id,
        "received_sequence":world.sequence,"created_sequence":created,
        "received_owner":owner,"cframe":encode_frame(frame)})))
}
