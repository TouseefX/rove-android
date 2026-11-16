//! Bounded local-object journal merging between isolated Actor VMs and their
//! shared root World. This only transfers mutations produced by the Rust
//! Instance bindings; it has no network or packet path.

use super::world::{self, WorldRef};
use mlua::{Lua, ObjectLike, Result, Value};
use serde_json::{json, Value as Json};
use std::collections::HashMap;

const MAX_ROWS: usize = 4096;
const MAX_JOURNAL_BYTES: usize = 1024 * 1024;

#[derive(Default, Debug)]
pub struct MergeReport {
    pub applied: usize,
    pub conflicts: Vec<String>,
    pub(crate) first_conflict: Option<usize>,
    pub(crate) mutation_cursors: Vec<usize>,
}

fn error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}

fn mutation_rows(ctx: &WorldRef) -> usize {
    ctx.borrow()
        .effects
        .get("mutations")
        .and_then(Json::as_array)
        .map_or(0, Vec::len)
}

fn tag_new_root_mutations(ctx: &WorldRef, from: usize, actor_id: &str) -> Result<()> {
    let mut world = ctx.borrow_mut();
    let Some(Json::Array(rows)) = world.effects.get_mut("mutations") else {
        return Ok(());
    };
    for row in rows.iter_mut().skip(from) {
        if !row.is_object() {
            return Err(error("root mutation journal contains a non-object row"));
        }
        row["actor_origin"] = json!(actor_id);
    }
    Ok(())
}

fn source_node_matches(actor_world: &WorldRef, root_world: &WorldRef, id: &str, retiring: bool) -> bool {
    let actor = actor_world.borrow();
    let root = root_world.borrow();
    let Some(actor_node) = actor.nodes.get(id) else {
        return false;
    };
    let Some(root_node) = root.nodes.get(id) else {
        return false;
    };
    actor_node.class_name == root_node.class_name
        && (!actor_node.destroyed || retiring)
        && !root_node.destroyed
        && actor_node.server_owned == root_node.server_owned
}

fn base_revision(row: &Json) -> Result<u64> {
    row.get("_base_local_revision")
        .and_then(Json::as_u64)
        .ok_or_else(|| error("Actor mutation is missing its local revision fence"))
}

fn revision_matches(root_world: &WorldRef, id: &str, base: u64) -> bool {
    root_world
        .borrow()
        .nodes
        .get(id)
        .is_some_and(|node| !node.destroyed && node.local_revision == base)
}

fn translate_revision_fences(row: &mut Json, revisions: &HashMap<(String, u64), u64>) {
    if let (Some(id), Some(base)) = (
        row.get("id").and_then(Json::as_str),
        row.get("_base_local_revision").and_then(Json::as_u64),
    ) {
        if let Some(revision) = revisions.get(&(id.to_owned(), base)) {
            row["_base_local_revision"] = json!(revision);
        }
    }
    if let Some(bases) = row.get_mut("_base_local_revisions").and_then(Json::as_object_mut) {
        for (id, base) in bases {
            if let Some(revision) = base.as_u64().and_then(|base| revisions.get(&(id.clone(), base))) {
                *base = json!(revision);
            }
        }
    }
    if let Some(writes) = row.get_mut("writes").and_then(Json::as_array_mut) {
        for write in writes { translate_revision_fences(write, revisions); }
    }
}

fn clear_stale_override(actor_world: &WorldRef, id: &str, property: &str) {
    if let Some(node) = actor_world.borrow_mut().nodes.get_mut(id) {
        node.local_overrides.remove(property);
    }
}

fn set_property(lua: &Lua, root_world: &WorldRef, actor_id: &str, row: &Json) -> Result<()> {
    let id = row
        .get("id")
        .and_then(Json::as_str)
        .ok_or_else(|| error("Actor set mutation needs an Instance ID"))?;
    let property = row
        .get("property")
        .or_else(|| row.get("key"))
        .and_then(Json::as_str)
        .ok_or_else(|| error("Actor set mutation needs a property name"))?;
    let payload = row
        .get("value")
        .ok_or_else(|| error("Actor set mutation needs a value"))?;
    let target = world::instance(lua, root_world, id)?;
    let Value::UserData(target) = target else {
        return Err(error("Actor set mutation target is unavailable"));
    };
    let value = world::value_from_json(lua, root_world, payload)?;
    let cursor = mutation_rows(root_world);
    if !super::material_service::replay_override(lua, root_world, id, property, row)? {
        target.set(property, value)?;
    }
    tag_new_root_mutations(root_world, cursor, actor_id)
}

