use super::closures;
use mlua::{Lua, Thread, UserData, UserDataMethods};
use std::{cell::RefCell, rc::Rc};

struct ClosureReceiver;

impl UserData for ClosureReceiver {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("GetService", |_, _, name: String| Ok(name));
    }
}

fn runtime() -> Lua {
    let lua = Lua::new();
    closures::install(&lua).expect("closure bindings install");
    lua
}

#[test]
fn function_metadata_vararg_flag_is_numeric_and_matches_the_function() {
    let lua = runtime();
    let (fixed, variadic, parameters): (i64, i64, i64) = lua
        .load(
            r#"
        local fixed = debug.getinfo(function(a,b) return a,b end)
        local variadic = debug.getinfo(function(a,...) return a,... end)
        assert(type(fixed.is_vararg) == 'number' and type(variadic.is_vararg) == 'number')
        return fixed.is_vararg, variadic.is_vararg, variadic.numparams
    "#,
        )
        .eval()
        .unwrap();
    assert_eq!((fixed, variadic, parameters), (0, 1, 1));
}

#[test]
fn environments_and_closure_kind_use_current_luau_functions() {
    let lua = runtime();
    let (env_is_global, local_environment_works, c, l): (bool, bool, bool, bool) = lua
        .load(
            r#"
            local f = function() return marker end
            local env = { marker = 42 }
            setfenv(f, env)
            return getfenv(0) == _G, f() == 42, iscclosure(print), islclosure(f)
        "#,
        )
        .eval()
        .expect("environment and closure kind checks");
    assert!(env_is_global && local_environment_works && c && l);
}

#[test]
fn function_metadata_clone_and_upvalue_read_write_are_real() {
    let lua = runtime();
    let value: (String, bool, i64, i64, bool, i64, i64) = lua
        .load(
            r#"
            local function factory(first, second)
                return function() return first, second end
            end
            local f = factory(11, 17)
            local copied = clonefunction(f)
            local info = debug.getinfo(f)
            local original1, original2 = getupvalue(f, 1), getupvalue(f, 2)
            local changed1 = debug.setupvalue(f, 1, 29)
            local changed2 = debug.setupvalue(f, 2, 31)
            local a, b = f()
            local old_a, old_b = copied()
            return info.what, type(info.source) == "string", original1, original2,
                changed1 and changed2 and a == 29 and b == 31,
                old_a, old_b
        "#,
        )
        .eval()
        .expect("function metadata and upvalue checks");
    assert_eq!(value.0, "Lua");
    assert!(value.1);
    assert_eq!((value.2, value.3), (11, 17));
    assert!(value.4, "setupvalue must return true for existing slots");
    assert_eq!((value.5, value.6), (11, 17));
}

#[test]
fn debug_getupvalue_accepts_a_live_stack_level() {
    let lua = runtime();
    let (same_value, c_closure_rejected): (bool, bool) = lua
        .load(
            r#"
            local captured = { marker = 17 }
            local function inspect()
                -- Referencing captured guarantees it is an upvalue of this
                -- active frame; level 1 means the current Luau function.
                return captured == debug.getupvalue(1, 1),
                    not pcall(debug.getupvalue, print, 1)
            end
            return inspect()
        "#,
        )
        .eval()
        .expect("debug.getupvalue reads function and active-frame targets");
    assert!(same_value && c_closure_rejected);
}

#[test]
fn upvalue_indices_are_one_based_and_out_of_range_is_explicit() {
    let lua = runtime();
    let (get_fails, setup_result, zero_fails): (bool, bool, bool) = lua
        .load(
            r#"
            local captured = 5
            local f = function() return captured end
            local get_ok = pcall(getupvalue, f, 2)
            local setup_ok = debug.setupvalue(f, 2, 8)
            local zero_ok = pcall(debug.setupvalue, f, 0, 8)
            return not get_ok, setup_ok, not zero_ok
        "#,
        )
        .eval()
        .expect("invalid upvalue paths return the documented outcome");
    assert!(get_fails, "missing getupvalue slot must raise an error");
    assert!(!setup_result, "missing setupvalue slot must return false");
    assert!(zero_fails, "upvalue slots are one-based");
}

