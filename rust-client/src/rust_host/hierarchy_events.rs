//! Ordered hierarchy notifications planned from headers before a transaction.
use super::{
    signals,
    world::{self, World, WorldRef},
};
use mlua::{Lua, MultiValue, Value};
use serde_json::Value as Json;
use std::collections::{HashMap, HashSet};

const NAMES: &[&str] = &[
    "ChildAdded",
    "ChildRemoved",
    "DescendantAdded",
    "DescendantRemoving",
    "AncestryChanged",
    "Destroying",
];
type Observed = HashMap<String, HashSet<String>>;
fn listens(observed: &Observed, event: &str, id: &str) -> bool {
    observed.get(event).is_some_and(|ids| ids.contains(id))
}
#[derive(Default)]
pub(super) struct Receipt(HashMap<usize, Vec<Change>>);
impl Receipt {
    pub(super) fn publish(
        &mut self,
        lua: &Lua,
        ctx: &WorldRef,
        index: usize,
    ) -> mlua::Result<Vec<String>> {
        let mut destroyed = Vec::new();
        for change in self.0.remove(&index).unwrap_or_default() {
            change.publish(lua, ctx)?;
            if change.destroy {
                destroyed.push(change.id);
            }
        }
        Ok(destroyed)
    }
}
struct Change {
    id: String,
    old: Option<String>,
    new: Option<String>,
    removed_from: Vec<String>,
    added_to: Vec<String>,
    descendants: Vec<String>,
    ancestry: Vec<String>,
    child_removed: bool,
    child_added: bool,
    destroy: bool,
    destroying: bool,
}
fn fire(
    lua: &Lua,
    ctx: &WorldRef,
    source: &str,
    event: &str,
    ids: &[Option<&str>],
) -> mlua::Result<()> {
    let mut args = MultiValue::new();
    for id in ids {
        args.push_back(match id {
            Some(id) => world::instance(lua, ctx, id)?,
            None => Value::Nil,
        });
    }
    signals::fire(lua, ctx, source, event, args)?;
    Ok(())
}
impl Change {
    fn publish(&self, lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
        if self.destroying {
            fire(lua, ctx, &self.id, "Destroying", &[])?;
        }
        if self.old == self.new {
            return Ok(());
        }
        for id in &self.descendants {
            for ancestor in &self.removed_from {
                fire(lua, ctx, ancestor, "DescendantRemoving", &[Some(id)])?;
            }
        }
        if self.child_removed {
            fire(
                lua,
                ctx,
                self.old.as_deref().unwrap(),
                "ChildRemoved",
                &[Some(&self.id)],
            )?;
        }
        world::emit_property_changed_direct(lua, ctx, &self.id, "Parent")?;
        for receiver in &self.ancestry {
            fire(
                lua,
                ctx,
                receiver,
                "AncestryChanged",
                &[Some(&self.id), self.new.as_deref()],
            )?;
        }
        if self.child_added {
            fire(
                lua,
                ctx,
                self.new.as_deref().unwrap(),
                "ChildAdded",
                &[Some(&self.id)],
            )?;
        }
        for id in &self.descendants {
            for ancestor in &self.added_to {
                fire(lua, ctx, ancestor, "DescendantAdded", &[Some(id)])?;
            }
        }
        Ok(())
    }
}
struct Graph<'a> {
    world: &'a World,
    parents: HashMap<String, Option<String>>,
    changed: Vec<String>,
    created: HashSet<String>,
    dead: HashSet<String>,
    children: Option<HashMap<String, Vec<&'a str>>>,
}
impl<'a> Graph<'a> {
    fn new(world: &'a World) -> Self {
        Self {
            world,
            parents: HashMap::new(),
            changed: Vec::new(),
            created: HashSet::new(),
            dead: HashSet::new(),
            children: None,
        }
    }
    fn live(&self, id: &str) -> bool {
        !self.dead.contains(id) && (self.created.contains(id) || self.world.nodes.is_live(id))
    }
    fn parent(&self, id: &str) -> Option<String> {
        self.parents
            .get(id)
            .cloned()
            .unwrap_or_else(|| self.world.nodes.parent(id).flatten())
    }
    fn set_parent(&mut self, id: &str, parent: Option<String>) {
        if !self.parents.contains_key(id) {
            self.changed.push(id.to_owned());
        }
        self.parents.insert(id.to_owned(), parent);
    }
    fn ancestors(&self, start: Option<String>) -> Vec<String> {
        let mut current = start;
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        while let Some(id) = current {
            if !seen.insert(id.clone())
                || (!self.created.contains(&id) && self.world.nodes.class_name(&id).is_none())
            {
                break;
            }
            current = self.parent(&id);
            result.push(id);
        }
        result
    }
    fn under(&self, id: &str, root: &str) -> bool {
        id == root || self.ancestors(self.parent(id)).iter().any(|id| id == root)
    }
    fn subtree(&mut self, root: &str) -> mlua::Result<Vec<String>> {
        if self.children.is_none() {
            let mut children = HashMap::<String, Vec<&str>>::new();
            for id in self.world.header_order() {
                if let Some(parent) = self.world.nodes.live_parent(id) {
                    children.entry(parent).or_default().push(id);
                }
            }
            self.children = Some(children);
        }
        let mut pending = vec![root.to_owned()];
        let mut selected = Vec::new();
        let mut seen = HashSet::new();
        while let Some(id) = pending.pop() {
            if !self.live(&id) || !seen.insert(id.clone()) {
                continue;
            }
            let mut children = self
                .children
                .as_ref()
                .unwrap()
                .get(&id)
                .into_iter()
                .flatten()
                .filter(|child| {
                    self.live(child) && self.parent(child).as_deref() == Some(id.as_str())
                })
                .map(|child| (*child).to_owned())
                .collect::<Vec<_>>();
            for child in &self.changed {
                if self.live(child)
                    && self.parent(child).as_deref() == Some(id.as_str())
                    && !children.contains(child)
                {
                    children.push(child.clone());
                }
            }
            pending.extend(children.into_iter().rev());
            selected.push(id);
            if selected.len() > 100_000 {
                return Err(mlua::Error::RuntimeError(
                    "hierarchy subtree limit exceeded".into(),
                ));
            }
        }
        Ok(selected)
    }
    fn move_node(
        &mut self,
        observed: &Observed,
        id: &str,
        new: Option<String>,
        destroy: bool,
    ) -> mlua::Result<Option<Change>> {
        if !self.live(id) {
            return Ok(None);
        }
        let old = self.parent(id);
        if old == new && !destroy {
            return Ok(None);
        }
        let old_chain = self.ancestors(old.clone());
        let new_chain = self.ancestors(new.clone());
        let removed_from = old_chain
            .iter()
            .filter(|id| !new_chain.contains(id) && listens(observed, "DescendantRemoving", id))
            .cloned()
            .collect::<Vec<_>>();
        let added_to = new_chain
            .iter()
            .filter(|id| !old_chain.contains(id) && listens(observed, "DescendantAdded", id))
            .cloned()
            .collect::<Vec<_>>();
        let descendants = if removed_from.is_empty() && added_to.is_empty() {
            Vec::new()
        } else {
            self.subtree(id)?
        };
        let mut ancestry = observed
            .get("AncestryChanged")
            .into_iter()
            .flatten()
            .filter(|candidate| self.live(candidate) && self.under(candidate, id))
            .cloned()
            .collect::<Vec<_>>();
        ancestry.sort();
        let change = Change {
            id: id.to_owned(),
            child_removed: old
                .as_deref()
                .is_some_and(|id| listens(observed, "ChildRemoved", id)),
            child_added: new
                .as_deref()
                .is_some_and(|id| listens(observed, "ChildAdded", id)),
            old,
            new: new.clone(),
            removed_from,
            added_to,
            descendants,
            ancestry,
            destroy,
            destroying: destroy && listens(observed, "Destroying", id),
        };
        self.set_parent(id, new);
        Ok(Some(change))
    }
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
fn relevant(action: &Json) -> bool {
    match action["kind"].as_str() {
        Some("create" | "destroy" | "remove") => true,
        Some("set" | "set_property") => parent_field(action).is_some(),
        _ => false,
    }
}
pub(super) fn prepare(
    lua: &Lua,
    ctx: &WorldRef,
    actions: &[Json],
    previous: Option<u64>,
) -> mlua::Result<Receipt> {
    let fresh = |a: &Json| previous.is_none_or(|seq| a["sequence"].as_u64().unwrap_or(0) > seq);
    let mut receipt = Receipt::default();
    if !actions.iter().any(|a| fresh(a) && relevant(a)) {
        return Ok(receipt);
    }
    let observed = signals::observed_ids(lua, ctx, NAMES);
    let world = ctx.borrow();
    let mut graph = Graph::new(&world);
    for (index, action) in actions
        .iter()
        .enumerate()
        .filter(|(_, a)| fresh(a) && relevant(a))
    {
        let id = action["id"]
            .as_str()
            .or_else(|| action["node"]["id"].as_str())
            .unwrap_or_default();
        let mut changes = Vec::new();
        match action["kind"].as_str() {
            Some("create") => {
                if !graph.live(id) {
                    graph.set_parent(id, None);
                }
                graph.created.insert(id.to_owned());
                graph.dead.remove(id);
                if let Some(change) =
                    graph.move_node(&observed, id, reference(&action["node"]["parent"]), false)?
                {
                    changes.push(change);
                }
            }
            Some("set" | "set_property") => {
                if let Some(value) = parent_field(action) {
                    if let Some(change) = graph.move_node(&observed, id, reference(value), false)? {
                        changes.push(change);
                    }
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
                        .collect::<Vec<_>>()
                };
                let top = roots
                    .iter()
                    .copied()
                    .filter(|id| {
                        !roots
                            .iter()
                            .any(|other| id != other && graph.under(id, other))
                    })
                    .collect::<Vec<_>>();
                for root in top {
                    for id in graph.subtree(root)? {
                        if let Some(change) = graph.move_node(&observed, &id, None, true)? {
                            changes.push(change);
                        }
                        graph.dead.insert(id);
                    }
                }
            }
            _ => {}
        }
        if !changes.is_empty() {
            receipt.0.insert(index, changes);
        }
    }
    Ok(receipt)
}
pub(super) fn local_parent(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    old: Option<String>,
    new: Option<String>,
) -> mlua::Result<()> {
    if old == new {
        return Ok(());
    }
    let observed = signals::observed_ids(lua, ctx, NAMES);
    let change = {
        let world = ctx.borrow();
        let mut graph = Graph::new(&world);
        graph.set_parent(id, old);
        graph.move_node(&observed, id, new, false)?
    };
    if let Some(change) = change {
        change.publish(lua, ctx)?;
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
    fn hierarchy_planning_keeps_received_property_wrappers_lazy() {
        let mut nodes = vec![
            json!({"id":"game","class_name":"DataModel","name":"game"}),
            json!({"id":"folder","class_name":"Folder","name":"folder"}),
        ];
        for i in 0..1000 {
            nodes.push(
                json!({"id":format!("p{i}"),"class_name":"Part","name":"part","parent":"folder"}),
            );
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
        let ctx = Rc::new(RefCell::new(
            World::from_snapshot(&json!({"version":1,"sequence":0,"nodes":[]}), Some(&view))
                .unwrap(),
        ));
        let lua = Lua::new();
        let scheduler = Rc::new(RefCell::new(super::super::scheduler::Scheduler::new()));
        super::super::scheduler::install(&lua, &scheduler).unwrap();
        world::install(&lua, &ctx).unwrap();
        lua.load("game.DescendantAdded:Connect(function() end)")
            .exec()
            .unwrap();
        let before = ctx.borrow().nodes.materialized_count();
        let movement = prepare(
            &lua,
            &ctx,
            &[json!({"kind":"set","sequence":1,"id":"p0","fields":{"Position":[1,2,3]}})],
            Some(0),
        )
        .unwrap();
        assert!(movement.0.is_empty());
        assert_eq!(ctx.borrow().nodes.materialized_count(), before);
        let hierarchy = prepare(
            &lua,
            &ctx,
            &[json!({"kind":"set","sequence":1,"id":"folder","fields":{"Parent":{"$ref":"game"}}})],
            Some(0),
        )
        .unwrap();
        assert_eq!(hierarchy.0[&0][0].descendants.len(), 1001);
        assert_eq!(ctx.borrow().nodes.materialized_count(), before);
    }
}
