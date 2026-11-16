use super::*;
use crate::rust_host::{closures, scheduler, world::World};
use serde_json::json;

#[test]
fn getscriptclosure_reuses_the_admitted_running_proto_and_live_child() {
    let snapshot = json!({
        "version": 1,
        "sequence": 0,
        "services": {"ReplicatedFirst": "replicated-first"},
        "nodes": [
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"replicated-first","class_name":"ReplicatedFirst","name":"ReplicatedFirst","parent":"root","server_owned":true},
            {"id":"probe","class_name":"LocalScript","name":"Probe","parent":"replicated-first","server_owned":true},
            {"id":"blank","class_name":"LocalScript","name":"Blank","parent":"replicated-first","server_owned":false,"properties":{"Source":""}}
        ],
        "script_sources": {
            "probe": {"source":"_G.__script_runs = (_G.__script_runs or 0) + 1; local function returns_false() return false end; _G.__retained_script_child = returns_false; task.wait(60)"}
        }
    });
    let world = Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()));
    let lua = Lua::new();
    super::super::integrity::install(&lua).unwrap();
    scheduler::install(&lua, &Rc::new(RefCell::new(scheduler::Scheduler::new()))).unwrap();
    super::super::world::install(&lua, &world).unwrap();
    super::super::source_helpers::install(&lua, &world).unwrap();
    closures::install(&lua).unwrap();
    install(&lua, &world).unwrap();

    let probe = world::instance(&lua, &world, "probe").unwrap();
    let blank = world::instance(&lua, &world, "blank").unwrap();
    lua.globals().set("probe_script", probe.clone()).unwrap();
    let getscriptclosure: Function = lua.globals().get("getscriptclosure").unwrap();
    let no_code: Value = getscriptclosure.call(blank.clone()).unwrap();
    assert!(matches!(no_code, Value::Nil));
    let getsenv: Function = lua.globals().get("getsenv").unwrap();
    assert!(getsenv.call::<Table>(blank.clone()).is_err());
    assert!(getsenv.call::<Table>(probe.clone()).is_err());
    let register_source: Function = lua
        .globals()
        .get::<Table>("client")
        .unwrap()
        .get("registerScriptSource")
        .unwrap();
    register_source.call::<()>((blank.clone(), "")).unwrap();
    let explicitly_empty: Value = getscriptclosure.call(blank).unwrap();
    assert!(matches!(explicitly_empty, Value::Function(_)));

    // The getter is allowed to compile an admitted script before it starts;
    // reconciliation must then execute that same closure/Proto.
    let before_start: Function = getscriptclosure.call(probe.clone()).unwrap();
    let hookfunction: Function = lua.globals().get("hookfunction").unwrap();
    let replacement = lua
        .create_function(|_, ()| Ok("detached clone hook"))
        .unwrap();
    hookfunction
        .call::<Function>((before_start.clone(), replacement))
        .unwrap();
    assert_eq!(
        before_start.call::<String>(()).unwrap(),
        "detached clone hook"
    );
    reconcile(&lua, &world).unwrap();
    let scheduler = lua
        .app_data_ref::<scheduler::SchedulerRef>()
        .unwrap()
        .clone();
    let report = scheduler::step(
        &lua,
        &scheduler,
        std::time::Instant::now() + std::time::Duration::from_secs(1),
        std::time::Duration::from_millis(100),
    );
    assert!(report.errors.is_empty(), "{:?}", report.errors);

    let after_start: Function = getscriptclosure.call(probe).unwrap();
    assert_ne!(before_start.to_pointer(), after_start.to_pointer());
    let probe_value: Value = lua.globals().get("probe_script").unwrap();
    let first_environment: Table = getsenv.call(probe_value.clone()).unwrap();
    let second_environment: Table = getsenv.call(probe_value).unwrap();
    assert_eq!(first_environment.to_pointer(), second_environment.to_pointer());
    let getrenv: Function = lua.globals().get("getrenv").unwrap();
    let engine: Table = getrenv.call(()).unwrap();
    let engine_globals: Table = engine.get("_G").unwrap();
    assert_eq!(engine_globals.get::<i64>("__script_runs").unwrap(), 1);
    let active: Table = lua
        .load("return debug.getproto(getscriptclosure(probe_script), 1, true)")
        .eval()
        .unwrap();
    let active_child: Function = active.get(1).expect("a retained live child closure");
    let retained_child: Function = engine_globals.get("__retained_script_child").unwrap();
    assert_eq!(active_child.to_pointer(), retained_child.to_pointer());
    assert!(!active_child.call::<bool>(()).unwrap());
}

#[test]
fn getsenv_returns_nil_for_a_script_running_in_an_actor_vm() {
    let snapshot = json!({
        "version": 1,
        "sequence": 0,
        "services": {"Workspace": "workspace"},
        "nodes": [
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"root","server_owned":true},
            {"id":"actor","class_name":"Actor","name":"Worker","parent":"workspace","server_owned":false},
            {"id":"actor-script","class_name":"LocalScript","name":"ActorScript","parent":"actor","server_owned":true},
            {"id":"inactive","class_name":"LocalScript","name":"Inactive","parent":"workspace","server_owned":true}
        ],
        "script_sources": {
            "actor-script": {"source":"task.wait(60)"},
            "inactive": {"source":"return true"}
        }
    });
    let world = Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()));
    let lua = Lua::new();
    scheduler::install(&lua, &Rc::new(RefCell::new(scheduler::Scheduler::new()))).unwrap();
    super::super::install_bindings(&lua, &world).unwrap();
    let shared = super::super::shared_tables::SharedTableRegistry::default();
    super::super::shared_tables::install(&lua, &world, shared.clone()).unwrap();
    super::super::actors::install(&lua, &world, shared.clone()).unwrap();

    let inactive = world::instance(&lua, &world, "inactive").unwrap();
    let actor_script = world::instance(&lua, &world, "actor-script").unwrap();
    let getsenv: Function = lua.globals().get("getsenv").unwrap();
    assert!(getsenv.call::<Table>(inactive).is_err());

    let report = super::super::actors::step(
        &lua,
        &world,
        &shared,
        std::time::Duration::from_secs(1),
    )
    .unwrap();
    assert!(report[0]["vm_instantiated"].as_bool().unwrap_or(false));
    let foreign_environment: Value = getsenv.call(actor_script).unwrap();
    assert!(matches!(foreign_environment, Value::Nil));
}