#[test]
fn setfenv_matches_official_function_and_thread_semantics() {
    let lua = runtime();
    let result: (bool, bool, bool) = lua
        .load(
            r#"
            local env = {}
            local f = function() end
            local returned = setfenv(f, env)
            local old = getfenv(0)
            local set_thread = setfenv(0, env) == nil and getfenv(0) == env
            setfenv(0, old)
            return returned == f, set_thread, not pcall(setfenv, print, env)
        "#,
        )
        .eval()
        .expect("official setfenv function/thread semantics");
    assert!(result.0 && result.1 && result.2);
}

#[test]
fn stack_debug_and_environment_helpers_accept_integral_float_levels() {
    let lua = runtime();
    let result: (bool, bool, bool, bool) = lua
        .load(
            r#"
            local info_ok = (function()
                local info = debug.getinfo(1.0)
                return info ~= nil and info.func ~= nil and info.currentline >= 0
            end)()
            local env = {marker=23}
            local function read_after_stack_set()
                setfenv(1.0, env)
                return marker
            end
            local stack_env_ok = read_after_stack_set() == 23
            local old = getfenv(0.0)
            setfenv(0.0, env)
            local thread_env_ok = getfenv(0.0) == env
            setfenv(0, old)
            local float_error = not pcall(getfenv, 1.5)
            return info_ok, stack_env_ok, thread_env_ok, float_error
        "#,
        )
        .eval()
        .expect("stack metadata and environment level behavior");
    assert!(result.0 && result.1 && result.2 && result.3);
}

#[test]
fn getupvalues_aliases_return_all_real_captures() {
    let lua = runtime();
    let values: (i64, i64, i64, i64) = lua
        .load(
            r#"
            local function factory(a, b)
                return function() return a, b end
            end
            local f = factory(12, 34)
            local a, b = debug.getupvalues(f), getupvals(f)
            return a[1], a[2], b[1], b[2]
        "#,
        )
        .eval()
        .expect("upvalue aliases");
    assert_eq!(values, (12, 34, 12, 34));
}

#[test]
fn current_thread_and_thread_environment_helpers_use_this_vm() {
    let lua = runtime();
    let result: (bool, bool, bool, bool) = lua
        .load(
            r#"
            local thread = coroutine.create(function()
                return getcurrentthread() == coroutine.running()
            end)
            local resumed, same_thread = coroutine.resume(thread)
            local env = gettenv(thread)
            local all = getallthreads()
            return resumed, same_thread, type(env) == "table", type(all) == "table"
        "#,
        )
        .eval()
        .expect("thread environment and registry enumeration helpers");
    assert!(result.0 && result.1 && result.2 && result.3);
}

#[test]
fn newcclosure_preserves_multiple_results_errors_and_yields() {
    let lua = runtime();
    let result: (bool, bool, i64, bool, String, bool, bool, i64, bool) = lua
        .load(
            r#"
            local wrapped = newcclosure(function(a, b)
                return a + b, nil, "tail"
            end)
            local values = table.pack(wrapped(4, 7))
            local failure = newcclosure(function() error("wrapped boom") end)
            local ok, message = pcall(failure)

            local yielding = newcclosure(function(value)
                local resumed = coroutine.yield(value + 1)
                return resumed + 2
            end)
            local thread = coroutine.create(function() return yielding(5) end)
            local first_ok, yielded = coroutine.resume(thread)
            local second_ok, returned = coroutine.resume(thread, 40)
            return isnewcclosure(wrapped), values.n == 3, values[1], values[2] == nil,
                tostring(message), not ok, first_ok, yielded, second_ok and returned == 42
        "#,
        )
        .eval()
        .expect("native closure forwarding and yield continuation");
    assert!(result.0 && result.1 && result.3 && result.5 && result.6);
    assert_eq!((result.2, result.7), (11, 6));
    assert!(result.4.contains("wrapped boom"));
    assert!(result.8);
}

