//! Script-owner metadata for scheduler-managed threads.
//!
//! The coroutine's environment is mutable Luau state, so it is not a reliable
//! source of its creator once a script changes its global `script` value. The
//! scheduler keeps the original Instance handle in its weak-key context table.
//! Active source-owned tasks are also visible as direct values in the real VM
//! registry; the entry is removed as soon as the coroutine finishes.

use super::scheduler;
use mlua::{Function, Lua, Table, Thread, Value};
use std::{cell::RefCell, collections::HashSet, ffi::c_void};

#[derive(Default)]
struct ManagedThreadKeys(RefCell<HashSet<usize>>);

/// Script identity for ordinary Lua coroutines created by a script-owned
/// scheduler task. Weak thread keys keep this metadata from extending a
/// coroutine's lifetime after user code drops it.
struct CoroutineOwners {
    owners: Table,
    started: Table,
}

fn thread_key(thread: &Thread) -> usize {
    thread.to_pointer() as usize
}

/// Retain an active source-owned scheduler thread as a real registry value.
/// Registry entries are bounded by the scheduler's active task limit and are
/// released on completion, error, timeout, or cancellation.
pub(crate) fn retain(lua: &Lua, thread: &Thread) -> mlua::Result<()> {
    let key = thread_key(thread);
    unsafe {
        lua.exec_raw::<()>(thread.clone(), |state| {
            mlua::ffi::lua_pushlightuserdata(state, key as *mut c_void);
            mlua::ffi::lua_pushvalue(state, -2);
            mlua::ffi::lua_rawset(state, mlua::ffi::LUA_REGISTRYINDEX);
        })?;
    }
    if lua.app_data_ref::<ManagedThreadKeys>().is_none() {
        lua.set_app_data(ManagedThreadKeys::default());
    }
    if let Some(keys) = lua.app_data_ref::<ManagedThreadKeys>() {
        keys.0.borrow_mut().insert(key);
    }
    Ok(())
}

fn remove(lua: &Lua, key: usize) -> mlua::Result<()> {
    unsafe {
        lua.exec_raw::<()>((), |state| {
            mlua::ffi::lua_pushlightuserdata(state, key as *mut c_void);
            mlua::ffi::lua_pushnil(state);
            mlua::ffi::lua_rawset(state, mlua::ffi::LUA_REGISTRYINDEX);
        })?;
    }
    if let Some(keys) = lua.app_data_ref::<ManagedThreadKeys>() {
        keys.0.borrow_mut().remove(&key);
    }
    Ok(())
}

/// Drop completed or closed coroutines from the real registry so script
/// inspection does not retain every historical task for the life of the VM.
pub(crate) fn cleanup_finished(lua: &Lua) -> mlua::Result<()> {
    let keys = lua
        .app_data_ref::<ManagedThreadKeys>()
        .map(|keys| keys.0.borrow().iter().copied().collect::<Vec<_>>())
        .unwrap_or_default();
    let mut finished = Vec::new();
    for key in keys {
        let value = unsafe {
            lua.exec_raw::<Value>((), |state| {
                mlua::ffi::lua_pushlightuserdata(state, key as *mut c_void);
                mlua::ffi::lua_rawget(state, mlua::ffi::LUA_REGISTRYINDEX);
            })?
        };
        match value {
            Value::Thread(thread)
                if matches!(
                    thread.status(),
                    mlua::thread::ThreadStatus::Finished | mlua::thread::ThreadStatus::Error
                ) =>
            {
                finished.push(key)
            }
            Value::Nil => finished.push(key),
            _ => {}
        }
    }
    for key in finished {
        remove(lua, key)?;
    }
    Ok(())
}

fn script_for_thread(lua: &Lua, thread: &Thread) -> Option<Value> {
    scheduler::script_for_thread(lua, thread).or_else(|| {
        lua.app_data_ref::<CoroutineOwners>()
            .and_then(|owners| owners.owners.get::<Value>(thread.clone()).ok())
            .filter(|value| !value.is_nil())
    })
}

fn remember_coroutine_owner(lua: &Lua, child: &Thread, parent: &Thread) -> mlua::Result<()> {
    let Some(owner) = script_for_thread(lua, parent) else {
        return Ok(());
    };
    if let Some(owners) = lua.app_data_ref::<CoroutineOwners>() {
        owners.owners.set(child.clone(), owner)?;
    }
    Ok(())
}

