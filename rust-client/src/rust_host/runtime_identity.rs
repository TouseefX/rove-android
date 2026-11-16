//! Identity and UI access for this process's own runtime.
use super::world::{self, WorldRef};
use mlua::Lua;
use std::rc::Rc;

pub(super) fn install(lua: &Lua, world: &WorldRef) -> mlua::Result<()> {
    let globals = lua.globals();
    let identify = lua.create_function(|_, ()| Ok((crate::EXECUTOR_NAME, crate::EXECUTOR_VERSION)))?;
    globals.set("identifyexecutor", identify.clone())?;
    globals.set("getexecutorname", identify)?;
    let world = Rc::downgrade(world);
    globals.set(
        "gethui",
        lua.create_function(move |lua, ()| {
            let world = world
                .upgrade()
                .ok_or_else(|| mlua::Error::RuntimeError("runtime world is unavailable".into()))?;
            world::ensure_client_service(lua, &world, "CoreGui")
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    fn setup() -> (Lua, WorldRef) {
        let world = Rc::new(RefCell::new(world::World::from_snapshot(&json!({
            "version":1,"sequence":0,"nodes":[{"id":"game","class_name":"DataModel","name":"Game","properties":{}}]
        }), None).unwrap()));
        let lua = Lua::new();
        super::super::install_bindings(&lua, &world).unwrap();
        (lua, world)
    }

    #[test]
    fn identity_reports_this_runtime_and_alias_retains_both_returns() {
        let (lua, _world) = setup();
        let (name, version, alias): (String, String, bool) = lua.load(
            "local name, version = getexecutorname(); return name, version, identifyexecutor == getexecutorname"
        ).eval().unwrap();
        assert_eq!(name, "rove");
        assert_eq!(version, "1.00");
        assert_eq!(crate::EXECUTOR_USER_AGENT, format!("{name} {version}"));
        assert!(alias);
    }

    #[test]
    fn ui_root_is_stable_accepts_real_children_and_is_session_local() {
        let (lua, _world) = setup();
        assert!(lua
            .load(
                r#"
            local root = gethui()
            assert(root == gethui() and root:IsA("CoreGui"))
            local screen = Instance.new("ScreenGui")
            screen.Name = "CompatibilityPanel"
            screen.Parent = root
            return root:FindFirstChild("CompatibilityPanel") == screen
                and screen:IsDescendantOf(game)
        "#
            )
            .eval::<bool>()
            .unwrap());
        let (other, _other_world) = setup();
        assert!(other
            .load("return gethui():FindFirstChild('CompatibilityPanel') == nil")
            .eval::<bool>()
            .unwrap());
    }
}
