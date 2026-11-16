//! Debug helpers for the current Luau VM, with the low-level work in the VM bridge.

use mlua::{AnyUserData, Function, Lua, Table, Thread, UserData, Value, Variadic};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    rc::Rc,
    time::Instant,
};

const PROFILE_MAX_DEPTH: usize = 128;
const PROFILE_MAX_LABEL_BYTES: usize = 256;
const PROFILE_MAX_LABELS: usize = 64;
const LUA_MEMORY_CATEGORIES: i32 = 256;

#[derive(Default)]
struct ProfileData {
    stack: Vec<ProfileSpan>,
    completed: u64,
    elapsed_seconds: f64,
    by_label: BTreeMap<String, (u64, f64)>,
    overflow_completed: u64,
    overflow_elapsed_seconds: f64,
}

struct ProfileSpan {
    label: String,
    started: Instant,
}

#[derive(Clone)]
struct ProfileStore(Table);

impl UserData for ProfileData {}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProfilerReport {
    pub completed: u64,
    pub elapsed_seconds: f64,
    pub by_label: BTreeMap<String, (u64, f64)>,
    pub overflow_completed: u64,
    pub overflow_elapsed_seconds: f64,
}

fn raw_registry(lua: &Lua) -> mlua::Result<Table> {
    unsafe {
        lua.exec_raw::<Table>((), |state| {
            mlua::ffi::lua_pushvalue(state, mlua::ffi::LUA_REGISTRYINDEX);
        })
    }
}

fn raw_get_metatable(lua: &Lua, target: Value) -> mlua::Result<Value> {
    unsafe {
        lua.exec_raw((target,), |state| {
            let slot = mlua::ffi::lua_gettop(state);
            if mlua::ffi::lua_getmetatable(state, slot) == 0 {
                mlua::ffi::lua_pushnil(state);
            }
            mlua::ffi::lua_remove(state, slot);
        })
    }
}

fn raw_set_metatable(lua: &Lua, target: Value, metatable: Option<Table>) -> mlua::Result<Value> {
    unsafe {
        lua.exec_raw((target, metatable), |state| {
            let target_slot = mlua::ffi::lua_gettop(state) - 1;
            mlua::ffi::lua_setmetatable(state, target_slot);
        })
    }
}

pub fn install(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    let registry = raw_registry(lua)?;
    let debug: Table = globals.get("debug")?;
    let get_registry = lua.create_function(|lua, ()| raw_registry(lua))?;
    globals.set("getregistry", get_registry.clone())?;
    globals.set("getreg", get_registry.clone())?;
    debug.set("getregistry", get_registry.clone())?;
    debug.set("getreg", get_registry)?;

    let get_metatable = lua.create_function(|lua, target: Value| raw_get_metatable(lua, target))?;
    let set_metatable =
        lua.create_function(|lua, (target, metatable): (Value, Option<Table>)| {
            raw_set_metatable(lua, target, metatable)
        })?;
    globals.set("getrawmetatable", get_metatable.clone())?;
    globals.set("setrawmetatable", set_metatable.clone())?;
    debug.set("getmetatable", get_metatable.clone())?;
    debug.set("getrawmetatable", get_metatable)?;
    debug.set("setmetatable", set_metatable.clone())?;
    debug.set("setrawmetatable", set_metatable)?;

    globals.set(
        "isreadonly",
        lua.create_function(|lua, table: Table| unsafe {
            lua.exec_raw::<bool>((table,), |state| {
                let slot = mlua::ffi::lua_gettop(state);
                let readonly = mlua::ffi::lua_getreadonly(state, slot) != 0;
                mlua::ffi::lua_remove(state, slot);
                mlua::ffi::lua_pushboolean(state, if readonly { 1 } else { 0 });
            })
        })?,
    )?;
    globals.set(
        "setreadonly",
        lua.create_function(|lua, (table, readonly): (Table, bool)| unsafe {
            lua.exec_raw::<()>((table,), |state| {
                let slot = mlua::ffi::lua_gettop(state);
                mlua::ffi::lua_setreadonly(state, slot, if readonly { 1 } else { 0 });
                mlua::ffi::lua_setsafeenv(state, slot, 0);
                mlua::ffi::lua_setsafeenv(state, mlua::ffi::LUA_GLOBALSINDEX, 0);
                mlua::ffi::lua_remove(state, slot);
            })
        })?,
    )?;
    // Keep a strong local handle while creating the function table above; this
    // also verifies that the VM registry is genuinely a table at installation.
    let _ = registry;
    install_thread_helpers(lua)?;
    install_stack_helpers(lua, &debug)?;
    install_profiler(lua, &debug)?;
    install_memory_categories(lua, &debug)?;
    install_namecall_query(lua)?;
    install_vm_inspection(lua, &debug)?;
    Ok(())
}

