use super::*;
use crate::rust_host::{scheduler, world::World};
use serde_json::json;

fn session(snapshot: &Json) -> (Lua, WorldRef) {
    let mut snapshot = snapshot.clone();
    let fields = snapshot
        .as_object_mut()
        .expect("world test snapshot must be an object");
    fields.entry("version").or_insert(json!(1));
    fields.entry("sequence").or_insert(json!(0));
    let world = Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()));
    let lua = Lua::new();
    super::super::integrity::install(&lua).unwrap();
    scheduler::install(&lua, &Rc::new(RefCell::new(scheduler::Scheduler::new()))).unwrap();
    super::super::world::install(&lua, &world).unwrap();
    super::super::source_helpers::install(&lua, &world).unwrap();
    install(&lua, &world).unwrap();
    (lua, world)
}

#[test]
fn pinned_builtin_single_entry_validation_accepts_exact_member_only() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let bundle = crate::runtime_sources::load_builtin_player_scripts(root).unwrap();
    let item = bundle
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry.get("bytecode_provenance").is_some())
        .expect("pinned bundle should include a compiled player script");

    // Compilation validates one selected item against the complete trusted
    // bundle. Passing a singleton bundle to whole-bundle validation used to
    // reject every otherwise-valid pinned script.
    super::validate_builtin_entry(item).unwrap();

    let mut changed = item.clone();
    changed["source"] = json!("-- altered source");
    assert!(super::validate_builtin_entry(&changed).is_err());
}

fn run_scheduler(lua: &Lua) -> Vec<String> {
    let scheduler = lua
        .app_data_ref::<scheduler::SchedulerRef>()
        .unwrap()
        .clone();
    let mut errors = Vec::new();
    for _ in 0..3 {
        let report = scheduler::step(
            lua,
            &scheduler,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(30),
        );
        errors.extend(report.errors);
    }
    errors.extend(scheduler::drain_errors(&scheduler));
    errors
}