fn set_attribute(lua: &Lua, root_world: &WorldRef, actor_id: &str, row: &Json) -> Result<()> {
    let id = row
        .get("id")
        .and_then(Json::as_str)
        .ok_or_else(|| error("Actor attribute mutation needs an Instance ID"))?;
    let name = row
        .get("name")
        .and_then(Json::as_str)
        .ok_or_else(|| error("Actor attribute mutation needs a name"))?;
    let payload = row
        .get("value")
        .ok_or_else(|| error("Actor attribute mutation needs a value"))?;
    let target = world::instance(lua, root_world, id)?;
    let Value::UserData(target) = target else {
        return Err(error("Actor attribute mutation target is unavailable"));
    };
    let value = world::value_from_json(lua, root_world, payload)?;
    let cursor = mutation_rows(root_world);
    target.call_method::<()>("SetAttribute", (name, value))?;
    tag_new_root_mutations(root_world, cursor, actor_id)
}

fn set_tags(lua: &Lua, root_world: &WorldRef, actor_id: &str, row: &Json) -> Result<()> {
    let id = row
        .get("id")
        .and_then(Json::as_str)
        .ok_or_else(|| error("Actor tag mutation needs an Instance ID"))?;
    let desired = row
        .get("tags")
        .and_then(Json::as_array)
        .ok_or_else(|| error("Actor tag mutation needs a tag list"))?
        .iter()
        .map(|tag| {
            tag.as_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| error("Actor tag list contains a non-string value"))
        })
        .collect::<Result<Vec<_>>>()?;
    if desired.len() > 256 {
        return Err(error("Actor tag list exceeds the 256 tag limit"));
    }
    let service_id = root_world
        .borrow()
        .services
        .get("CollectionService")
        .cloned()
        .ok_or_else(|| error("CollectionService is unavailable in this World"))?;
    let service = world::instance(lua, root_world, &service_id)?;
    let Value::UserData(service) = service else {
        return Err(error("CollectionService handle is unavailable"));
    };
    let target = world::instance(lua, root_world, id)?;
    let existing = root_world
        .borrow()
        .nodes
        .get(id)
        .filter(|node| node.collection_tags_known && !node.destroyed)
        .map(|node| node.collection_tags.clone())
        .ok_or_else(|| error("Actor tag target has unknown tag state"))?;
    let mut desired = desired;
    desired.sort();
    desired.dedup();
    let cursor = mutation_rows(root_world);
    for tag in existing.iter().filter(|tag| !desired.contains(tag)) {
        service.call_method::<()>("RemoveTag", (target.clone(), tag.as_str()))?;
    }
    for tag in desired.iter().filter(|tag| !existing.contains(tag)) {
        service.call_method::<()>("AddTag", (target.clone(), tag.as_str()))?;
    }
    tag_new_root_mutations(root_world, cursor, actor_id)
}