#[test]
fn newcclosure_preserves_existing_c_function_identity() {
    let lua = runtime();
    let result: (bool, bool) = lua
        .load("local f=print; return newcclosure(f)==f, not isnewcclosure(f)")
        .eval()
        .expect("existing C closures are returned unchanged");
    assert!(result.0 && result.1);
}

#[test]
fn hook_clone_restore_and_executor_closure_markers_apply_to_this_vm() {
    let lua = runtime();
    // This exercises real closure identity and body replacement, including a
    // native closure target. A registry-based Lua wrapper cannot satisfy the
    // captured-reference assertions below.
    let value: (bool, bool, bool, bool, bool, bool, bool, bool, bool, bool) = lua
        .load(
            r#"
            local state = 0
            local target = function(x) state += 1; return x + 1 end
            local original = clonefunction(target)
            local old = hookfunction(target, function(x) state += 10; return x + 9 end)
            local hook_works = old(1) == 2 and target(1) == 10 and state == 11
            local ctarget = coroutine.resume
            local coriginal = clonefunction(ctarget)
            local previous = hookfunction(ctarget, function() return true, 'hooked' end)
            local copied_ok = not pcall(coriginal, {})
            local c_works = ctarget({}) == true and type(previous) == 'function'
            restorefunction(ctarget)
            local co = coroutine.create(function() return 6 end)
            local resumed, returned = ctarget(co)
            local restored_c = ctarget == coroutine.resume and resumed and returned == 6
            hookfunction(target, function(x) return x * 3 end)
            hookfunction(target, function(x) return x * 4 end)
            local stacked = target(2) == 8
            restorefunction(target)
            local restored = target(2) == 3
            local native = newcclosure(function() return 7 end)
            local clone_native = clonefunction(native)
            local tags = iscclosure(native) and iscclosure(clone_native)
                and isexecutorclosure(native) and isnewcclosure(native)
                and isexecutorclosure(isexecutorclosure)
                and not isexecutorclosure(table.concat)
                and not isexecutorclosure(function() end)
            return hook_works, original(1) == 2, c_works, copied_ok, restored_c,
                stacked, restored, tags,
                pcall(restorefunction, function() end) == false,
                iscclosure(coroutine.wrap), not islclosure(coroutine.wrap)
        "#,
        )
        .eval()
        .expect("official VM closure hooks, clones, and markers");
    assert!(value.0 && value.1 && value.2 && value.3 && value.4 && value.5 && value.6 && value.7);
    assert!(value.8 && value.9);
}

#[test]
fn closure_hooks_forward_extra_upvalues_and_lua_to_c_yields() {
    let lua = runtime();
    let values: (bool, bool, bool, bool, bool, bool) = lua
        .load(
            r#"
            local target = function(value) return value + 1 end
            local original = clonefunction(target)
            local replacement = (function(multiplier, addition)
                return function(value) return value * multiplier + addition end
            end)(4, 5)
            hookfunction(target, replacement)
            local extra_upvalues = target(3) == 17 and original(3) == 4
            restorefunction(target)
            local restored = target(3) == 4

            local yielding = function() return 'original' end
            hookfunction(yielding, coroutine.yield)
            local thread = coroutine.create(function() return yielding('pause') end)
            local first_ok, paused = coroutine.resume(thread)
            local second_ok, resumed = coroutine.resume(thread, 'continue')
            restorefunction(yielding)
            return extra_upvalues, restored, first_ok, paused == 'pause',
                second_ok, resumed == 'continue' and yielding() == 'original'
        "#,
        )
        .eval()
        .expect("closure replacement upvalue capacity and yield continuation");
    assert!(values.0 && values.1 && values.2 && values.3 && values.4 && values.5);
}