#[test]
fn cancelled_require_callers_do_not_resume_and_shared_module_cache_still_completes() {
    for fail in [false, true] {
        let source = if fail {
            "task.wait(0); shared.module_runs=(shared.module_runs or 0)+1; error('expected module failure')"
        } else {
            "task.wait(0); shared.module_runs=(shared.module_runs or 0)+1; return {answer=42}"
        };
        let (lua, world) = session(&json!({
            "nodes":[
                {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
                {"id":"slow","class_name":"ModuleScript","name":"Slow","parent":"root","server_owned":true}
            ],"script_sources":{"slow":{"source":source}}
        }));
        lua.globals().set("slow", world::instance(&lua, &world, "slow").unwrap()).unwrap();
        lua.load(r#"
            resumed = 0; active_result = nil
            first = task.spawn(function() pcall(require, slow); resumed += 1 end)
            second = task.spawn(function() pcall(require, slow); resumed += 1 end)
            task.spawn(function() active_result = table.pack(pcall(require, slow)) end)
        "#).exec().unwrap();
        let sched = lua.app_data_ref::<scheduler::SchedulerRef>().unwrap().clone();
        let report = scheduler::step(&lua, &sched, std::time::Instant::now(), std::time::Duration::from_millis(100));
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        lua.load("assert(active_result == nil); task.cancel(first); task.cancel(second)").exec().unwrap();
        let errors = run_scheduler(&lua);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(lua.globals().get::<i64>("resumed").unwrap(), 0, "fail={fail}");
        lua.load("task.spawn(function() cached_result = table.pack(pcall(require, slow)) end)").exec().unwrap();
        assert!(run_scheduler(&lua).is_empty());
        lua.load("assert(shared.module_runs == 1 and active_result[1] == cached_result[1])").exec().unwrap();
        if fail {
            lua.load("assert(active_result[1] == false and string.find(active_result[2], 'expected module failure') and string.find(cached_result[2], 'expected module failure'))").exec().unwrap();
        } else {
            lua.load("assert(active_result[1] == true and active_result[2].answer == 42 and active_result[2] == cached_result[2])").exec().unwrap();
        }
    }
}

#[test]
fn cancelled_require_waiters_do_not_accumulate_while_module_execution_is_pending() {
    let (lua, world) = session(&json!({
        "nodes":[
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"slow","class_name":"ModuleScript","name":"Slow","parent":"root","server_owned":true}
        ],"script_sources":{"slow":{"source":"task.wait(60); return 42"}}
    }));
    lua.globals().set("slow", world::instance(&lua, &world, "slow").unwrap()).unwrap();
    lua.load("resumed=0; function start() caller=task.spawn(function() assert(require(slow) == 42); resumed+=1 end) end; function cancel() task.cancel(caller) end")
        .exec().unwrap();
    let start: Function = lua.globals().get("start").unwrap();
    let cancel: Function = lua.globals().get("cancel").unwrap();
    let sched = lua.app_data_ref::<scheduler::SchedulerRef>().unwrap().clone();
    for _ in 0..128 {
        start.call::<()>(()).unwrap();
        let report = scheduler::step(&lua, &sched, std::time::Instant::now(), std::time::Duration::from_millis(100));
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        cancel.call::<()>(()).unwrap();
    }
    let lifecycle = lua.app_data_ref::<Rc<RefCell<Lifecycle>>>().unwrap().clone();
    assert_eq!(lifecycle.borrow().modules["slow"].waiters.len(), 1);
    start.call::<()>(()).unwrap();
    assert!(scheduler::step(&lua, &sched, std::time::Instant::now(), std::time::Duration::from_millis(100)).errors.is_empty());
    let report = scheduler::step(&lua, &sched, std::time::Instant::now() + std::time::Duration::from_secs(61), std::time::Duration::from_millis(100));
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(lua.globals().get::<i64>("resumed").unwrap(), 1);
    assert!(lifecycle.borrow().modules["slow"].waiters.is_empty());
}

#[test]
fn module_require_caches_local_module_results() {
    let snapshot = json!({
        "nodes": [
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"module1","class_name":"ModuleScript","name":"Config","parent":"root","server_owned":true}
        ],
        "script_sources": {"module1":{"source":"shared.module_runs=(shared.module_runs or 0)+1; return {answer=42}"}}
    });
    let (lua, world) = session(&snapshot);
    let module = world::instance(&lua, &world, "module1").unwrap();
    lua.globals().set("config_module", module).unwrap();
    let task: Function = lua
        .load("return function() local a=require(config_module); local b=require(config_module); shared.module_answer=a==b and a.answer end")
        .eval()
        .unwrap();
    scheduler::enqueue(&lua, task, MultiValue::new()).unwrap();
    let scheduler_errors = run_scheduler(&lua);
    let answer = lua
        .load("return shared.module_answer")
        .eval::<Value>()
        .unwrap();
    let status = lua
        .load("local s=client.scriptStatus(); return s.modules_loaded, s.modules_loading, s.modules_failed, s.details[1] and s.details[1].error")
        .eval::<MultiValue>()
        .unwrap();
    assert!(
        matches!(answer, Value::Integer(42)),
        "answer={answer:?}, status={status:?}, scheduler_errors={scheduler_errors:?}"
    );
    assert_eq!(
        lua.load("return shared.module_runs")
            .eval::<i64>()
            .unwrap(),
        1
    );
    assert_eq!(
        lua.load("return #getloadedmodules()")
            .eval::<i64>()
            .unwrap(),
        1
    );
    assert_eq!(
        lua.load("return #getrunningscripts()")
            .eval::<i64>()
            .unwrap(),
        0,
        "a finished ModuleScript is loaded but no longer running"
    );
}

