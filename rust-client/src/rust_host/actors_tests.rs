use super::*;
#[path = "joint_pose_runtime_tests.rs"]
mod joint_pose_tests;
#[path = "actor_animation_loading_tests.rs"]
mod animation_loading_tests;

fn setup() -> (Lua, WorldRef, SharedTableRegistry) {
    let snapshot = json!({
        "version":1,"sequence":0,
        "services":{"Workspace":"workspace","RunService":"run"},
        "nodes":[
            {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
            {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
            {"id":"run","class_name":"RunService","name":"RunService","parent":"game","properties":{}},
            {"id":"worker","class_name":"Actor","name":"worker","parent":"workspace","properties":{}},
            {"id":"other","class_name":"Actor","name":"other","parent":"workspace","properties":{}}
        ]
    });
    let world = Rc::new(RefCell::new(
        world::World::from_snapshot(&snapshot, None).unwrap(),
    ));
    let lua = Lua::new();
    scheduler::install(&lua, &Rc::new(RefCell::new(scheduler::Scheduler::new()))).unwrap();
    super::super::install_bindings(&lua, &world).unwrap();
    let pool = SharedTableRegistry::default();
    shared_tables::install(&lua, &world, pool.clone()).unwrap();
    install(&lua, &world, pool.clone()).unwrap();
    (lua, world, pool)
}

fn advance(lua: &Lua, world: &WorldRef, pool: &SharedTableRegistry) -> Json {
    step(lua, world, pool, Duration::from_millis(30)).unwrap()
}

#[test]
fn catalog_source_below_actor_causes_automatic_vm_startup() {
    let snapshot = json!({
        "version":1,"sequence":0,
        "services":{"Workspace":"workspace"},
        "script_sources":{"reader":{"source":"script:SetAttribute('Started',true); task.wait(60)"}},
        "nodes":[
            {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
            {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":null,"properties":{}},
            {"id":"actor","class_name":"Actor","name":"Actor","parent":"workspace","properties":{}},
            {"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false}}
        ]
    });
    let world = Rc::new(RefCell::new(
        world::World::from_snapshot(&snapshot, None).unwrap(),
    ));
    assert!(actor_has_runnable_source(&world, "actor"));
    let root = Lua::new();
    let scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
    scheduler::install(&root, &scheduler).unwrap();
    super::super::install_bindings(&root, &world).unwrap();
    let pool = SharedTableRegistry::default();
    shared_tables::install(&root, &world, pool.clone()).unwrap();
    install(&root, &world, pool.clone()).unwrap();
    let statuses = step(&root, &world, &pool, Duration::from_millis(30)).unwrap();
    let actor = statuses
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["actor_id"] == "actor")
        .unwrap();
    assert_eq!(actor["vm_instantiated"], true, "{actor}");
    let started: bool = root
        .load("workspace.Actor.Reader:GetAttribute('Started')")
        .eval()
        .unwrap();
    assert!(started, "{actor}");
    let visible_from_root: bool = root
        .load("table.find(getrunningscripts(), workspace.Actor.Reader) ~= nil")
        .eval()
        .unwrap();
    assert!(!visible_from_root, "Actor scripts belong to their own global state");
    let visible_from_actor = super::with_actor(&root, "actor", |actor_lua, _, _| {
        actor_lua
            .load("table.find(getrunningscripts(), workspace.Actor.Reader) ~= nil")
            .eval::<bool>()
    })
    .unwrap()
    .unwrap();
    assert!(visible_from_actor, "the Actor VM should report its running script");
}

#[test]
fn actor_message_copies_arguments_between_isolated_vms() {
    let (root, world, pool) = setup();
    assert_eq!(actor_id(&root), None);
    start_source(
        &root,
        &world,
        &pool,
        "worker",
        "=worker-script",
        "local actor=workspace:FindFirstChild('worker')\nactor:BindToMessage('copy',function(number, payload) print(number,payload.label,payload.nested[1],payload.nested[2]) end)",
        Duration::from_secs(1),
    )
    .unwrap();
    let ready = advance(&root, &world, &pool);
    assert_eq!(ready.as_array().unwrap().len(), 2);

    root.load(
        "workspace:FindFirstChild('worker'):SendMessage('copy',42,{label='ok',nested={11,12}})",
    )
    .exec()
    .unwrap();
    let result = advance(&root, &world, &pool);
    let actor = result
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["actor_id"] == "worker")
        .unwrap();
    assert_eq!(actor["actor_id"], "worker");
    assert!(
        actor["output"].as_str().unwrap().contains("42\tok\t11\t12"),
        "{actor}"
    );
    assert_eq!(actor["vm_instantiated"], true);
    assert_eq!(diagnostics(&root).unwrap()["queued_messages"], 0);
}

#[test]
fn actor_vms_are_isolated_and_only_created_for_runnable_actors() {
    let (root, world, pool) = setup();
    let first = advance(&root, &world, &pool);
    assert_eq!(first.as_array().unwrap().len(), 2);
    assert_eq!(diagnostics(&root).unwrap()["actor_count"], 2);
    assert_eq!(diagnostics(&root).unwrap()["vm_count"], 0);
    assert_eq!(first[0]["state"], "dormant");
    start_source(
        &root,
        &world,
        &pool,
        "worker",
        "=isolated",
        "_G.actor_only='yes'",
        Duration::from_secs(1),
    )
    .unwrap();
    advance(&root, &world, &pool);
    let root_value: Value = root.globals().get("actor_only").unwrap();
    assert!(matches!(root_value, Value::Nil));
    assert_eq!(actor_id(&root), None);
}

#[test]
fn actor_status_reports_its_vm_generation_and_real_integrity_counters() {
    let (root, world, pool) = setup();
    start_source(
        &root,
        &world,
        &pool,
        "worker",
        "=generation-check",
        "return true",
        Duration::from_secs(1),
    )
    .unwrap();
    let status = advance(&root, &world, &pool);
    let actor = status
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["actor_id"] == "worker")
        .unwrap();
    assert_eq!(actor["runtime_instance_generation"], 1);
    assert_eq!(actor["runtime_thread_integrity"]["version"], 1);
    assert!(
        actor["runtime_thread_integrity"]["checks"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(actor["runtime_protected_integrity"]["version"], 1);
}

#[test]
fn messages_reject_functions_cycles_and_parallel_binding() {
    let (root, world, pool) = setup();
    ensure_actor(&root, &world, &pool, "worker").unwrap();
    let function_result: bool = root
        .load("local a=workspace:FindFirstChild('worker'); local ok=pcall(function() a:SendMessage('x',function() end) end); return ok")
        .eval()
        .unwrap();
    assert!(!function_result);
    let cycle_result: bool = root
        .load("local a=workspace:FindFirstChild('worker'); local t={}; t.self=t; local ok=pcall(function() a:SendMessage('x',t) end); return ok")
        .eval()
        .unwrap();
    assert!(!cycle_result);
    let actor = runtime_ref(&root).unwrap().borrow().actors["worker"].clone();
    let parallel: bool = actor
        .borrow()
        .lua
        .load("local a=workspace:FindFirstChild('worker'); local ok=pcall(function() a:BindToMessageParallel('x',function() end) end); return ok")
        .eval()
        .unwrap();
    assert!(!parallel);
}

#[test]
fn actor_bind_rejects_binding_another_actor() {
    let (root, world, pool) = setup();
    ensure_actor(&root, &world, &pool, "worker").unwrap();
    let actor = runtime_ref(&root).unwrap().borrow().actors["worker"].clone();
    let result: bool = actor
        .borrow()
        .lua
        .load("local a=workspace:FindFirstChild('other'); local ok=pcall(function() a:BindToMessage('x',function() end) end); return ok")
        .eval()
        .unwrap();
    assert!(!result);
}

#[test]
fn queue_is_bounded_and_unbound_messages_wait_then_expire() {
    let (root, world, pool) = setup();
    root.load("workspace:FindFirstChild('worker'):SendMessage('nobody',1)")
        .exec()
        .unwrap();
    advance(&root, &world, &pool);
    assert_eq!(diagnostics(&root).unwrap()["queued_messages"], 1);
    for _ in 0..121 {
        advance(&root, &world, &pool);
    }
    let last = advance(&root, &world, &pool);
    assert_eq!(last.as_array().unwrap().len(), 2);
    assert_eq!(diagnostics(&root).unwrap()["queued_messages"], 0);
    assert_eq!(diagnostics(&root).unwrap()["dropped_messages"], 1);
    assert_eq!(dropped_messages(&root).unwrap(), 1);
}

#[test]
fn existing_actor_receives_a_late_catalog_after_ordinary_world_updates() {
    let (lua, world, pool) = setup();
    start_source(&lua, &world, &pool, "worker", "catalog-fixture", "return true", Duration::from_millis(30)).unwrap();
    advance(&lua, &world, &pool);
    world::apply_delta_with_lua(&lua, &world, &json!({"version":1,"delta":true,"sequence":1,
        "actions":[{"kind":"create","sequence":1,"node":{"id":"late","class_name":"LocalScript",
            "name":"late","parent":"worker","attributes_known":true,"attributes":{},"properties":{"Enabled":true,"Disabled":false}}}]}), None).unwrap();
    advance(&lua, &world, &pool);
    assert_eq!(lua.load("return workspace.worker.late:GetAttribute('Ran')").eval::<Value>().unwrap(), Value::Nil);
    world::apply_delta_with_lua(&lua, &world, &json!({"version":1,"delta":true,"sequence":2,"actions":[],
        "script_sources":{"late":{"source":"script:SetAttribute('Ran',42)"}}}), None).unwrap();
    advance(&lua, &world, &pool);
    assert_eq!(lua.load("return workspace.worker.late:GetAttribute('Ran')").eval::<i64>().unwrap(), 42);
    world::apply_delta_with_lua(&lua, &world, &json!({"version":1,"delta":true,"sequence":3,
        "actions":[{"kind":"set","sequence":3,"id":"late","fields":{"Name":"renamed"}}]}), None).unwrap();
    advance(&lua, &world, &pool);
    assert_eq!(lua.load("return workspace.worker.renamed:GetAttribute('Ran')").eval::<i64>().unwrap(), 42);
}

#[test]
fn getconnections_exposes_real_foreign_actor_records_with_generation_fencing() {
    let (root, world, pool) = setup();
    ensure_actor(&root, &world, &pool, "worker").unwrap();
    let actor_connection = with_actor(&root, "worker", |actor_lua, actor_world, _| {
        let callback = actor_lua.load("return function() end").eval::<Function>()?;
        signals::connect_owned(actor_lua, actor_world, "worker", "Event", callback, "test", false)
    }).unwrap().unwrap();
    with_actor(&root, "worker", |actor_lua, actor_world, _| {
        signals::connect_native(actor_lua, actor_world, "worker", "Event", "native-test")?;
        Ok(())
    }).unwrap().unwrap();
    let signal = signals::signal(&root, &world, "worker", "Event").unwrap();
    root.globals().set("foreign_event", signal).unwrap();
    root.load(r#"
        foreign_rows=getconnections(foreign_event)
        assert(#foreign_rows==2)
        assert(foreign_rows[2].ForeignState and not foreign_rows[2].LuaConnection)
        assert(foreign_rows[2].Function==nil and foreign_rows[2].Thread==nil)
        foreign_connection=foreign_rows[1]
        assert(foreign_connection.ForeignState==true)
        assert(foreign_connection.LuaConnection==true)
        assert(foreign_connection.Function==nil and foreign_connection.Thread==nil)
        assert(foreign_connection.Connected and foreign_connection.Enabled)
        foreign_connection:Disable()
        assert(not foreign_connection.Enabled and foreign_connection.Connected)
        foreign_connection:Enable()
        assert(foreign_connection.Enabled)
    "#).exec().unwrap();
    assert_eq!(root.load("return foreign_connection.Connected").eval::<bool>().unwrap(), true);
    // Removing the owning Actor VM makes the proxy stale; it cannot bind to a
    // later VM that reuses the same Actor ID.
    world.borrow_mut().nodes.get_mut("worker").unwrap().destroyed = true;
    let _ = step(&root, &world, &pool, Duration::from_millis(1)).unwrap();
    root.load("assert(not foreign_connection.Connected); foreign_connection:Enable(); foreign_connection:Disconnect()")
        .exec().unwrap();
    world.borrow_mut().nodes.get_mut("worker").unwrap().destroyed = false;
    ensure_actor(&root, &world, &pool, "worker").unwrap();
    with_actor(&root, "worker", |actor_lua, actor_world, _| {
        let callback = actor_lua.load("return function() end").eval::<Function>()?;
        signals::connect_owned(actor_lua, actor_world, "worker", "Event", callback, "new-incarnation", false)?;
        Ok(())
    }).unwrap().unwrap();
    root.load("assert(not foreign_connection.Connected); local rows=getconnections(foreign_event); assert(#rows==2 and rows[1].Connected and rows[1].ForeignState)").exec().unwrap();
    let _ = actor_connection;
}