#[test]
fn hookmetamethod_updates_live_metatable_and_returns_previous_values() {
    let lua = runtime();
    let values: (bool, bool, bool, bool, bool) = lua
        .load(
            r#"
            local object = setmetatable({}, {
                __namecall = function(self, value) return 'original', value end,
            })
            local original = hookmetamethod(object, '__namecall', function(self, value)
                return getnamecallmethod(), value + 1
            end)
            local old_name, old_value = original(object, 9)

            local indexed = setmetatable({}, {})
            local previous = hookmetamethod(indexed, '__index', function(_, key)
                return 'read:' .. key
            end)
            return type(original) == 'function', original ~= nil,
                old_name == 'original' and old_value == 9,
                previous == nil and indexed.Answer == 'read:Answer',
                getnamecallmethod() == nil
        "#,
        )
        .eval()
        .expect("live metatable replacement and namecall atom");
    assert!(values.0 && values.1 && values.2 && values.3 && values.4);
}

#[test]
fn hookmetamethod_temporarily_updates_readonly_metatable_and_restores_flag() {
    let lua = runtime();
    let values: (bool, bool, bool, bool, bool) = lua
        .load(
            r#"
            local object = setmetatable({}, {__index = function() return 'old' end})
            local mt = getrawmetatable(object)
            setreadonly(mt, true)
            local old = hookmetamethod(object, '__index', function(_, key)
                return 'new:' .. key
            end)
            local readonly = isreadonly(mt)
            local blocked = not pcall(function() mt.__index = function() end end)
            return old(object, 'x') == 'old', object.x == 'new:x', readonly,
                blocked, isreadonly(mt)
        "#,
        )
        .eval()
        .expect("hooking a readonly metatable preserves its readonly state");
    assert!(values.0 && values.1 && values.2 && values.3 && values.4);
}

#[test]
fn scheduler_coroutine_wrappers_are_native_yieldable_and_keep_results() {
    let lua = Lua::new();
    let scheduler = Rc::new(RefCell::new(super::scheduler::Scheduler::new()));
    super::scheduler::install(&lua, &scheduler).expect("scheduler bindings");
    let coroutine: mlua::Table = lua.globals().get("coroutine").unwrap();
    unsafe {
        lua.exec_raw::<()>((coroutine.clone(),), |state| {
            mlua::ffi::lua_setreadonly(state, -1, 1);
        })
        .unwrap();
    }
    closures::install(&lua).expect("closure and native coroutine bindings");
    let values: (bool, bool, bool, bool, bool, bool, bool, bool, bool) = lua
        .load(
            r#"
            local co = coroutine.create(function(value)
                local resumed = coroutine.yield('paused', nil)
                return value, resumed, nil
            end)
            local first = table.pack(coroutine.resume(co, 'input'))
            local second = table.pack(coroutine.resume(co, 23))

            local wrapped = coroutine.wrap(function()
                local resumed = coroutine.yield('wrapped-wait', nil)
                return 'wrapped-done', resumed, nil
            end)
            local parent = coroutine.create(function()
                local first = table.pack(wrapped())
                local second = table.pack(wrapped(41))
                return first.n == 2 and first[1] == 'wrapped-wait' and first[2] == nil,
                    second.n == 3 and second[1] == 'wrapped-done'
                        and second[2] == 41 and second[3] == nil
            end)
            local wrap_results = table.pack(coroutine.resume(parent))

            local failing = coroutine.create(function() error('resume failure') end)
            local resume_error = table.pack(coroutine.resume(failing))
            local wrap_error = coroutine.wrap(function() error('wrap failure') end)
            local wrap_ok, wrap_message = pcall(wrap_error)
            return iscclosure(coroutine.create) and iscclosure(coroutine.resume)
                    and iscclosure(coroutine.wrap),
                not isexecutorclosure(coroutine.create)
                    and not isnewcclosure(coroutine.resume),
                first.n == 3 and first[1] and first[2] == 'paused' and first[3] == nil,
                second.n == 4 and second[1] and second[2] == 'input'
                    and second[3] == 23 and second[4] == nil,
                wrap_results.n == 3 and wrap_results[1] and wrap_results[2],
                wrap_results.n == 3 and wrap_results[3],
                resume_error.n == 2 and not resume_error[1]
                    and string.find(tostring(resume_error[2]), 'resume failure') ~= nil,
                not wrap_ok and string.find(tostring(wrap_message), 'wrap failure') ~= nil,
                isreadonly(coroutine)
        "#,
        )
        .eval()
        .expect("native coroutine adapters preserve scheduler results, errors, and yields");
    assert!(
        values.0 && values.1 && values.2 && values.3 && values.4 && values.5,
        "native identity / create resume / wrap results: {values:?}"
    );
    assert!(values.6 && values.7 && values.8);
}