#[test]
fn executor_and_engine_environments_are_distinct_and_script_scopes_are_clean() {
    let snapshot = json!({
        "nodes": [
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"env-module","class_name":"ModuleScript","name":"EnvProbe","parent":"root","server_owned":true},
            {"id":"env-module-reader","class_name":"ModuleScript","name":"EnvReader","parent":"root","server_owned":true}
        ],
        "script_sources": {
            "env-module":{"source":r#"
                _G.from_first = 'engine shared _G'
                shared.from_first = 'engine shared table'
                return {
                    script = script,
                    has_executor = getgenv ~= nil or loadstring ~= nil or request ~= nil,
                    marker = __environmentProbeMarker,
                    engine_debug = debug.getinfo == nil and type(debug.info) == 'function',
                    shared = shared,
                    globals = _G,
                }
            "#},
            "env-module-reader":{"source":r#"
                return {g = _G.from_first, shared = shared.from_first, env = _G}
            "#}
        }
    });
    let (lua, world) = session(&snapshot);
    let module = world::instance(&lua, &world, "env-module").unwrap();
    lua.globals().set("env_probe_module", module.clone()).unwrap();
    let task: Function = lua.load(r#"return function()
        local executor = getgenv()
        local engine = getrenv()
        assert(getgenv() == executor and getrenv() == engine)
        assert(executor ~= engine and executor._G == executor)
        assert(engine._G == getrenv()._G and engine._G ~= engine and engine._G ~= executor._G)
        assert(_G == executor)
        assert(engine.getgenv == nil and engine.getrenv == nil and engine.loadstring == nil)
        assert(engine.request == nil and engine.debug.getinfo == nil)
        assert(type(engine.debug.info) == 'function' and engine.debug.traceback == debug.traceback)
        assert(engine.shared == shared and engine.task == task and engine.require == require)
        executor.__environmentProbeMarker = 'executor only'
        assert(getgenv().__environmentProbeMarker == 'executor only')
        local values = require(env_probe_module)
        local scope = getsenv(env_probe_module)
        local reader = require(game:FindFirstChild('EnvReader'))
        assert(values.script == env_probe_module and scope.script == env_probe_module)
        assert(not values.has_executor and values.marker == nil)
        assert(values.engine_debug and values.shared == shared and values.globals == engine._G)
        assert(reader.g == 'engine shared _G' and reader.shared == 'engine shared table')
        assert(reader.env == engine._G)
        return true
    end"#).eval().unwrap();
    scheduler::enqueue(&lua, task, MultiValue::new()).unwrap();
    let errors = run_scheduler(&lua);
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn engine_service_globals_follow_late_world_refreshes() {
    let snapshot = json!({
        "nodes":[
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true}
        ]
    });
    let (lua, world) = session(&snapshot);
    let before: bool = lua
        .load("local engine=getrenv(); return engine.game==game and engine.workspace==nil")
        .eval()
        .unwrap();
    assert!(before);

    super::super::world::apply_delta_with_lua(
        &lua,
        &world,
        &json!({
            "version":1,"delta":true,"sequence":1,"services":{"Workspace":"workspace"},"actions":[{
                "kind":"create","sequence":1,"node":{
                    "id":"workspace","class_name":"Workspace","name":"Workspace",
                    "parent":"root","server_owned":true,"properties":{}
                }
            }]
        }),
        None,
    )
    .unwrap();

    let after: bool = lua
        .load("local engine=getrenv(); return engine.workspace==workspace and engine.workspace:IsA('Workspace')")
        .eval()
        .unwrap();
    assert!(after);
}

#[test]
fn clone_source_lookup_follows_catalog_origin_without_exposing_protected_text() {
    let snapshot = json!({
        "nodes": [
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"authored","class_name":"ModuleScript","name":"Authored","parent":"root","server_owned":true,"properties":{"Source":"return 'not the admitted source'"}},
            {"id":"protected","class_name":"ModuleScript","name":"Protected","parent":"root","server_owned":true,"properties":{"Source":"server text must stay hidden"}}
        ],
        "script_sources": {
            "authored":{"source":"return 'admitted source'"},
            "protected":{"reason":"code_unavailable"}
        }
    });
    let (lua, world) = session(&snapshot);
    let authored_clone = world::clone_instance(&lua, &world, "authored").unwrap();
    let authored_code = script_code(&lua, &world, authored_clone).unwrap();
    assert_eq!(authored_code["source"], "return 'admitted source'");

    let protected_clone = world::clone_instance(&lua, &world, "protected").unwrap();
    let protected_id = world::instance_id(&protected_clone).unwrap();
    lua.app_data_ref::<Rc<RefCell<Lifecycle>>>()
        .unwrap()
        .borrow_mut()
        .catalog
        .insert(protected_id, json!({"reason":"code_unavailable"}));
    assert!(script_code(&lua, &world, protected_clone)
        .unwrap_err()
        .to_string()
        .contains("code_unavailable"));
}

#[test]
fn local_player_scripts_container_is_created_once_after_late_initial_world() {
    let snapshot = json!({
        "initial_replication_complete":false,
        "nodes":[
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"players","class_name":"Players","name":"Players","parent":"root","server_owned":true}
        ]
    });
    let (lua, world) = session(&snapshot);
    assert_eq!(reconcile(&lua, &world).unwrap()["running"], 0);
    world.borrow_mut().apply_delta(&json!({
        "version":1,"delta":true,"sequence":1,"actions":[{
            "kind":"create","sequence":1,"id":"player","node":{"id":"player","class_name":"Player","name":"Local","parent":"players","server_owned":true,"properties":{}}
        }],
        "local_player":"player","initial_replication_complete":true
    }), None).unwrap();
    reconcile(&lua, &world).unwrap();
    let created: Vec<String> = world::children(&world, "player").into_iter().filter(|id| {
        world.borrow().nodes.get(id).is_some_and(|node| node.class_name == "PlayerScripts")
    }).collect();
    assert_eq!(created.len(), 1);
    assert!(!world.borrow().nodes[&created[0]].server_owned);
    reconcile(&lua, &world).unwrap();
    let count = world::children(&world, "player").into_iter().filter(|id| {
        world.borrow().nodes.get(id).is_some_and(|node| node.class_name == "PlayerScripts")
    }).count();
    assert_eq!(count, 1);
}

