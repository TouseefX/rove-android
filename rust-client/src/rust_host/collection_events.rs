//! Collection membership notifications without a second persistent world index.
use super::{
    signals,
    world::{self, WorldRef},
};
use mlua::{Lua, MultiValue};
use serde_json::Value as Json;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(super) struct Receipt(HashMap<usize, Vec<(String, String, bool)>>);
impl Receipt {
    pub(super) fn publish(&mut self, lua: &Lua, ctx: &WorldRef, index: usize) -> mlua::Result<()> {
        for (id, tag, added) in self.0.remove(&index).unwrap_or_default() {
            fire(lua, ctx, &id, &tag, added)?;
        }
        Ok(())
    }
}
fn fire(lua: &Lua, ctx: &WorldRef, id: &str, tag: &str, added: bool) -> mlua::Result<()> {
    let Some(service) = ctx.borrow().services.get("CollectionService").cloned() else {
        return Ok(());
    };
    let event = format!("tag-{}:{tag}", if added { "added" } else { "removed" });
    if signals::has_receivers(lua, ctx, &service, &event)? {
        signals::fire(
            lua,
            ctx,
            &service,
            &event,
            MultiValue::from_vec(vec![world::instance(lua, ctx, id)?]),
        )?;
    }
    Ok(())
}
fn reference(value: &Json) -> Option<String> {
    value
        .as_str()
        .or_else(|| value.get("$ref").and_then(Json::as_str))
        .map(str::to_owned)
}
fn parent_field(action: &Json) -> Option<&Json> {
    if action
        .get("property")
        .or_else(|| action.get("key"))
        .and_then(Json::as_str)
        == Some("Parent")
    {
        action.get("value")
    } else {
        action.get("fields").and_then(|fields| fields.get("Parent"))
    }
}
fn tags(value: &Json) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Json::as_str)
        .map(str::to_owned)
        .collect()
}
pub(super) fn relevant(action: &Json) -> bool {
    match action["kind"].as_str() {
        Some("create" | "remove" | "destroy" | "set_tags") => true,
        Some("set" | "set_property") => {
            parent_field(action).is_some() || action.get("tag_snapshot").is_some()
        }
        _ => false,
    }
}
pub(super) fn has_changes(actions: &[Json], previous: u64) -> bool {
    actions
        .iter()
        .any(|a| a["sequence"].as_u64().unwrap_or(0) > previous && relevant(a))
}