fn destroy(lua: &Lua, root_world: &WorldRef, actor_id: &str, row: &Json) -> Result<()> {
    let ids = row
        .get("ids")
        .and_then(Json::as_array)
        .ok_or_else(|| error("Actor destroy mutation needs an ID list"))?;
    let revisions = row
        .get("_base_local_revisions")
        .and_then(Json::as_object)
        .ok_or_else(|| error("Actor destroy mutation is missing revision fences"))?;
    if ids.len() > 100_000 {
        return Err(error("Actor destroy mutation exceeds the instance limit"));
    }
    let mut selected = Vec::with_capacity(ids.len());
    for value in ids {
        let id = value
            .as_str()
            .ok_or_else(|| error("Actor destroy mutation has an invalid ID"))?;
        let base = revisions
            .get(id)
            .and_then(Json::as_u64)
            .ok_or_else(|| error("Actor destroy mutation has an invalid revision fence"))?;
        if !revision_matches(root_world, id, base) {
            return Err(error(format!("stale Actor destroy mutation for {id}")));
        }
        selected.push(id.to_owned());
    }
    let selected_set = selected.iter().collect::<std::collections::HashSet<_>>();
    let roots = {
        let world = root_world.borrow();
        selected
            .iter()
            .filter(|id| {
                !world
                    .nodes
                    .get(*id)
                    .and_then(|node| node.parent.as_ref())
                    .is_some_and(|parent| selected_set.contains(parent))
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    let cursor = mutation_rows(root_world);
    for id in roots {
        world::destroy_instance(lua, root_world, &id)?;
    }
    tag_new_root_mutations(root_world, cursor, actor_id)
}

/// Apply one Actor VM's internal Instance journal in order. Invalid rows fail
/// explicitly; revision conflicts are reported and the stale local override is
/// discarded before the Actor fork is refreshed.
pub fn merge_actor_journal(
    lua: &Lua,
    root_world: &WorldRef,
    actor_world: &WorldRef,
    actor_id: &str,
    journal: &[Json],
) -> Result<MergeReport> {
    merge_actor_journal_traced(lua,root_world,actor_world,actor_id,journal,false)
}
pub(crate) fn merge_actor_journal_traced(
    lua: &Lua, root_world: &WorldRef, actor_world: &WorldRef,
    actor_id: &str, journal: &[Json], trace: bool,
) -> Result<MergeReport> {
    let _intent_guard=super::movement::suppress_replayed_intents(lua);
    if journal.len() > MAX_ROWS {
        return Err(error("Actor shadow journal exceeds the 4096 row limit"));
    }
    if serde_json::to_vec(journal)
        .map_err(mlua::Error::external)?
        .len()
        > MAX_JOURNAL_BYTES
    {
        return Err(error("Actor shadow journal exceeds the 1 MiB limit"));
    }
    let mut report = MergeReport::default();
    // A final snapshot may already contain a tombstone for an earlier create
    // or write. Permit that source only when a later destroy row carries its
    // actual final revision; the root still checks every normal write fence.
    let mut retirements = HashMap::new();
    for (index, row) in journal.iter().enumerate().filter(|(_, row)| row["kind"] == "destroy") {
        for id in row["ids"].as_array().into_iter().flatten().filter_map(Json::as_str) {
            if actor_world.borrow().nodes.get(id).is_some_and(|node| {
                node.destroyed && row["_base_local_revisions"][id].as_u64() == Some(node.local_revision)
            }) {
                retirements.insert(id.to_owned(), index);
            }
        }
    }
    // Forks allocate revision numbers independently. Translate only revisions
    // produced by writes this journal has already committed successfully; an
    // unrelated root write still fails the ordinary optimistic fence.
    let mut revisions = HashMap::<(String, u64), u64>::new();
    for (index, row) in journal.iter().enumerate() {
        if trace {report.mutation_cursors.push(mutation_rows(root_world));}
        let retiring = |id: &str| retirements.get(id).is_some_and(|last| *last > index);
        let mut translated;
        let row = if revisions.is_empty() {
            row
        } else {
            translated = row.clone();
            translate_revision_fences(&mut translated, &revisions);
            &translated
        };
        let applied_before = report.applied;
        let kind = row
            .get("kind")
            .and_then(Json::as_str)
            .ok_or_else(|| error("Actor shadow row needs a kind"))?;
        match kind {
            "create" => {
                let node = row
                    .get("node")
                    .ok_or_else(|| error("Actor create row needs a node record"))?;
                let pending_destroy = node["id"].as_str().is_some_and(retiring);
                world::apply_actor_create_for_replay(lua, root_world, actor_world, actor_id, node, pending_destroy)?;
                report.applied += 1;
            }
            "set" => {
                let id = row
                    .get("id")
                    .and_then(Json::as_str)
                    .ok_or_else(|| error("Actor set row needs an ID"))?;
                let property = row
                    .get("property")
                    .or_else(|| row.get("key"))
                    .and_then(Json::as_str)
                    .ok_or_else(|| error("Actor set row needs a property"))?;
                let base = base_revision(row)?;
                if !source_node_matches(actor_world, root_world, id, retiring(id))
                    || !revision_matches(root_world, id, base)
                {
                    clear_stale_override(actor_world, id, property);
                    report.first_conflict.get_or_insert(index);
                    report.conflicts.push(format!("set:{id}.{property}"));
                    continue;
                }
                set_property(lua, root_world, actor_id, row)?;
                report.applied += 1;
            }
            "pose_batch" => {
                let list = row
                    .get("writes")
                    .and_then(Json::as_array)
                    .filter(|v| !v.is_empty() && v.len() <= 4096)
                    .ok_or_else(|| error("Actor pose batch needs bounded writes"))?;
                let cframe_only = row
                    .get("cframe_only")
                    .and_then(Json::as_bool)
                    .ok_or_else(|| error("Actor pose batch needs an event mode"))?;
                let mut writes = Vec::new();
                let mut conflict = false;
                for record in list {
                    let id = record
                        .get("id")
                        .and_then(Json::as_str)
                        .ok_or_else(|| error("Actor pose target needs an ID"))?;
                    let fields = record
                        .get("fields")
                        .and_then(Json::as_object)
                        .ok_or_else(|| error("Actor pose fields are missing"))?;
                    let base = base_revision(record)?;
                    conflict |= !source_node_matches(actor_world, root_world, id, retiring(id))
                        || !revision_matches(root_world, id, base);
                    writes.push(world::PoseWrite {
                        id: id.into(),
                        fields: fields.clone(),
                    });
                }
                if conflict {
                    for write in writes {
                        for key in write.fields.keys() {
                            clear_stale_override(actor_world, &write.id, key);
                        }
                    }
                    report.first_conflict.get_or_insert(index);
                    report.conflicts.push("pose_batch:stale_target".into());
                    continue;
                }
                let cursor = mutation_rows(root_world);
                let changes = world::apply_pose_batch(root_world, &writes, cframe_only)?;
                tag_new_root_mutations(root_world, cursor, actor_id)?;
                world::pose_events(lua, root_world, changes)?;
                report.applied += 1;
            }
            "curve_keys" => {
                let cursor = mutation_rows(root_world);
                if super::curves::replay(lua, root_world, actor_world, row)? {
                    tag_new_root_mutations(root_world, cursor, actor_id)?;
                    report.applied += 1;
                } else {
                    report.first_conflict.get_or_insert(index);
                    report.conflicts.push("curve_keys:stale_input".into());
                }
            }
            "model_scale" => {
                let cursor = mutation_rows(root_world);
                if super::movement::replay_scale(lua, root_world, actor_world, row)? {
                    tag_new_root_mutations(root_world, cursor, actor_id)?;
                    report.applied += 1;
                } else {
                    report.first_conflict.get_or_insert(index);
                    report.conflicts.push("model_scale:stale_target".into());
                }
            }
            "bulk_move" => {
                let cursor = mutation_rows(root_world);
                if super::movement::replay_bulk_move(lua, root_world, actor_world, row)? {
                    tag_new_root_mutations(root_world, cursor, actor_id)?;
                    report.applied += 1;
                } else {
                    report.first_conflict.get_or_insert(index);
                    report.conflicts.push("bulk_move:stale_input".into());
                }
            }
            "attachment_pose" => {
                let cursor = mutation_rows(root_world);
                if super::movement::replay_attachment_pose(lua, root_world, actor_world, row)? {
                    tag_new_root_mutations(root_world, cursor, actor_id)?;
                    report.applied += 1;
                } else {
                    report.first_conflict.get_or_insert(index);
                    report.conflicts.push("attachment_pose:stale_input".into());
                }
            }
            "joint_offset" | "joint_target" | "joint_flush" | "joint_rebase"
            | "joint_animation" => {
                let cursor = mutation_rows(root_world);
                if super::movement::replay_joint_pose(lua, root_world, actor_world, row)? {
                    tag_new_root_mutations(root_world, cursor, actor_id)?;
                    report.applied += 1;
                } else {
                    report.first_conflict.get_or_insert(index);
                    report.conflicts.push(format!("{kind}:stale_input"));
                }
            }
            "attribute" => {
                let id = row
                    .get("id")
                    .and_then(Json::as_str)
                    .ok_or_else(|| error("Actor attribute row needs an ID"))?;
                let base = base_revision(row)?;
                if !source_node_matches(actor_world, root_world, id, retiring(id))
                    || !revision_matches(root_world, id, base)
                {
                    report.first_conflict.get_or_insert(index);
                    report.conflicts.push(format!("attribute:{id}"));
                    continue;
                }
                set_attribute(lua, root_world, actor_id, row)?;
                report.applied += 1;
            }
            "set_tags" => {
                let id = row
                    .get("id")
                    .and_then(Json::as_str)
                    .ok_or_else(|| error("Actor tag row needs an ID"))?;
                let base = base_revision(row)?;
                if !source_node_matches(actor_world, root_world, id, retiring(id))
                    || !revision_matches(root_world, id, base)
                {
                    report.first_conflict.get_or_insert(index);
                    report.conflicts.push(format!("tags:{id}"));
                    continue;
                }
                set_tags(lua, root_world, actor_id, row)?;
                report.applied += 1;
            }
            "destroy" => match destroy(lua, root_world, actor_id, row) {
                Ok(()) => report.applied += 1,
                Err(mlua::Error::RuntimeError(message))
                    if message.starts_with("stale Actor destroy mutation") =>
                {
                    report.first_conflict.get_or_insert(index);
                    report.conflicts.push(message);
                }
                Err(error) => return Err(error),
            },
            _ => {
                return Err(error(format!(
                    "unsupported Actor shadow mutation kind {kind}"
                )))
            }
        }
        if report.applied > applied_before {
            if let (Some(id), Some(result)) = (
                row.get("id").and_then(Json::as_str),
                row.get("_result_local_revision").and_then(Json::as_u64),
            ) {
                if let Some(actual) = root_world.borrow().nodes.get(id).map(|node| node.local_revision) {
                    if actual != result { revisions.insert((id.to_owned(), result), actual); }
                    // If this is the fork's final write to this node, adopt the
                    // accepted root fence before refreshing its local overlays.
                    if let Some(node) = actor_world.borrow_mut().nodes.get_mut(id) {
                        if node.local_revision == result { node.local_revision = actual; }
                    }
                }
            }
        }
    }
    if trace {report.mutation_cursors.push(mutation_rows(root_world));}
    // Root remains authoritative after conflicts. This also clears any stale
    // Actor-local property overlay rejected by a revision fence.
    {
        let root = root_world.borrow();
        actor_world.borrow_mut().sync_actor_fork(&root);
    }
    Ok(report)
}

/// Refresh a peer Actor fork and deliver normal local Instance events for the
/// committed journal. External event queues are deliberately not merged here.
pub fn sync_peer(
    lua: &Lua,
    peer_world: &WorldRef,
    root_world: &WorldRef,
    journal: &[Json],
) -> Result<()> {
    world::sync_actor_fork_with_lua(lua, peer_world, root_world, journal)
}

/// Remove internal revision fences before a status row reaches the session
/// response. Root mutations retain actor_origin as their public ownership tag.
pub fn strip_internal_fields(status: &mut Json) {
    let actor_origin = status.get("actor_id").cloned().unwrap_or(Json::Null);
    if let Some(rows) = status.get_mut("mutations").and_then(Json::as_array_mut) {
        for row in rows {
            if let Some(object) = row.as_object_mut() {
                object.remove("_base_local_revision");
                object.remove("_base_local_revisions");
                object.remove("_result_local_revision");
                object.insert("actor_origin".into(), actor_origin.clone());
            }
        }
    }
    if let Some(rows) = status
        .pointer_mut("/world_effects/mutations")
        .and_then(Json::as_array_mut)
    {
        for row in rows {
            if let Some(object) = row.as_object_mut() {
                object.remove("_base_local_revision");
                object.remove("_base_local_revisions");
                object.remove("_result_local_revision");
                object.insert("actor_origin".into(), actor_origin.clone());
            }
        }
    }
}

#[cfg(test)]
#[path = "actor_shadow_tests.rs"]
mod tests;
