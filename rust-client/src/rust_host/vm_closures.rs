//! Safe entry points for the small, layout-sensitive adapter compiled against
//! this checkout's pinned official Luau VM headers.

use mlua::{Function, Lua};

unsafe extern "C" {
    fn rove_vm_closures_install(state: *mut mlua::ffi::lua_State);
    fn rove_vm_closures_mark(state: *mut mlua::ffi::lua_State, index: i32);
    fn rove_vm_coroutines_wrap(state: *mut mlua::ffi::lua_State);
}

pub(super) fn install(lua: &Lua) -> mlua::Result<()> {
    unsafe {
        lua.exec_raw::<()>((), |state| rove_vm_closures_install(state))?;
    }
    Ok(())
}

pub(super) fn wrap_coroutines(lua: &Lua) -> mlua::Result<()> {
    unsafe {
        lua.exec_raw::<()>((), |state| rove_vm_coroutines_wrap(state))?;
    }
    Ok(())
}

pub(super) fn mark(lua: &Lua, function: &Function) -> mlua::Result<()> {
    unsafe {
        lua.exec_raw::<()>((function,), |state| rove_vm_closures_mark(state, -1))?;
    }
    Ok(())
}