pub(super) fn destroying(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<()> {
    let action = serde_json::json!({"kind":"destroy","id":id});
    prepare(ctx, &[action], None).publish(lua, ctx, 0)
}

// Only changed ancestry is overlaid. Walking a received parent uses headers and
// must not materialize property wrappers for every ancestor or sibling.
fn under(ctx: &WorldRef, parents: &HashMap<String, Option<String>>, id: &str, root: &str) -> bool {
    let world = ctx.borrow();
    let mut current = Some(id.to_owned());
    let mut remaining = world
        .nodes
        .len()
        .saturating_add(parents.len())
        .saturating_add(1);
    while let Some(id) = current {
        if id == root {
            return true;
        }
        if remaining == 0 {
            return false;
        }
        remaining -= 1;
        current = parents
            .get(&id)
            .cloned()
            .unwrap_or_else(|| world.nodes.live_parent(&id));
    }
    false
}
fn tagged(ctx: &WorldRef) -> HashMap<String, Vec<String>> {
    // Nonempty tags live in NodeData, so tagged records are already materialized.
    ctx.borrow()
        .nodes
        .materialized_values()
        .filter(|node| {
            !node.destroyed && node.collection_tags_known && !node.collection_tags.is_empty()
        })
        .map(|node| (node.id.clone(), node.collection_tags.clone()))
        .collect()
}
fn root(ctx: &WorldRef) -> Option<String> {
    ctx.borrow()
        .nodes
        .first_live_of_class("DataModel")
        .map(|node| node.id.clone())
}
fn visible(
    ctx: &WorldRef,
    parents: &HashMap<String, Option<String>>,
    root: &str,
    tags: &HashMap<String, Vec<String>>,
) -> HashSet<(String, String)> {
    tags.iter()
        .filter(|(id, _)| id.as_str() != root && under(ctx, parents, id, root))
        .flat_map(|(id, tags)| tags.iter().map(move |tag| (id.clone(), tag.clone())))
        .collect()
}

/// Plan ordered notifications against the old world; publish only after the
/// actual transaction validates. Stale receipts and property-only movement do
/// not enumerate tags, and rejected transactions leave no retained state.
pub(super) fn prepare(ctx: &WorldRef, actions: &[Json], previous: Option<u64>) -> Receipt {
    let fresh = |row: &Json| previous.is_none_or(|seq| row["sequence"].as_u64().unwrap_or(0) > seq);
    let mut receipt = Receipt::default();
    if !ctx.borrow().services.contains_key("CollectionService")
        || !actions.iter().any(|a| fresh(a) && relevant(a))
    {
        return receipt;
    }
    let Some(root) = root(ctx) else {
        return receipt;
    };
    let mut tagged = tagged(ctx);
    let mut parents = HashMap::new();
    let mut prior = visible(ctx, &parents, &root, &tagged);
    for (index, action) in actions
        .iter()
        .enumerate()
        .filter(|(_, a)| fresh(a) && relevant(a))
    {
        let id = action["id"]
            .as_str()
            .or_else(|| action["node"]["id"].as_str())
            .unwrap_or_default();
        match action["kind"].as_str() {
            Some("create") => {
                parents.insert(id.to_owned(), reference(&action["node"]["parent"]));
                tagged.remove(id);
                if action["node"]["collection_tags_known"].as_bool() == Some(true) {
                    let values = tags(&action["node"]["collection_tags"]);
                    if !values.is_empty() {
                        tagged.insert(id.to_owned(), values);
                    }
                }
            }
            Some("set" | "set_property") => {
                if let Some(parent) = parent_field(action) {
                    parents.insert(id.to_owned(), reference(parent));
                }
                if let Some(snapshot) = action.get("tag_snapshot") {
                    tagged.remove(id);
                    if snapshot["collection_tags_known"].as_bool().unwrap_or(true) {
                        let values = tags(&snapshot["collection_tags"]);
                        if !values.is_empty() {
                            tagged.insert(id.to_owned(), values);
                        }
                    }
                }
            }
            Some("set_tags") => {
                tagged.remove(id);
                let values = tags(&action["tags"]);
                if !values.is_empty() {
                    tagged.insert(id.to_owned(), values);
                }
            }
            Some("remove" | "destroy") => {
                let roots = if let Some(id) = action["id"].as_str() {
                    vec![id]
                } else {
                    action["ids"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Json::as_str)
                        .collect()
                };
                tagged.retain(|id, _| !roots.iter().any(|root| under(ctx, &parents, id, root)));
                for id in roots {
                    parents.insert(id.to_owned(), None);
                }
            }
            _ => {}
        }
        let next = visible(ctx, &parents, &root, &tagged);
        let mut events: Vec<_> = prior
            .difference(&next)
            .map(|(id, tag)| (id.clone(), tag.clone(), false))
            .chain(
                next.difference(&prior)
                    .map(|(id, tag)| (id.clone(), tag.clone(), true)),
            )
            .collect();
        events.sort();
        if !events.is_empty() {
            receipt.0.insert(index, events);
        }
        prior = next;
    }
    receipt
}

/// Local parent writes have already committed, but the old parent is supplied
/// by the caller. Moving inside the same DataModel changes no tag membership.
pub(super) fn parent_changed(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    old: Option<String>,
    new: Option<String>,
) -> mlua::Result<()> {
    if old == new || !ctx.borrow().services.contains_key("CollectionService") {
        return Ok(());
    }
    let Some(root) = root(ctx) else { return Ok(()) };
    let mut parents = HashMap::new();
    parents.insert(id.to_owned(), old);
    let was_inside = id != root && under(ctx, &parents, id, &root);
    parents.insert(id.to_owned(), new);
    let now_inside = id != root && under(ctx, &parents, id, &root);
    if was_inside == now_inside {
        return Ok(());
    }
    let mut selected: Vec<_> = tagged(ctx)
        .into_iter()
        .filter(|(candidate, _)| under(ctx, &parents, candidate, id))
        .collect();
    selected.sort_by(|a, b| a.0.cmp(&b.0));
    for (candidate, tags) in selected {
        for tag in tags {
            fire(lua, ctx, &candidate, &tag, now_inside)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine_world_projection::ProjectionContext, engine_world_view::SharedWorldView};
    use rove_client_core::{schema::Schema, world::WorldStore};
    use serde_json::json;
    use std::{cell::RefCell, rc::Rc};

    #[test]
    fn membership_planning_keeps_unrelated_shared_records_unmaterialized() {
        let mut nodes = vec![
            json!({"id":"game","class_name":"DataModel","name":"game"}),
            json!({"id":"folder","class_name":"Folder","name":"folder","parent":null}),
            json!({"id":"collection","class_name":"CollectionService","name":"CollectionService","parent":"game"}),
        ];
        for index in 0..1000 {
            nodes.push(json!({"id":format!("p{index}"),"class_name":"Part","name":"part","parent":"folder"}));
        }
        let store =
            WorldStore::from_snapshot(&json!({"version":1,"sequence":0,"nodes":nodes})).unwrap();
        let schema = Schema {
            version: 3,
            flags: 0,
            enums: vec![],
            classes: vec![],
            properties: vec![],
            events: vec![],
            strings: vec![],
            names: vec![],
            lr_types: vec![],
            parent_property_id: 0,
            decoded_bytes: 0,
        };
        let view = SharedWorldView::projected(
            store.view(),
            ProjectionContext::new(&schema, HashMap::new()).unwrap(),
        );
        let ctx=Rc::new(RefCell::new(world::World::from_snapshot(&json!({"version":1,"sequence":0,"nodes":[],"services":{"CollectionService":"collection"}}),Some(&view)).unwrap()));
        {
            let mut world = ctx.borrow_mut();
            let part = world.nodes.get_mut("p0").unwrap();
            part.collection_tags_known = true;
            part.collection_tags = vec!["watch".into()];
        }
        let count = ctx.borrow().nodes.materialized_count();
        let movement = prepare(
            &ctx,
            &[json!({"kind":"set","sequence":1,"id":"p1","fields":{"Position":[1,2,3]}})],
            Some(0),
        );
        assert!(movement.0.is_empty());
        assert_eq!(ctx.borrow().nodes.materialized_count(), count);
        let receipt = prepare(
            &ctx,
            &[json!({"kind":"set","sequence":1,"id":"folder","fields":{"Parent":{"$ref":"game"}}})],
            Some(0),
        );
        assert_eq!(receipt.0[&0], vec![("p0".into(), "watch".into(), true)]);
        // Only the DataModel header may be materialized by root lookup.
        assert!(ctx.borrow().nodes.materialized_count() <= count + 1);
    }
}