#[test]
fn instance_namecall_hook_forwards_index_methods_and_yields() {
    let lua = runtime();
    lua.globals()
        .set("game", lua.create_userdata(ClosureReceiver).unwrap())
        .unwrap();
    let values: (String, String, bool, bool) = lua
        .load(
            r#"
            local mt = getrawmetatable(game)
            mt.__index = {
                GetService = function(_, name) return name end,
                InvokeServer = function(_, value)
                    local resumed = coroutine.yield('wait:' .. value)
                    return 'reply:' .. resumed
                end,
            }
            local old
            old = hookmetamethod(game, '__namecall', function(self, ...)
                assert(getnamecallmethod() ~= nil)
                return old(self, ...)
            end)
            local service = game:GetService('Players')
            local thread = coroutine.create(function()
                return game:InvokeServer('request')
            end)
            local first_ok, yielded = coroutine.resume(thread)
            local second_ok, returned = coroutine.resume(thread, 'done')
            return service, yielded, first_ok, second_ok and returned == 'reply:done'
        "#,
        )
        .eval()
        .expect("yieldable generic namecall forwarding");
    assert_eq!(
        values,
        ("Players".into(), "wait:request".into(), true, true)
    );
}

#[test]
fn registry_aliases_return_the_current_vm_registry() {
    let lua = runtime();
    let valid: bool = lua
        .load(
            "local r=getregistry(); return type(r)=='table' and r==getreg() and r==debug.getregistry() and r==debug.getreg()",
        )
        .eval()
        .expect("registry aliases");
    assert!(valid);
}

#[test]
fn raw_metatable_helpers_bypass_protection_and_return_the_target() {
    let lua = runtime();
    let result: (bool, bool, bool) = lua
        .load(
            r#"
            local value = setmetatable({}, {__metatable="locked", old=true})
            local hidden = getmetatable(value) == "locked"
            local raw = getrawmetatable(value)
            raw.old = false
            local returned = setrawmetatable(value, {new=true}) == value
            return hidden and raw.old == false, returned,
                debug.getmetatable(value).new == true
        "#,
        )
        .eval()
        .expect("raw metatable aliases");
    assert!(result.0 && result.1 && result.2);
}

#[test]
fn readonly_helpers_toggle_luau_table_write_protection() {
    let lua = runtime();
    let result: (bool, bool, bool, bool) = lua
        .load(
            r#"
            local value = {}
            local before = not isreadonly(value)
            setreadonly(value, true)
            local blocked = isreadonly(value) and not pcall(function() value.x=1 end)
            setreadonly(value, false)
            value.x = 2
            return before, blocked, not isreadonly(value), value.x == 2
        "#,
        )
        .eval()
        .expect("readonly table controls");
    assert!(result.0 && result.1 && result.2 && result.3);
}

#[test]
fn profiler_spans_are_nested_and_validate_limits() {
    let lua = runtime();
    let result: (bool, bool, bool) = lua
        .load(
            r#"
            local unmatched = not pcall(debug.profileend)
            debug.profilebegin("outer")
            debug.profilebegin("inner")
            debug.profileend()
            debug.profileend()
            local balanced = pcall(debug.profileend)
            local oversized = not pcall(debug.profilebegin, string.rep("x", 257))
            return unmatched, not balanced, oversized
        "#,
        )
        .eval()
        .expect("profile span balance and label bound");
    assert!(result.0 && result.1 && result.2);
}