#[test]
fn existing_server_player_scripts_instance_is_preserved() {
    let snapshot = json!({
        "initial_replication_complete":true,
        "local_player":"player",
        "nodes":[
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"players","class_name":"Players","name":"Players","parent":"root","server_owned":true},
            {"id":"player","class_name":"Player","name":"Local","parent":"players","server_owned":true},
            {"id":"existing","class_name":"PlayerScripts","name":"PlayerScripts","parent":"player","server_owned":true}
        ]
    });
    let (lua, world) = session(&snapshot);
    reconcile(&lua, &world).unwrap();
    let containers: Vec<String> = world::children(&world, "player").into_iter().filter(|id| {
        world.borrow().nodes.get(id).is_some_and(|node| node.class_name == "PlayerScripts")
    }).collect();
    assert_eq!(containers, vec!["existing"]);
    assert!(world.borrow().nodes["existing"].server_owned);
}

#[test]
fn actor_lifecycle_does_not_create_player_scripts_container() {
    let snapshot = json!({
        "nodes":[
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"player","class_name":"Player","name":"Local","parent":"root","server_owned":true}
        ],
        "local_player":"player"
    });
    let (lua, world) = session(&snapshot);
    set_actor_id(&lua, Some("actor-1".into())).unwrap();
    reconcile(&lua, &world).unwrap();
    assert!(world::children(&world, "player").into_iter().all(|id| {
        world.borrow().nodes.get(&id).is_none_or(|node| node.class_name != "PlayerScripts")
    }));
}

#[test]
fn require_waits_across_scheduler_polls_and_wakes_concurrent_callers() {
    let snapshot = json!({
        "nodes": [
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"module1","class_name":"ModuleScript","name":"Slow","parent":"root","server_owned":true}
        ],
        "script_sources": {"module1":{"source":"task.wait(0); shared.module_runs=(shared.module_runs or 0)+1; return {answer=99}"}}
    });
    let (lua, world) = session(&snapshot);
    lua.globals()
        .set(
            "slow_module",
            world::instance(&lua, &world, "module1").unwrap(),
        )
        .unwrap();
    let caller: Function = lua
        .load(
            "return function() local value=require(slow_module); getgenv().answer=value.answer end",
        )
        .eval()
        .unwrap();
    scheduler::enqueue(&lua, caller.clone(), MultiValue::new()).unwrap();
    scheduler::enqueue(&lua, caller, MultiValue::new()).unwrap();
    let scheduler_ref = lua
        .app_data_ref::<scheduler::SchedulerRef>()
        .unwrap()
        .clone();
    scheduler::step(
        &lua,
        &scheduler_ref,
        std::time::Instant::now(),
        std::time::Duration::from_millis(30),
    );
    assert_eq!(
        lua.load("return getgenv().answer").eval::<Value>().unwrap(),
        Value::Nil
    );
    let scheduler_errors = run_scheduler(&lua);
    let answer = lua.load("return getgenv().answer").eval::<Value>().unwrap();
    let status = lua
        .load("local s=client.scriptStatus(); return s.modules_loaded, s.modules_loading, s.modules_failed, s.details[1] and s.details[1].error")
        .eval::<MultiValue>()
        .unwrap();
    assert!(
        matches!(answer, Value::Integer(99)),
        "answer={answer:?}, status={status:?}, scheduler_errors={scheduler_errors:?}"
    );
    assert_eq!(
        lua.load("return shared.module_runs")
            .eval::<i64>()
            .unwrap(),
        1
    );
}

