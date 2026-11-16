//! Forward validated physics observations without BasePart property signals.
use super::*;
use crate::rust_host::actors;

pub(super) const POSE_KEYS: &[&str] = &["CFrame", "Position", "Orientation", "Rotation"];
pub(super) const BODY_KEYS: &[&str] = &[
    "CFrame",
    "Position",
    "Orientation",
    "Rotation",
    "AssemblyLinearVelocity",
    "AssemblyAngularVelocity",
];

struct After<'a> {
    id: &'a str,
    values: Vec<Json>,
}

pub(super) fn apply(
    lua: &Lua,
    ctx: &WorldRef,
    batch: &Json,
    keys: &[&str],
    revision: Option<u64>,
    commit: impl FnOnce() -> Result<Json, String>,
) -> mlua::Result<Json> {
    let result = commit().map_err(err)?;
    if result["status"] != "applied" {
        return Ok(result);
    }
    // The existing commit path validates and caps batches at 32 parts. Physics
    // observations update reads, but do not emit BasePart property signals.
    // https://create.roblox.com/docs/reference/engine/classes/Object#GetPropertyChangedSignal
    let after = batch["frames"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|frame| frame["id"].as_str())
        .map(|id| {
            let values = keys
                .iter()
                .map(|key| world::get_property(ctx, id, key))
                .collect::<mlua::Result<Vec<_>>>()?;
            Ok(After { id, values })
        })
        .collect::<mlua::Result<Vec<_>>>()?;
    world::refresh_derived_properties(lua)?;

    if actors::has_live_vms(lua) {
        actors::for_each_actor(lua, |_, actor_lua, actor_world, _| {
            for row in &after {
                if !actor_world.borrow().nodes.is_live(row.id) || !is_basepart(actor_world, row.id)
                {
                    continue;
                }
                for (key, value) in keys.iter().zip(&row.values) {
                    world::set_engine_property(actor_world, row.id, key, value.clone(), revision)?;
                }
            }
            world::refresh_derived_properties(actor_lua)
        })?;
    }
    Ok(result)
}
