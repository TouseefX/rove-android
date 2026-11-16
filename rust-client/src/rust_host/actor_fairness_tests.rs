use super::*;
use crate::rust_host::{scheduler, shared_tables::SharedTableRegistry, world::World};
use mlua::{Function, Lua, Value};
use serde_json::json;
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

#[test]
fn second_actor_advances_after_first_native_call_overruns_its_reserved_slice() {
    let snapshot = json!({
        "version": 1,
        "sequence": 0,
        "services": {"Workspace":"workspace"},
        "nodes": [
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"root","server_owned":true},
            {"id":"actor-a","class_name":"Actor","name":"ActorA","parent":"workspace","server_owned":false},
            {"id":"script-a","class_name":"LocalScript","name":"Slow","parent":"actor-a","server_owned":true},
            {"id":"actor-b","class_name":"Actor","name":"ActorB","parent":"workspace","server_owned":false},
            {"id":"script-b","class_name":"LocalScript","name":"Progress","parent":"actor-b","server_owned":true}
        ],
        "script_sources": {
            "script-a":{"source":"native_delay(); workspace.ActorA:SetAttribute('Advanced', true); task.wait(60)"},
            "script-b":{"source":"workspace.ActorB:SetAttribute('Advanced', true); task.wait(60)"}
        }
    });
    let world = Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()));
    let root = Lua::new();
    scheduler::install(&root, &Rc::new(RefCell::new(scheduler::Scheduler::new()))).unwrap();
    super::super::install_bindings(&root, &world).unwrap();
    let shared = SharedTableRegistry::default();
    super::super::shared_tables::install(&root, &world, shared.clone()).unwrap();
    install(&root, &world, shared.clone()).unwrap();

    // Create both isolated VMs before starting either script so the test-only
    // native callback is installed before the first Actor frame.
    ensure_actor(&root, &world, &shared, "actor-a").unwrap();
    ensure_actor(&root, &world, &shared, "actor-b").unwrap();
    assert!(has_planned_vm(&root, &world));

    let overran = Arc::new(AtomicBool::new(false));
    let marker = overran.clone();
    with_actor(&root, "actor-a", move |actor_lua, _, _| {
        let native_delay = actor_lua.create_function(move |_, ()| {
            std::thread::sleep(Duration::from_millis(80));
            marker.store(true, Ordering::SeqCst);
            Ok(())
        })?;
        actor_lua
            .globals()
            .set("native_delay", native_delay.clone())?;
        let getrenv: Function = actor_lua.globals().get("getrenv")?;
        let engine: mlua::Table = getrenv.call(())?;
        engine.set("native_delay", native_delay)?;
        Ok(())
    })
    .unwrap();

    // Each Actor receives a 20 ms baseline. Actor A's native call blocks for
    // 80 ms, but Actor B must still get and use its independent baseline.
    step(&root, &world, &shared, Duration::from_millis(40)).unwrap();
    assert!(overran.load(Ordering::SeqCst));
    let actor_b_advanced: Value = root
        .load("return workspace.ActorB:GetAttribute('Advanced')")
        .eval()
        .unwrap();
    assert_eq!(actor_b_advanced, Value::Boolean(true));
}