#[test]
fn required_module_runs_with_its_instance_owner_and_inherits_caller_identity() {
    let (lua, world) = session(&json!({
        "nodes": [
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"module8","class_name":"ModuleScript","name":"Module8","parent":"root","server_owned":true},
            {"id":"module2","class_name":"ModuleScript","name":"Module2","parent":"root","server_owned":true},
            {"id":"caller2","class_name":"LocalScript","name":"Caller2","parent":"root","server_owned":true}
        ],
        "script_sources": {
            "module8":{"source":"shared.record_module_context('module8'); task.spawn(function() shared.record_module_context('module8_child') end); task.wait(0); return {answer=42}"},
            "module2":{"source":"shared.record_module_context('module2'); task.spawn(function() shared.record_module_context('module2_child') end); task.wait(0); return {answer=24}"}
        }
    }));
    super::super::script_threads::install(&lua).unwrap();
    // Game modules expose only engine globals. A test-owned observer called
    // through shared records the real calling coroutine's executor metadata.
    lua.load(r#"function shared.record_module_context(prefix)
        local root = getgenv()
        root[prefix..'_owner'] = getcallingscript()
        root[prefix..'_identity'] = getthreadidentity()
        root[prefix..'_executor'] = checkcaller()
    end"#).exec().unwrap();

    lua.globals()
        .set("module8", world::instance(&lua, &world, "module8").unwrap())
        .unwrap();
    lua.globals()
        .set("module2", world::instance(&lua, &world, "module2").unwrap())
        .unwrap();

    // An executor-owned caller remains identity 8 while its required module
    // gets the ModuleScript that supplied its actual environment.
    let caller8: Function = lua
        .load("local first=require(module8); local second=require(module8); getgenv().module8_cached=first==second")
        .into_function()
        .unwrap();
    scheduler::enqueue(&lua, caller8, MultiValue::new()).unwrap();

    // A game-script caller remains at its existing identity 2 through require.
    let caller_script = world::instance(&lua, &world, "caller2").unwrap();
    let environment = lua.create_table().unwrap();
    environment.set("script", caller_script).unwrap();
    let metatable = lua.create_table().unwrap();
    metatable.set("__index", lua.globals()).unwrap();
    environment.set_metatable(Some(metatable)).unwrap();
    let caller2: Function = lua
        .load("require(module2)")
        .set_environment(environment)
        .into_function()
        .unwrap();
    scheduler::enqueue_owned(&lua, caller2, MultiValue::new(), "script:caller2".into()).unwrap();

    let errors = run_scheduler(&lua);
    assert!(errors.is_empty(), "{errors:?}");
    let getgenv: Function = lua.globals().get("getgenv").unwrap();
    let executor_globals: Table = getgenv.call(()).unwrap();

    for (name, id, identity) in [
        ("module8", "module8", 8),
        ("module2", "module2", 2),
    ] {
        let owner: Value = executor_globals.get(format!("{name}_owner")).unwrap();
        let expected = world::instance(&lua, &world, id).unwrap();
        assert!(world::compare_instance_references(&owner, &expected).unwrap(), "{name}");
        assert_eq!(
            executor_globals.get::<i64>(format!("{name}_identity")).unwrap(),
            identity,
            "{name} must retain the caller's security identity"
        );
        let child_owner: Value = executor_globals.get(format!("{name}_child_owner")).unwrap();
        assert!(world::compare_instance_references(&child_owner, &expected).unwrap(), "{name} child");
        assert_eq!(
            executor_globals.get::<i64>(format!("{name}_child_identity")).unwrap(),
            identity,
            "{name} child must retain the caller's security identity"
        );
    }
    assert!(executor_globals.get::<bool>("module8_cached").unwrap());
    assert!(!executor_globals.get::<bool>("module8_executor").unwrap());
}