unsafe extern "C-unwind" {
    fn rove_vm_getgc(state: *mut mlua::ffi::lua_State) -> std::ffi::c_int;
    fn rove_vm_getconstants(state: *mut mlua::ffi::lua_State) -> std::ffi::c_int;
    fn rove_vm_getconstant(state: *mut mlua::ffi::lua_State) -> std::ffi::c_int;
    fn rove_vm_setconstant(state: *mut mlua::ffi::lua_State) -> std::ffi::c_int;
    fn rove_vm_functiondata(state: *mut mlua::ffi::lua_State) -> std::ffi::c_int;
    fn rove_vm_getproto(state: *mut mlua::ffi::lua_State) -> std::ffi::c_int;
    fn rove_vm_getprotos(state: *mut mlua::ffi::lua_State) -> std::ffi::c_int;
    fn rove_vm_getstack(state: *mut mlua::ffi::lua_State) -> std::ffi::c_int;
    fn rove_vm_setstack(state: *mut mlua::ffi::lua_State) -> std::ffi::c_int;
}

fn vm_c_function(lua: &Lua, callback: mlua::ffi::lua_CFunction) -> mlua::Result<Function> {
    unsafe {
        lua.exec_raw::<Function>((), |state| {
            mlua::ffi::lua_pushcfunction(state, callback);
        })
    }
}

fn install_vm_inspection(lua: &Lua, debug: &Table) -> mlua::Result<()> {
    let globals = lua.globals();
    let getgc = vm_c_function(lua, rove_vm_getgc)?;
    globals.set("getgc", getgc.clone())?;
    for (name, callback) in [
        (
            "getconstants",
            rove_vm_getconstants as mlua::ffi::lua_CFunction,
        ),
        (
            "getconstant",
            rove_vm_getconstant as mlua::ffi::lua_CFunction,
        ),
        (
            "setconstant",
            rove_vm_setconstant as mlua::ffi::lua_CFunction,
        ),
        ("getprotos", rove_vm_getprotos as mlua::ffi::lua_CFunction),
        ("getproto", rove_vm_getproto as mlua::ffi::lua_CFunction),
    ] {
        debug.set(name, vm_c_function(lua, callback)?)?;
    }
    let getconstants: Function = debug.get("getconstants")?;
    let getupvalues: Function = debug.get("getupvalues")?;
    let functiondata = vm_c_function(lua, rove_vm_functiondata)?;
    let hash_functiondata = functiondata.clone();
    globals.set(
        "getfunctionhash",
        lua.create_function(move |_, function: Function| {
            use sha2::Digest;
            let data: mlua::String = hash_functiondata.call(function)?;
            let digest = sha2::Sha384::digest(data.as_bytes());
            let mut hex = String::with_capacity(96);
            for byte in digest {
                use std::fmt::Write;
                let _ = write!(hex, "{byte:02x}");
            }
            Ok(hex)
        })?,
    )?;
    let weak_getgc = getgc.clone();
    let weak_getconstants = getconstants.clone();
    let weak_getupvalues = getupvalues.clone();
    let weak_functiondata = functiondata.clone();
    let is_executor: Option<Function> = globals.get("isexecutorclosure").ok();
    globals.set(
        "filtergc",
        lua.create_function(
            move |lua, (kind, options, return_one): (String, Table, Option<bool>)| {
                filter_gc(
                    lua,
                    &weak_getgc,
                    &weak_getconstants,
                    &weak_getupvalues,
                    &weak_functiondata,
                    is_executor.as_ref(),
                    &kind,
                    options,
                    return_one.unwrap_or(false),
                )
            },
        )?,
    )?;
    Ok(())
}

fn lua_values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) if a.is_nan() || b.is_nan() => {
            a.is_nan() && b.is_nan()
        }
        (Value::Number(a), Value::Number(b)) if *a == 0.0 && *b == 0.0 => {
            a.is_sign_negative() == b.is_sign_negative()
        }
        _ => left == right,
    }
}

fn has_filter_member(lua: &Lua, wanted: &Value, candidates: &Table) -> mlua::Result<bool> {
    for pair in candidates.clone().pairs::<Value, Value>() {
        let (_, value) = pair?;
        if lua_values_equal(wanted, &value) {
            return Ok(true);
        }
    }
    let _ = lua;
    Ok(false)
}

