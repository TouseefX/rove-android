//! Sparse Mass observers for local edits; receipt notifications are planned in order.
use super::*;
use crate::rust_host::signals;
use std::collections::HashMap;
type Key = (usize, String);
struct Watch {
    world: Weak<RefCell<world::World>>,
    last: Option<f64>,
}
#[derive(Default)]
struct Watches {
    entries: HashMap<Key, Watch>,
    refreshing: bool,
}
#[derive(Clone, Default)]
struct Registry(Rc<RefCell<Watches>>);
const LIMIT: usize = 4096;
fn registry(lua: &Lua) -> Registry {
    if let Some(r) = lua.app_data_ref::<Registry>() {
        return r.clone();
    }
    let r = Registry::default();
    lua.set_app_data(r.clone());
    r
}
fn interested(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<bool> {
    Ok(signals::has_receivers(lua, ctx, id, "Changed")?
        || signals::has_receivers(lua, ctx, id, "property:Mass")?)
}
pub(crate) fn watch_signal(lua: &Lua, ctx: &WorldRef, id: &str, signal: &str) -> mlua::Result<()> {
    if !matches!(signal, "Changed" | "property:Mass") || !world::is_a(ctx, id, "BasePart") {
        return Ok(());
    }
    refresh(lua)?;
    let r = registry(lua);
    let token = (Rc::as_ptr(ctx) as usize, id.to_owned());
    if !r.0.borrow().entries.contains_key(&token) || !interested(lua, ctx, id)? {
        if r.0.borrow().entries.len() >= LIMIT && !r.0.borrow().entries.contains_key(&token) {
            return Err(err("observed Mass limit exceeded"));
        }
        r.0.borrow_mut().entries.insert(
            token,
            Watch {
                world: Rc::downgrade(ctx),
                last: part_mass_properties::read(ctx, id).ok(),
            },
        );
    }
    Ok(())
}
fn sweep(
    lua: &Lua,
    only: Option<usize>,
    ids: Option<&std::collections::HashSet<&str>>,
    notify: bool,
) -> mlua::Result<()> {
    let Some(r) = lua.app_data_ref::<Registry>().map(|r| r.clone()) else {
        return Ok(());
    };
    let entries = {
        let mut state = r.0.borrow_mut();
        if state.refreshing || state.entries.is_empty() {
            return Ok(());
        }
        state.refreshing = true;
        state
            .entries
            .iter()
            .filter(|(k, _)| {
                only.is_none_or(|id| k.0 == id) && ids.is_none_or(|ids| ids.contains(k.1.as_str()))
            })
            .map(|(k, w)| (k.clone(), w.world.clone()))
            .collect::<Vec<_>>()
    };
    let result = (|| {
        let mut changed = Vec::new();
        for (key, weak) in entries {
            let Some(ctx) = weak.upgrade() else {
                r.0.borrow_mut().entries.remove(&key);
                continue;
            };
            if !ctx.borrow().nodes.is_live(&key.1) || !interested(lua, &ctx, &key.1)? {
                r.0.borrow_mut().entries.remove(&key);
                continue;
            }
            let next = part_mass_properties::read(&ctx, &key.1).ok();
            if let Some(watch) = r.0.borrow_mut().entries.get_mut(&key) {
                if notify && next.is_some() && next != watch.last {
                    changed.push((ctx.clone(), key.1.clone()));
                }
                watch.last = next;
            }
        }
        for (ctx, id) in changed {
            world::emit_property_changed_direct(lua, &ctx, &id, "Mass")?;
        }
        Ok(())
    })();
    r.0.borrow_mut().refreshing = false;
    result
}
pub(crate) fn refresh(lua: &Lua) -> mlua::Result<()> {
    sweep(lua, None, None, true)
}
/// Ordered Mass notifications already cover these parts. An equal direct input
/// must not hide an external material change in the same committed batch.
pub(crate) fn synchronize(
    lua: &Lua,
    ctx: &WorldRef,
    planned: &crate::rust_host::property_events::Receipt<'_>,
) -> mlua::Result<()> {
    if lua.app_data_ref::<Registry>().is_none() {
        return Ok(());
    }
    let ids = planned.mass_ids();
    sweep(lua, Some(Rc::as_ptr(ctx) as usize), Some(&ids), false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn observers_release_disconnected_and_dropped_worlds() {
        let lua = Lua::new();
        let ctx = Rc::new(RefCell::new(
            world::World::from_snapshot(
                &json!({
                    "version":1,"sequence":0,"nodes":[{"id":"p","class_name":"Part","properties":{
                        "Size":{"$vector3":[2,2,2]},"Shape":{"$enum":{"type":"PartType","value":1}},
                        "CustomPhysicalProperties":{"$physical_properties":[1,0.3,0.5,1,1]}
                    }}]
                }),
                None,
            )
            .unwrap(),
        ));
        signals::install(&lua, &ctx).unwrap();
        let connect = || {
            signals::connect_owned(
                &lua,
                &ctx,
                "p",
                "property:Mass",
                lua.create_function(|_, (): ()| Ok(())).unwrap(),
                "watch",
                false,
            )
            .unwrap()
        };
        connect();
        refresh(&lua).unwrap();
        assert_eq!(registry(&lua).0.borrow().entries.len(), 1);
        signals::disconnect_owned(&lua, &ctx, "p", "property:Mass", "watch").unwrap();
        refresh(&lua).unwrap();
        assert!(registry(&lua).0.borrow().entries.is_empty());
        connect();
        drop(ctx);
        refresh(&lua).unwrap();
        assert!(registry(&lua).0.borrow().entries.is_empty());
    }
}