#[test]
fn require_reports_nested_circular_module_dependencies() {
    let snapshot = json!({
        "nodes": [
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"module_a","class_name":"ModuleScript","name":"A","parent":"root","server_owned":true},
            {"id":"module_b","class_name":"ModuleScript","name":"B","parent":"root","server_owned":true}
        ],
        "script_sources": {
            "module_a":{"source":"require(shared.module_b_value); return true"},
            "module_b":{"source":"require(shared.module_a_value); return true"}
        }
    });
    let (lua, world) = session(&snapshot);
    let a = world::instance(&lua, &world, "module_a").unwrap();
    let b = world::instance(&lua, &world, "module_b").unwrap();
    lua.globals().set("module_a_value", a).unwrap();
    lua.globals().set("module_b_value", b).unwrap();
    lua.load("shared.module_a_value=module_a_value; shared.module_b_value=module_b_value").exec().unwrap();
    let task: Function = lua.load("return function() local ok, message=pcall(require, module_a_value); getgenv().cycle_message=tostring(message); getgenv().cycle_result=not ok and string.find(tostring(message), 'circular ModuleScript require') ~= nil end").eval().unwrap();
    scheduler::enqueue(&lua, task, MultiValue::new()).unwrap();
    let scheduler_errors = run_scheduler(&lua);
    let cycle_result = lua
        .load("return getgenv().cycle_result")
        .eval::<Value>()
        .unwrap();
    let cycle_message = lua
        .load("return getgenv().cycle_message")
        .eval::<String>()
        .unwrap_or_default();
    let status = lua
        .load("local s=client.scriptStatus(); return s.modules_loaded, s.modules_loading, s.modules_failed, s.details[1] and s.details[1].error")
        .eval::<MultiValue>()
        .unwrap();
    assert!(
        matches!(cycle_result, Value::Boolean(true)),
        "cycle_message={cycle_message}, status={status:?}, scheduler_errors={scheduler_errors:?}"
    );
}

#[test]
fn source_admission_rejects_unverified_bytecode() {
    let snapshot = json!({"nodes": []});
    let (lua, _) = session(&snapshot);
    let candidate = json!({"script1":{"bytecode_hex":"0d030000"}});
    assert!(update_sources(&lua, &candidate).is_err());
}

#[test]
fn reconcile_schedules_only_ready_local_scripts_and_reports_completion() {
    let snapshot = json!({
        "initial_replication_complete":true,
        "nodes":[
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"rf","class_name":"ReplicatedFirst","name":"ReplicatedFirst","parent":"root","server_owned":true},
            {"id":"ls","class_name":"LocalScript","name":"test","parent":"rf","server_owned":true}
        ],
        "script_sources":{"ls":{"source":"shared.ran_local_script=true"}}
    });
    let (lua, world) = session(&snapshot);
    assert_eq!(reconcile(&lua, &world).unwrap()["running"], 1);
    let scheduler_ref = lua
        .app_data_ref::<scheduler::SchedulerRef>()
        .unwrap()
        .clone();
    scheduler::step(
        &lua,
        &scheduler_ref,
        std::time::Instant::now(),
        std::time::Duration::from_millis(20),
    );
    assert_eq!(
        lua.load("return shared.ran_local_script")
            .eval::<bool>()
            .unwrap(),
        true
    );
    assert_eq!(reconcile(&lua, &world).unwrap()["completed"], 1);
}