fn filter_gc(
    lua: &Lua,
    getgc: &Function,
    getconstants: &Function,
    getupvalues: &Function,
    functiondata: &Function,
    is_executor: Option<&Function>,
    kind: &str,
    options: Table,
    return_one: bool,
) -> mlua::Result<Value> {
    if !matches!(kind, "function" | "table") {
        return Err(mlua::Error::RuntimeError(
            "filter type must be function or table".into(),
        ));
    }
    for pair in options.clone().pairs::<Value, Value>() {
        let (key, value) = pair?;
        let key = match key {
            Value::String(key) => key.to_string_lossy(),
            _ => {
                return Err(mlua::Error::RuntimeError(
                    "filter option names must be strings".into(),
                ))
            }
        };
        let valid = match key.as_str() {
            "Name" | "Hash" => matches!(value, Value::String(_)),
            "IgnoreExecutor" => matches!(value, Value::Boolean(_)),
            "Constants" | "Upvalues" | "Keys" | "Values" | "KeyValuePairs" | "Metatable" => {
                matches!(value, Value::Table(_))
            }
            _ => {
                return Err(mlua::Error::RuntimeError(format!(
                    "unknown filter option {key}"
                )))
            }
        };
        if !valid {
            return Err(mlua::Error::RuntimeError(format!(
                "invalid filter option {key}"
            )));
        }
    }

    let all: Table = getgc.call(kind == "table")?;
    let result = lua.create_table()?;
    let ignore_executor = options
        .get::<Option<bool>>("IgnoreExecutor")?
        .unwrap_or(true);
    let expected_name = options.get::<Option<String>>("Name")?;
    let expected_hash = options.get::<Option<String>>("Hash")?;
    let constants = options.get::<Option<Table>>("Constants")?;
    let upvalues = options.get::<Option<Table>>("Upvalues")?;
    let keys = options.get::<Option<Table>>("Keys")?;
    let values = options.get::<Option<Table>>("Values")?;
    let pairs = options.get::<Option<Table>>("KeyValuePairs")?;
    let metatable = options.get::<Option<Table>>("Metatable")?;
    let mut count = 0;

    for pair in all.pairs::<Value, Value>() {
        let (_, candidate) = pair?;
        let matches = if kind == "function" {
            let Value::Function(function) = &candidate else {
                continue;
            };
            let is_c_function = function.info().what == "C";
            if ignore_executor
                && is_executor.is_some_and(|is_executor| {
                    is_executor.call::<bool>(function.clone()).unwrap_or(false)
                })
            {
                false
            } else {
                let name_matches = expected_name
                    .as_ref()
                    .is_none_or(|name| function.info().name.as_ref() == Some(name));
                let constants_match = if let Some(expected) = &constants {
                    if is_c_function {
                        false
                    } else {
                    let actual: Table = getconstants.call(function.clone())?;
                    let mut matched = true;
                    for pair in expected.clone().pairs::<Value, Value>() {
                        let (_, wanted) = pair?;
                        if !has_filter_member(lua, &wanted, &actual)? {
                            matched = false;
                            break;
                        }
                    }
                    matched
                    }
                } else {
                    true
                };
                let upvalues_match = if let Some(expected) = &upvalues {
                    if is_c_function {
                        false
                    } else {
                    let actual: Table = getupvalues.call(function.clone())?;
                    let mut matched = true;
                    for pair in expected.clone().pairs::<Value, Value>() {
                        let (_, wanted) = pair?;
                        if !has_filter_member(lua, &wanted, &actual)? {
                            matched = false;
                            break;
                        }
                    }
                    matched
                    }
                } else {
                    true
                };
                let hash_matches = if let Some(expected) = &expected_hash {
                    if is_c_function {
                        false
                    } else {
                    let data: mlua::String = functiondata.call(function.clone())?;
                    use sha2::Digest;
                    let digest = sha2::Sha384::digest(data.as_bytes());
                    let actual = digest
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>();
                    &actual == expected
                    }
                } else {
                    true
                };
                name_matches && constants_match && upvalues_match && hash_matches
            }
        } else {
            let Value::Table(table) = &candidate else {
                continue;
            };
            let metatable_matches = metatable
                .as_ref()
                .is_none_or(|want| table.metatable().is_some_and(|actual| actual == *want));
            let keys_match = if let Some(expected) = &keys {
                let mut matched = true;
                for pair in expected.clone().pairs::<Value, Value>() {
                    let (_, key) = pair?;
                    if matches!(table.raw_get::<Value>(key)?, Value::Nil) {
                        matched = false;
                        break;
                    }
                }
                matched
            } else {
                true
            };
            let values_match = if let Some(expected) = &values {
                let mut matched = true;
                for pair in expected.clone().pairs::<Value, Value>() {
                    let (_, wanted) = pair?;
                    let mut found = false;
                    for actual in table.clone().pairs::<Value, Value>() {
                        let (_, value) = actual?;
                        if lua_values_equal(&wanted, &value) {
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        matched = false;
                        break;
                    }
                }
                matched
            } else {
                true
            };
            let pairs_match = if let Some(expected) = &pairs {
                let mut matched = true;
                for pair in expected.clone().pairs::<Value, Value>() {
                    let (key, wanted) = pair?;
                    let actual: Value = table.raw_get(key)?;
                    if !lua_values_equal(&wanted, &actual) {
                        matched = false;
                        break;
                    }
                }
                matched
            } else {
                true
            };
            metatable_matches && keys_match && values_match && pairs_match
        };
        if matches {
            if return_one {
                return Ok(candidate);
            }
            count += 1;
            result.raw_set(count, candidate)?;
        }
    }
    if return_one {
        Ok(Value::Nil)
    } else {
        Ok(Value::Table(result))
    }
}

pub fn stack_function(lua: &Lua, level: i32) -> mlua::Result<Option<(Function, i32)>> {
    stack_function_in_thread(lua, None, level)
}

#[cfg(test)]
mod vm_inspection_tests {
    use super::*;

    #[test]
    fn gc_enumeration_returns_real_live_functions_tables_and_userdata_classes() {
        let lua = Lua::new();
        super::super::install(&lua).unwrap();
        lua.load(
            r#"
            local function visible_function() return 'live function marker' end
            local visible_table = { marker = 'live table marker' }
            _G.__inspection_function = visible_function
            _G.__inspection_table = visible_table
            local functions = getgc()
            local all = getgc(true)
            local found_function, found_table = false, false
            for _, value in functions do
                if value == visible_function then found_function = true end
            end
            for _, value in all do
                if value == visible_function then found_function = true end
                if value == visible_table then found_table = true end
            end
            assert(found_function and found_table)
            assert(#all >= #functions)
            _G.__inspection_function = nil
            _G.__inspection_table = nil
        "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn getgc_true_collects_stale_heap_before_snapshot_under_memory_limit() {
        let lua = Lua::new();
        super::super::install(&lua).unwrap();
        let churn = lua
            .load(
                r#"
            local retained = { marker = "must remain discoverable" }
            _G.__inspection_retained = retained
            local weak = setmetatable({}, { __mode = "v" })
            _G.__inspection_weak = weak
            do
                for i = 1, 100000 do
                    local transient = { index = i, text = tostring(i), a = i, b = i }
                    if i == 100000 then weak[1] = transient end
                end
            end
        "#,
        )
            .into_function()
            .unwrap();
        let repeated_scans = lua
            .load(
                r#"
            local retained = _G.__inspection_retained
            local weak = _G.__inspection_weak
            local snapshot = getgc(true)
            assert(weak[1] == nil, "getgc should sweep unreachable heap objects first")
            local found = false
            for _, object in snapshot do
                if object == retained then found = true; break end
            end
            assert(found)
            snapshot = nil
            for _ = 1, 24 do
                local next_snapshot = getgc(true)
                local still_found = false
                for _, object in next_snapshot do
                    if object == retained then still_found = true; break end
                end
                assert(still_found)
                next_snapshot = nil
            end
            _G.__inspection_retained = nil
            _G.__inspection_weak = nil
        "#,
            )
            .into_function()
            .unwrap();

        // Suppress collection only while constructing the stale heap, then
        // restart it immediately before the constrained getgc call.
        unsafe {
            lua.exec_raw::<()>((), |state| {
                mlua::ffi::lua_gc(state, mlua::ffi::LUA_GCSTOP, 0);
            })
            .unwrap();
        }
        churn.call::<()>(()).unwrap();
        unsafe {
            lua.exec_raw::<()>((), |state| {
                mlua::ffi::lua_gc(state, mlua::ffi::LUA_GCRESTART, 0);
            })
            .unwrap();
        }

        // Keep the amount cap below the deliberately accumulated stale heap.
        // getgc must sweep that garbage before allocating its live snapshot.
        lua.set_memory_limit(8 * 1024 * 1024).unwrap();
        repeated_scans
            .call::<()>(())
        .expect("getgc(true) should exclude unreachable garbage and survive repeated bounded scans");
        assert!(lua.gc_is_running(), "getgc should leave automatic collection enabled");
    }

    #[test]
    fn active_proto_inspection_does_not_collect_existing_unreferenced_closures() {
        let lua = Lua::new();
        super::super::install(&lua).unwrap();
        let fixture: mlua::Function = lua.load(r#"
            return function()
                local result = 0
                local function parent()
                    local function child(value) result = value; return result end
                end
                parent()
                local active = debug.getproto(parent, 1, true)
                assert(#active == 1, 'inspection collected the existing child closure')
                assert(active[1](71) == 71 and result == 71)
            end
        "#).eval().unwrap();
        lua.gc_collect().unwrap();
        fixture.call::<()>(()).unwrap();
        assert!(lua.gc_is_running());
    }

    #[test]
    fn constants_and_child_prototypes_are_read_from_live_luau_closures() {
        let lua = Lua::new();
        super::super::install(&lua).unwrap();
        lua.load(
            r#"
            local function parent()
                local function child() return 'child literal', 73 end
                return child
            end
            local constants = debug.getconstants(parent)
            local found = false
            for _, value in constants do
                if value == 'parent literal' then found = true end
            end
            -- Keep a guaranteed parent-proto literal in this function's code.
            local with_literal = function() return 'parent literal' end
            constants = debug.getconstants(with_literal)
            found = false
            for _, value in constants do if value == 'parent literal' then found = true end end
            assert(found)
            local protos = debug.getprotos(parent)
            assert(#protos == 1 and type(protos[1]) == 'function')
            local active = debug.getproto(parent, 1, true)
            assert(type(active) == 'table')
            assert(not pcall(protos[1]))
            assert(not pcall(debug.getconstant, print, 1))
            assert(not pcall(debug.getconstant, with_literal, 100000))
        "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn function_hash_and_filtergc_match_real_functions_and_tables() {
        let lua = Lua::new();
        super::super::install(&lua).unwrap();
        lua.load(
            r#"
            local target = function() return 'filter literal', 91 end
            local other = function() return 'different literal', 92 end
            local target_hash = getfunctionhash(target)
            assert(#target_hash == 96 and target_hash:match('^[0-9a-f]+$'))
            assert(target_hash == getfunctionhash(target))
            assert(target_hash ~= getfunctionhash(other))
            local by_constant = filtergc('function', {Constants={'filter literal'}})
            local found = false
            for _, value in by_constant do if value == target then found = true end end
            assert(found)
            local by_hash = filtergc('function', {Hash=target_hash}, true)
            assert(type(by_hash) == 'function' and getfunctionhash(by_hash) == target_hash)
            assert(filtergc('function', {Name='no such closure'}, true) == nil)
            local wanted = { alpha = 17, beta = 'needle' }
            local tables = filtergc('table', {
                Keys={'alpha'}, Values={'needle'}, KeyValuePairs={alpha=17},
                Metatable=getmetatable(wanted)
            })
            found = false
            for _, value in tables do if value == wanted then found = true end end
            local by_key = filtergc('table', {KeyValuePairs={alpha=17}})
            for _, value in by_key do if value == wanted then found = true end end
            assert(found)
            assert(not pcall(filtergc, 'userdata', {}))
            assert(not pcall(filtergc, 'table', {Unknown=true}))
        "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn filtergc_matches_a_negative_zero_bytecode_constant() {
        let lua = Lua::new();
        super::super::install(&lua).unwrap();
        lua.load(
            r#"
            local function negative_zero() return -0 end
            _G.__negative_zero_fixture = negative_zero
            local found_constant = false
            for _, value in debug.getconstants(negative_zero) do
                if type(value) == 'number' and value == 0 and 1 / value == -math.huge then
                    found_constant = true
                end
            end
            assert(found_constant, 'the compiled literal must preserve negative zero')

            local matches = filtergc('function', {Constants={-0}})
            local found_function = false
            for _, candidate in matches do
                if candidate == negative_zero then found_function = true end
            end
            assert(found_function, 'filtergc should find a closure with a -0 constant')
            _G.__negative_zero_fixture = nil
        "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn setconstant_changes_matching_primitive_and_rejects_invalid_or_import_slots() {
        let lua = Lua::new();
        super::super::install(&lua).unwrap();
        lua.load(
            r#"
            local function target() return 'before', 37 end
            local constants = debug.getconstants(target)
            local string_index
            for index, value in constants do
                if value == 'before' then string_index = index end
            end
            assert(string_index)
            assert(debug.setconstant(target, string_index, 'after') == nil)
            assert(target() == 'after')
            assert(not pcall(debug.setconstant, target, string_index, 99))
            assert(target() == 'after')
            assert(not pcall(debug.setconstant, target, 100000, 'bad'))
            assert(not pcall(debug.setconstant, print, 1, 'bad'))

            local function imported() return math.abs(-5), 'imported marker' end
            debug.getconstants(imported)
            for index = 1, 64 do
                -- This function contains no boolean literals. A bool write to
                -- its imported-function cache or any other constant must fail.
                local ok = pcall(debug.setconstant, imported, index, true)
                assert(not ok)
            end
            local number, marker = imported()
            assert(number == 5 and marker == 'imported marker')
        "#,
        )
        .exec()
        .unwrap();
    }
}

fn stack_function_in_thread(
    lua: &Lua,
    thread: Option<Thread>,
    level: i32,
) -> mlua::Result<Option<(Function, i32)>> {
    unsafe {
        lua.exec_raw((thread,), |state| {
            let thread_slot = mlua::ffi::lua_gettop(state);
            let target = mlua::ffi::lua_tothread(state, thread_slot);
            let same_thread = target.is_null() || target == state;
            let target = if same_thread { state } else { target };
            let mut info: mlua::ffi::lua_Debug = std::mem::zeroed();
            if mlua::ffi::lua_getinfo(
                target,
                if same_thread {
                    level.saturating_add(1)
                } else {
                    level
                },
                c"slnuaf".as_ptr(),
                &mut info,
            ) == 0
            {
                mlua::ffi::lua_pushnil(state);
                mlua::ffi::lua_pushinteger_(state, -1);
            } else {
                if !same_thread {
                    mlua::ffi::lua_xmove(target, state, 1);
                }
                mlua::ffi::lua_pushinteger_(state, info.currentline);
            }
            mlua::ffi::lua_remove(state, thread_slot);
        })
        .map(|(function, line): (Option<Function>, i32)| function.map(|f| (f, line)))
    }
}

pub fn stack_environment(lua: &Lua, level: i32) -> mlua::Result<Option<Table>> {
    unsafe {
        lua.exec_raw((), |state| {
            if level == 0 {
                mlua::ffi::lua_pushthread(state);
                let thread_slot = mlua::ffi::lua_gettop(state);
                mlua::ffi::lua_getfenv(state, thread_slot);
                mlua::ffi::lua_remove(state, thread_slot);
            } else {
                let mut info: mlua::ffi::lua_Debug = std::mem::zeroed();
                if mlua::ffi::lua_getinfo(state, level.saturating_add(1), c"f".as_ptr(), &mut info)
                    == 0
                {
                    mlua::ffi::lua_pushnil(state);
                } else {
                    let function_slot = mlua::ffi::lua_gettop(state);
                    mlua::ffi::lua_getfenv(state, function_slot);
                    mlua::ffi::lua_remove(state, function_slot);
                }
            }
        })
    }
}

pub fn mark_environment_unsafe(lua: &Lua, environment: Table) -> mlua::Result<()> {
    unsafe {
        lua.exec_raw::<()>((environment,), |state| {
            let slot = mlua::ffi::lua_gettop(state);
            mlua::ffi::lua_setsafeenv(state, slot, 0);
            mlua::ffi::lua_remove(state, slot);
        })
    }
}

pub fn set_stack_environment(lua: &Lua, level: i32, environment: Table) -> mlua::Result<bool> {
    unsafe {
        lua.exec_raw((environment,), |state| {
            let environment_slot = mlua::ffi::lua_gettop(state);
            mlua::ffi::lua_setsafeenv(state, environment_slot, 0);
            let target_slot = if level == 0 {
                mlua::ffi::lua_pushthread(state);
                mlua::ffi::lua_gettop(state)
            } else {
                let mut info: mlua::ffi::lua_Debug = std::mem::zeroed();
                if mlua::ffi::lua_getinfo(state, level.saturating_add(1), c"f".as_ptr(), &mut info)
                    == 0
                {
                    0
                } else {
                    mlua::ffi::lua_gettop(state)
                }
            };
            let changed = if target_slot == 0 {
                false
            } else {
                mlua::ffi::lua_pushvalue(state, environment_slot);
                mlua::ffi::lua_setfenv(state, target_slot) != 0
            };
            mlua::ffi::lua_settop(state, environment_slot - 1);
            mlua::ffi::lua_pushboolean(state, if changed { 1 } else { 0 });
        })
    }
}

pub fn upvalues(lua: &Lua, function: Function) -> mlua::Result<Table> {
    unsafe {
        lua.exec_raw((function,), |state| {
            let function_slot = mlua::ffi::lua_gettop(state);
            mlua::ffi::lua_createtable(state, 0, 0);
            let table_slot = mlua::ffi::lua_gettop(state);
            let mut index = 1;
            while !mlua::ffi::lua_getupvalue(state, function_slot, index).is_null() {
                mlua::ffi::lua_rawseti_(state, table_slot, index);
                index += 1;
            }
            mlua::ffi::lua_remove(state, function_slot);
        })
    }
}

fn current_thread(lua: &Lua) -> mlua::Result<Thread> {
    unsafe {
        lua.exec_raw::<Thread>((), |state| {
            mlua::ffi::lua_pushthread(state);
        })
    }
}

fn thread_environment(lua: &Lua, thread: Thread) -> mlua::Result<Table> {
    unsafe {
        lua.exec_raw((thread,), |state| {
            let thread_slot = mlua::ffi::lua_gettop(state);
            mlua::ffi::lua_getfenv(state, thread_slot);
            mlua::ffi::lua_remove(state, thread_slot);
        })
    }
}

fn thread_script(lua: &Lua, thread: Thread) -> mlua::Result<Value> {
    unsafe {
        lua.exec_raw((thread,), |state| {
            let thread_slot = mlua::ffi::lua_gettop(state);
            mlua::ffi::lua_getfenv(state, thread_slot);
            let env_slot = mlua::ffi::lua_gettop(state);
            mlua::ffi::lua_rawgetfield(state, env_slot, c"script".as_ptr());
            mlua::ffi::lua_remove(state, env_slot);
            mlua::ffi::lua_remove(state, thread_slot);
        })
    }
}

fn all_threads(lua: &Lua) -> mlua::Result<Table> {
    unsafe {
        lua.exec_raw((), |state| {
            let base = mlua::ffi::lua_gettop(state);
            mlua::ffi::lua_createtable(state, 0, 0);
            let output = base + 1;
            mlua::ffi::lua_pushnil(state);
            let mut index = 0;
            while mlua::ffi::lua_next(state, mlua::ffi::LUA_REGISTRYINDEX) != 0 {
                if mlua::ffi::lua_type(state, -1) == mlua::ffi::LUA_TTHREAD
                    && mlua::ffi::lua_tothread(state, -1) != state
                {
                    mlua::ffi::lua_pushvalue(state, -1);
                    index += 1;
                    mlua::ffi::lua_rawseti_(state, output, index);
                }
                mlua::ffi::lua_settop(state, mlua::ffi::lua_gettop(state) - 1);
            }
        })
    }
}

fn install_thread_helpers(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    globals.set(
        "getcurrentthread",
        lua.create_function(|lua, ()| current_thread(lua))?,
    )?;
    let all = lua.create_function(|lua, ()| all_threads(lua))?;
    globals.set("getallthreads", all.clone())?;
    globals.set("getthreads", all)?;
    let environment = lua.create_function(|lua, thread: Thread| thread_environment(lua, thread))?;
    globals.set("gettenv", environment.clone())?;
    globals.set("getthreadenv", environment)?;
    let script = lua.create_function(|lua, thread: Thread| thread_script(lua, thread))?;
    globals.set("getscriptfromthread", script.clone())?;
    globals.set(
        "getcallingscript",
        lua.create_function(|lua, ()| thread_script(lua, current_thread(lua)?))?,
    )?;
    globals.set("getcaller", globals.get::<Function>("getcallingscript")?)?;
    Ok(())
}

fn profile_record(lua: &Lua, thread: Thread, create: bool) -> mlua::Result<Option<AnyUserData>> {
    let store = lua
        .app_data_ref::<ProfileStore>()
        .ok_or_else(|| mlua::Error::RuntimeError("profiler state is not installed".into()))?
        .0
        .clone();
    if let Some(record) = store.get::<Option<AnyUserData>>(thread.clone())? {
        return Ok(Some(record));
    }
    if !create {
        return Ok(None);
    }
    let record = lua.create_userdata(ProfileData::default())?;
    store.set(thread, record.clone())?;
    Ok(Some(record))
}

fn install_profiler(lua: &Lua, debug: &Table) -> mlua::Result<()> {
    let store = lua.create_table()?;
    let weak_keys = lua.create_table()?;
    weak_keys.set("__mode", "k")?;
    store.set_metatable(Some(weak_keys))?;
    lua.set_app_data(ProfileStore(store));

    debug.set(
        "profilebegin",
        lua.create_function(|lua, label: String| {
            if label.len() > PROFILE_MAX_LABEL_BYTES {
                return Err(mlua::Error::RuntimeError(
                    "profile label exceeds 256 bytes".into(),
                ));
            }
            let thread = current_thread(lua)?;
            let record = profile_record(lua, thread, true)?.ok_or_else(|| {
                mlua::Error::RuntimeError("could not create profiler record".into())
            })?;
            let mut data = record.borrow_mut::<ProfileData>()?;
            if data.stack.len() >= PROFILE_MAX_DEPTH {
                return Err(mlua::Error::RuntimeError(
                    "profile nesting exceeds 128 spans".into(),
                ));
            }
            if !data.by_label.contains_key(&label) && data.by_label.len() < PROFILE_MAX_LABELS {
                data.by_label.insert(label.clone(), (0, 0.0));
            }
            data.stack.push(ProfileSpan {
                label,
                started: Instant::now(),
            });
            Ok(())
        })?,
    )?;
    debug.set(
        "profileend",
        lua.create_function(|lua, ()| {
            let thread = current_thread(lua)?;
            let record = profile_record(lua, thread, false)?.ok_or_else(|| {
                mlua::Error::RuntimeError(
                    "profileend called without a matching profilebegin".into(),
                )
            })?;
            let mut data = record.borrow_mut::<ProfileData>()?;
            let span = data.stack.pop().ok_or_else(|| {
                mlua::Error::RuntimeError(
                    "profileend called without a matching profilebegin".into(),
                )
            })?;
            let elapsed = span.started.elapsed().as_secs_f64();
            data.completed = data.completed.saturating_add(1);
            data.elapsed_seconds += elapsed;
            if let Some(stats) = data.by_label.get_mut(&span.label) {
                stats.0 = stats.0.saturating_add(1);
                stats.1 += elapsed;
            } else {
                data.overflow_completed = data.overflow_completed.saturating_add(1);
                data.overflow_elapsed_seconds += elapsed;
            }
            Ok(())
        })?,
    )?;
    Ok(())
}

pub fn profiler_report(lua: &Lua, thread: &Thread) -> mlua::Result<Option<ProfilerReport>> {
    let Some(record) = profile_record(lua, thread.clone(), false)? else {
        return Ok(None);
    };
    let data = record.borrow::<ProfileData>()?;
    Ok(Some(ProfilerReport {
        completed: data.completed,
        elapsed_seconds: data.elapsed_seconds,
        by_label: data.by_label.clone(),
        overflow_completed: data.overflow_completed,
        overflow_elapsed_seconds: data.overflow_elapsed_seconds,
    }))
}

fn install_memory_categories(lua: &Lua, debug: &Table) -> mlua::Result<()> {
    let names = Rc::new(RefCell::new(HashMap::<String, i32>::new()));
    let set_names = names.clone();
    debug.set(
        "setmemorycategory",
        lua.create_function(move |lua, name: String| {
            if name.len() > 256 {
                return Err(mlua::Error::RuntimeError(
                    "memory category name exceeds 256 bytes".into(),
                ));
            }
            let category = {
                let mut names = set_names.borrow_mut();
                if let Some(&category) = names.get(&name) {
                    category
                } else {
                    let category = i32::try_from(names.len() + 1).map_err(|_| {
                        mlua::Error::RuntimeError("local VM memory category limit exceeded".into())
                    })?;
                    if category >= LUA_MEMORY_CATEGORIES {
                        return Err(mlua::Error::RuntimeError(
                            "local VM memory category limit exceeded".into(),
                        ));
                    }
                    names.insert(name, category);
                    category
                }
            };
            unsafe {
                lua.exec_raw::<()>((), |state| {
                    mlua::ffi::lua_setmemcat(state, category);
                })
            }
        })?,
    )?;
    debug.set(
        "resetmemorycategory",
        lua.create_function(|lua, ()| unsafe {
            lua.exec_raw::<()>((), |state| {
                mlua::ffi::lua_setmemcat(state, 0);
            })
        })?,
    )?;
    Ok(())
}

fn install_namecall_query(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    let get_namecall = lua.create_function(|lua, ()| unsafe {
        lua.exec_raw::<Value>((), |state| {
            let atom = mlua::ffi::lua_namecallatom(state, std::ptr::null_mut());
            if atom.is_null() {
                mlua::ffi::lua_pushnil(state);
            } else {
                mlua::ffi::lua_pushstring_(state, atom);
            }
        })
    })?;
    for alias in [
        "getnamecallmethod",
        "get_namecall_method",
        "get_namecall",
        "getnamecall",
    ] {
        globals.set(alias, get_namecall.clone())?;
    }
    Ok(())
}

fn stack_level(value: Value) -> mlua::Result<i32> {
    let number = match value {
        Value::Integer(value) => {
            return i32::try_from(value).map_err(|_| {
                mlua::Error::RuntimeError("debug index or level out of range".into())
            });
        }
        Value::Number(value) => value,
        _ => return Err(mlua::Error::RuntimeError("integer level expected".into())),
    };
    if !number.is_finite()
        || number.fract() != 0.0
        || number < i32::MIN as f64
        || number > i32::MAX as f64
    {
        return Err(mlua::Error::RuntimeError(
            "debug index or level out of range".into(),
        ));
    }
    Ok(number as i32)
}

fn install_stack_helpers(lua: &Lua, debug: &Table) -> mlua::Result<()> {
    let globals = lua.globals();
    let valid = lua.create_function(|lua, value: Value| {
        let level = stack_level(value)?;
        if level < 0 {
            return Ok(false);
        }
        Ok(stack_function(lua, level)?.is_some())
    })?;
    globals.set("validlevel", valid.clone())?;
    globals.set("isvalidlevel", valid)?;
    let get_callstack = lua.create_function(|lua, args: Variadic<Value>| {
        let (thread, start) = match args.first() {
            Some(Value::Thread(thread)) => {
                let level = args
                    .get(1)
                    .filter(|v| !matches!(v, Value::Nil))
                    .cloned()
                    .map(stack_level)
                    .transpose()?
                    .unwrap_or(1);
                (Some(thread.clone()), level)
            }
            Some(Value::Nil) | None => (None, 0),
            Some(value) => (None, stack_level(value.clone())?),
        };
        if start < 0 {
            return Err(mlua::Error::RuntimeError(
                "stack level must be nonnegative".into(),
            ));
        }
        let output = lua.create_table()?;
        for offset in 0..PROFILE_MAX_DEPTH {
            let Some(level) = start.checked_add(offset as i32) else {
                break;
            };
            let Some((function, line)) = stack_function_in_thread(lua, thread.clone(), level)?
            else {
                break;
            };
            let function_info = function.info();
            let row = lua.create_table()?;
            row.set("func", function)?;
            row.set(
                "name",
                function_info.name.unwrap_or_else(|| "anonymous".into()),
            )?;
            row.set(
                "source",
                function_info.short_src.unwrap_or_else(|| "?".into()),
            )?;
            row.set("line", line)?;
            output.set(offset + 1, row)?;
        }
        Ok(output)
    })?;
    globals.set("getcallstack", get_callstack.clone())?;
    debug.set("getcallstack", get_callstack)?;

    debug.set("getstack", vm_c_function(lua, rove_vm_getstack)?)?;
    let set_stack = vm_c_function(lua, rove_vm_setstack)?;
    debug.set("setstack", set_stack.clone())?;
    debug.set("setcallstack", set_stack)?;
    Ok(())
}
