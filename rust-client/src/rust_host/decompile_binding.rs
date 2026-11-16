//! Provides actual normalized bytecode for the asynchronous local decompiler.
//! The generated source is returned as text and never executed implicitly.

use super::world::WorldRef;
use mlua::{Lua, Value};
use std::rc::Rc;

fn error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}

pub(super) fn install(lua: &Lua, world: &WorldRef) -> mlua::Result<()> {
    let world = Rc::downgrade(world);
    lua.globals().set(
        "__rove_script_bytecode",
        lua.create_function(move |lua, value: Value| {
            let world = world
                .upgrade()
                .ok_or_else(|| error("runtime world is unavailable"))?;
            let bytes = super::script_bytes::bytes(lua, &world, value, true)?;
            lua.create_string(bytes)
        })?,
    )?;
    Ok(())
}