#[test]
fn self_destroy_keeps_current_thread_owned_and_cancels_other_owner_tasks() {
    let snapshot = json!({
        "initial_replication_complete":true,
        "nodes":[
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"rf","class_name":"ReplicatedFirst","name":"ReplicatedFirst","parent":"root","server_owned":true},
            {"id":"ls","class_name":"LocalScript","name":"SelfDestroy","parent":"rf","server_owned":true}
        ],
        "script_sources":{"ls":{"source":r#"
            local state={instance=script, after_wait=false}
            shared.self_destroy_state=state
            state.main_thread=coroutine.running()
            state.child_thread=task.spawn(function()
                task.wait(60)
                state.child_after=true
            end)
            script:Destroy()
            state.destroy_returned=true
            task.wait(60)
            state.after_wait=true
        "#}}
    });
    let (lua, world) = session(&snapshot);
    super::super::script_threads::install(&lua).unwrap();
    reconcile(&lua, &world).unwrap();
    let scheduler_ref = lua.app_data_ref::<scheduler::SchedulerRef>().unwrap().clone();
    let first = scheduler::step(&lua, &scheduler_ref, std::time::Instant::now(), std::time::Duration::from_millis(40));
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    reconcile(&lua, &world).unwrap();
    lua.load(r#"
        local state=shared.self_destroy_state
        assert(state.destroy_returned and not state.child_after)
        assert(coroutine.status(state.main_thread)=='suspended')
        assert(coroutine.status(state.child_thread)=='dead')
        assert(table.find(getrunningscripts(),state.instance))
        assert(getscriptfromthread(state.main_thread)==state.instance)
    "#).exec().unwrap();

    let later = std::time::Instant::now() + std::time::Duration::from_secs(61);
    let resumed = scheduler::step(&lua, &scheduler_ref, later, std::time::Duration::from_millis(40));
    assert!(resumed.errors.is_empty(), "{:?}", resumed.errors);
    reconcile(&lua, &world).unwrap();
    lua.load(r#"
        local state=shared.self_destroy_state
        assert(state.after_wait and not state.child_after)
        assert(not table.find(getrunningscripts(),state.instance))
    "#).exec().unwrap();
}

#[test]
fn externally_destroyed_waiting_script_is_cancelled() {
    let snapshot = json!({
        "initial_replication_complete":true,
        "nodes":[
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"rf","class_name":"ReplicatedFirst","name":"ReplicatedFirst","parent":"root","server_owned":true},
            {"id":"ls","class_name":"LocalScript","name":"ExternalDestroy","parent":"rf","server_owned":true}
        ],
        "script_sources":{"ls":{"source":"shared.external_state={instance=script,thread=coroutine.running()}; task.wait(60); shared.external_state.after=true"}}
    });
    let (lua, world) = session(&snapshot);
    super::super::script_threads::install(&lua).unwrap();
    reconcile(&lua, &world).unwrap();
    let scheduler_ref = lua.app_data_ref::<scheduler::SchedulerRef>().unwrap().clone();
    let first = scheduler::step(&lua, &scheduler_ref, std::time::Instant::now(), std::time::Duration::from_millis(40));
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    lua.load("shared.external_state.instance:Destroy()").exec().unwrap();
    reconcile(&lua, &world).unwrap();
    lua.load(r#"
        local state=shared.external_state
        assert(not state.after)
        assert(coroutine.status(state.thread)=='dead')
        assert(not table.find(getrunningscripts(),state.instance))
    "#).exec().unwrap();
}

#[test]
fn actor_scope_rejects_scripts_from_other_actor_instances() {
    let snapshot = json!({
        "nodes":[
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"actor1","class_name":"Actor","name":"A","parent":"root","server_owned":true},
            {"id":"actor2","class_name":"Actor","name":"B","parent":"root","server_owned":true},
            {"id":"script2","class_name":"LocalScript","name":"Client","parent":"actor2","server_owned":true}
        ],
        "script_sources":{"script2":{"source":"shared.wrong_actor=true"}}
    });
    let (lua, world) = session(&snapshot);
    set_actor_id(&lua, Some("actor1".into())).unwrap();
    let status = reconcile(&lua, &world).unwrap();
    assert_eq!(status["running"], 0);
    assert_eq!(
        lua.load("return shared.wrong_actor")
            .eval::<Value>()
            .unwrap(),
        Value::Nil
    );
}
