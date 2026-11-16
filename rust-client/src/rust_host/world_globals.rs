//! Root selection is stable across property-only receipts. Keep just its ID,
//! not a copied world or a strong reference that outlives the VM's world.
use super::{instance, World, WorldRef};
use mlua::{Lua, Value};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::{Rc, Weak},
};

struct Root {
    owner: Weak<RefCell<World>>,
    id: Option<String>,
    sequence: u64,
    nodes: usize,
}
#[derive(Default)]
struct Roots(HashMap<usize, Root>);

pub(super) fn refresh(
    lua: &Lua,
    ctx: &WorldRef,
    previous: Option<u64>,
    structural: bool,
) -> mlua::Result<Option<String>> {
    let key = Rc::as_ptr(ctx) as usize;
    let world = ctx.borrow();
    let cached = lua.app_data_ref::<Roots>().and_then(|roots| {
        let root = roots.0.get(&key)?;
        if structural
            || previous != Some(root.sequence)
            || root.nodes != world.nodes.len()
            || !root
                .owner
                .upgrade()
                .is_some_and(|owner| Rc::ptr_eq(&owner, ctx))
        {
            return None;
        }
        if root.id.as_ref().is_some_and(|id| {
            world
                .nodes
                .get(id)
                .is_none_or(|n| n.destroyed || n.class_name != "DataModel")
        }) {
            return None;
        }
        Some(root.id.clone())
    });
    let game = cached.unwrap_or_else(|| {
        world
            .node_order
            .iter()
            .filter(|id| {
                world.nodes.class_name(id) == Some("DataModel")
                    && world.nodes.get(id).is_some_and(|n| !n.destroyed)
            })
            .min_by_key(|id| id.as_ref() == "__world_snapshot_datamodel__")
            .map(|id| id.to_string())
    });
    let workspace = world.services.get("Workspace").cloned();
    let root = Root {
        owner: Rc::downgrade(ctx),
        id: game.clone(),
        sequence: world.sequence,
        nodes: world.nodes.len(),
    };
    drop(world);
    if lua.app_data_ref::<Roots>().is_none() {
        lua.set_app_data(Roots::default());
    }
    {
        let mut roots = lua.app_data_mut::<Roots>().unwrap();
        roots.0.retain(|_, root| root.owner.strong_count() > 0);
        roots.0.insert(key, root);
    }
    let globals = lua.globals();
    globals.set(
        "game",
        match &game {
            Some(id) => instance(lua, ctx, id)?,
            None => Value::Nil,
        },
    )?;
    globals.set(
        "workspace",
        match workspace {
            Some(id) => instance(lua, ctx, &id)?,
            None => Value::Nil,
        },
    )?;
    Ok(game)
}