#[test]
fn profiler_report_tracks_completed_local_vm_spans() {
    let lua = runtime();
    lua.load(
        r#"
        profile_thread = getcurrentthread()
        debug.profilebegin("local probe")
        debug.profileend()
    "#,
    )
    .exec()
    .expect("profile span");
    let thread: Thread = lua.globals().get("profile_thread").unwrap();
    let report = closures::raw_vm_debug::profiler_report(&lua, &thread)
        .expect("read local profiler report")
        .expect("report exists");
    assert_eq!(report.completed, 1);
    assert_eq!(
        report.by_label.get("local probe").map(|entry| entry.0),
        Some(1)
    );
    assert!(report.elapsed_seconds >= 0.0);
}

#[test]
fn memory_category_helpers_use_named_local_vm_categories() {
    let lua = runtime();
    let result: (bool, bool) = lua
        .load(
            r#"
            local valid = pcall(debug.setmemorycategory, "local-category")
            debug.resetmemorycategory()
            local too_long = not pcall(debug.setmemorycategory, string.rep("x", 257))
            return valid, too_long
        "#,
        )
        .eval()
        .expect("memory category name validation");
    assert!(result.0 && result.1);
}

#[test]
fn public_local_debug_apis_read_and_write_active_luau_locals() {
    let lua = runtime();
    let result: (bool, bool, bool, bool, bool) = lua
        .load(
            r#"
            local function inspect(argument)
                local before = debug.getstack(1, 1)
                local locals = debug.getstack(1)
                local valid = validlevel(1.0) and isvalidlevel(1)
                local callstack = debug.getcallstack()
                local visible = locals[1] == argument and callstack[2].func == inspect
                debug.setstack(1, 1, 41)
                return before == 9, visible, valid, argument == 41,
                    pcall(debug.getstack, 1, 100000)
            end
            return inspect(9)
        "#,
        )
        // Lua's public lua_getlocal API needs compiler-emitted local metadata.
        .set_compiler(mlua::chunk::Compiler::new().set_debug_level(2))
        .eval()
        .expect("public Lua local inspection and write");
    assert!(
        result.0 && result.1 && result.2 && result.3 && !result.4,
        "{result:?}"
    );
}

#[test]
fn loaded_authored_source_has_locals_for_public_stack_debug_apis() {
    let lua = runtime();
    super::integrity::install(&lua).expect("integrity loader install");
    let function = super::integrity::load_source(
        &lua,
        "=authored-stack-debug-test",
        r#"
            local function inspect(argument)
                local before = debug.getstack(1, 1)
                debug.setstack(1, 1, 41)
                return before == 9 and argument == 41
            end
            return inspect(9)
        "#,
    )
    .expect("authored source compiles with local debug metadata");
    assert!(function
        .call::<bool>(())
        .expect("public stack APIs inspect real loaded-source locals"));
}

#[test]
fn stack_helpers_address_temporary_registers_without_local_names() {
    let lua = runtime();
    let marker = lua.create_table().unwrap();
    lua.globals().set("stack_marker", marker.clone()).unwrap();
    let result: bool = lua
        .load(
            r#"
            local function inspect()
                -- Force the compiler to reserve VM registers beyond the
                -- source locals in this frame.
                local holder = (function(a, b, c, d, e, f, g, h)
                    return a
                end)(1, 2, 3, 4, 5, 6, 7, 8)
                debug.setstack(1, 8, stack_marker)
                local indexed = debug.getstack(1, 8)
                local all = debug.getstack(1)
                local after_snapshot = debug.getstack(1, 8)
                collectgarbage('collect')
                return holder == 1 and indexed == stack_marker
                    and all[8] == stack_marker and after_snapshot == stack_marker
                    and stack_marker.marker == nil
            end
            return inspect()
        "#,
        )
        .eval()
        .expect("stack helpers read/write VM registers without local metadata");
    assert!(result);
    assert_eq!(marker.raw_len(), 0);
}