pub(crate) fn running_script_ids(lua: &Lua) -> mlua::Result<HashSet<String>> {
    let mut seen = HashSet::new();
    let mut append = |owner: Value| {
        if owner.is_nil() {
            return;
        }
        if let Some(id) = super::world::instance_id(&owner) {
            seen.insert(id);
        }
    };

    let keys = lua
        .app_data_ref::<ManagedThreadKeys>()
        .map(|keys| keys.0.borrow().iter().copied().collect::<Vec<_>>())
        .unwrap_or_default();
    for key in keys {
        let value = unsafe {
            lua.exec_raw::<Value>((), |state| {
                mlua::ffi::lua_pushlightuserdata(state, key as *mut c_void);
                mlua::ffi::lua_rawget(state, mlua::ffi::LUA_REGISTRYINDEX);
            })?
        };
        if let Value::Thread(thread) = value {
            if !matches!(
                thread.status(),
                mlua::thread::ThreadStatus::Finished | mlua::thread::ThreadStatus::Error
            ) {
                if let Some(owner) = script_for_thread(lua, &thread) {
                    append(owner);
                }
            }
        }
    }

    if let Some(owners) = lua.app_data_ref::<CoroutineOwners>() {
        for row in owners.owners.pairs::<Thread, Value>() {
            let (thread, owner) = row?;
            if !owners.started.get::<bool>(thread.clone()).unwrap_or(false) {
                continue;
            }
            if !matches!(
                thread.status(),
                mlua::thread::ThreadStatus::Finished | mlua::thread::ThreadStatus::Error
            ) {
                append(owner);
            }
        }
    }
    Ok(seen)
}

fn current_thread(lua: &Lua) -> mlua::Result<Thread> {
    unsafe {
        lua.exec_raw::<Thread>((), |state| {
            mlua::ffi::lua_pushthread(state);
        })
    }
}

pub fn install(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    let weak_table = || -> mlua::Result<Table> {
        let table = lua.create_table()?;
        let weak_keys = lua.create_table()?;
        weak_keys.set("__mode", "k")?;
        table.set_metatable(Some(weak_keys))?;
        Ok(table)
    };
    lua.set_app_data(CoroutineOwners {
        owners: weak_table()?,
        started: weak_table()?,
    });

    // Standard coroutines run in a new VM thread, outside the task scheduler.
    // Keep the source Instance attached to that real coroutine so identity
    // queries inside coroutine.create/wrap descendants remain accurate.
    let coroutine: Table = globals.get("coroutine")?;
    let native_create: Function = coroutine.get("create")?;
    let create = lua.create_function(move |lua, function: Function| {
        let parent = lua.current_thread();
        let thread: Thread = native_create.call(function)?;
        remember_coroutine_owner(lua, &thread, &parent)?;
        Ok(thread)
    })?;
    coroutine.set("create", create)?;

    let native_resume: Function = coroutine.get("resume")?;
    let mark_started = lua.create_function(move |lua, thread: Thread| {
        if let Some(owners) = lua.app_data_ref::<CoroutineOwners>() {
            if owners
                .owners
                .get::<Value>(thread.clone())
                .is_ok_and(|owner| !owner.is_nil())
            {
                owners.started.set(thread.clone(), true)?;
            }
        }
        Ok(())
    })?;
    // Keep the scheduler's resume wrapper in a yieldable Lua frame. Calling
    // it from a Rust callback would put VM preemption yields across a C
    // boundary, which Luau correctly rejects.
    let wrap_resume: Function = lua
        .load(
            "return function(native_resume, mark_started) return function(thread, ...) mark_started(thread); return native_resume(thread, ...) end end",
        )
        .eval()?;
    coroutine.set(
        "resume",
        wrap_resume.call::<Function>((native_resume, mark_started))?,
    )?;

    let wrap: Function = lua
        .load(
            r#"return function(coroutine)
            local create, resume, unpack_values = coroutine.create, coroutine.resume, table.unpack
            return function(fn)
                local thread = create(fn)
                return function(...)
                    local results = table.pack(resume(thread, ...))
                    if not results[1] then error(results[2], 2) end
                    return unpack_values(results, 2, results.n)
                end
            end
        end"#,
        )
        .eval()?;
    coroutine.set("wrap", wrap.call::<Function>(coroutine.clone())?)?;

    globals.set(
        "getscriptfromthread",
        lua.create_function(|lua, thread: Thread| {
            Ok(script_for_thread(lua, &thread).unwrap_or(Value::Nil))
        })?,
    )?;
    let calling = lua.create_function(|lua, ()| {
        let thread = current_thread(lua)?;
        Ok(script_for_thread(lua, &thread).unwrap_or(Value::Nil))
    })?;
    globals.set("getcallingscript", calling.clone())?;
    globals.set("getcaller", calling)?;
    // Ownership instrumentation runs inside the native, yieldable entry points,
    // preserving the standard coroutine API's actual C-closure representation.
    super::closures::wrap_coroutine_entries(lua)?;
    Ok(())
}

#[cfg(test)]
#[path = "script_threads_tests.rs"]
mod tests;
