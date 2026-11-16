use super::{
    bindables, errors,
    scheduler::{self, Scheduler},
    signals,
    world::{self, World, WorldRef},
};
use mlua::{Lua, Value};
use serde_json::json;
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

fn fixture() -> (Lua, WorldRef, scheduler::SchedulerRef) {
    let lua = Lua::new();
    let snapshot = json!({"version":1,"sequence":0,"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{},"server_owned":true}
    ]});
    let world = Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()));
    let sched = Rc::new(RefCell::new(Scheduler::new()));
    scheduler::install(&lua, &sched).unwrap();
    errors::install(&lua).unwrap();
    signals::install(&lua, &world).unwrap();
    world::install(&lua, &world).unwrap();
    bindables::install(&lua, &world).unwrap();
    (lua, world, sched)
}

fn step(lua: &Lua, sched: &scheduler::SchedulerRef, now: Instant) -> scheduler::StepReport {
    scheduler::step(lua, sched, now, Duration::from_millis(100))
}

#[test]
fn cancelled_bindable_callers_stay_cancelled_after_completion_or_destruction() {
    for destroy in [false, true] {
        let (lua, world, sched) = fixture();
        lua.load(r#"
            fn = Instance.new('BindableFunction')
            callbacks = 0; cancelled_resumed = false; active_result = nil
            fn.OnInvoke = function(value) task.wait(0); callbacks += 1; return value, nil, 'done' end
            cancelled = task.spawn(function() pcall(function() fn:Invoke(1) end); cancelled_resumed = true end)
            task.spawn(function() active_result = table.pack(pcall(function() return fn:Invoke(2) end)) end)
        "#).exec().unwrap();
        assert!(step(&lua, &sched, Instant::now()).errors.is_empty());
        lua.load("task.cancel(cancelled)").exec().unwrap();
        if destroy {
            let id = world::instance_id(&lua.globals().get::<Value>("fn").unwrap()).unwrap();
            world::destroy_instance(&lua, &world, &id).unwrap();
        }
        let report = step(&lua, &sched, Instant::now() + Duration::from_secs(1));
        assert!(report.errors.is_empty(), "{destroy}: {:?}", report.errors);
        assert!(!lua.globals().get::<bool>("cancelled_resumed").unwrap(), "destroy={destroy}");
        assert_eq!(lua.globals().get::<i64>("callbacks").unwrap(), 2);
        if destroy {
            lua.load("assert(active_result[1] == false and string.find(active_result[2], 'destroyed'))").exec().unwrap();
        } else {
            lua.load("assert(active_result.n == 4 and active_result[1] == true and active_result[2] == 2 and active_result[3] == nil and active_result[4] == 'done')").exec().unwrap();
        }
    }
}

#[test]
fn cancelled_bindable_callers_release_pending_capacity_while_callbacks_continue() {
    let (lua, _world, sched) = fixture();
    lua.load(r#"
        fn = Instance.new('BindableFunction'); gate = Instance.new('BindableEvent')
        resumed = 0; completed = 0
        fn.OnInvoke = function() gate.Event:Wait(); completed += 1; return 42 end
        function start() caller = task.spawn(function() assert(fn:Invoke() == 42); resumed += 1 end) end
        function cancel() task.cancel(caller) end
    "#).exec().unwrap();
    let start: mlua::Function = lua.globals().get("start").unwrap();
    let cancel: mlua::Function = lua.globals().get("cancel").unwrap();
    for _ in 0..514 {
        start.call::<()>(()).unwrap();
        let report = step(&lua, &sched, Instant::now());
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        cancel.call::<()>(()).unwrap();
    }
    start.call::<()>(()).unwrap();
    assert!(step(&lua, &sched, Instant::now()).errors.is_empty());
    lua.load("gate:Fire()").exec().unwrap();
    let report = step(&lua, &sched, Instant::now());
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    lua.load("assert(resumed == 1 and completed == 515)").exec().unwrap();
}

#[test]
fn bindable_event_fire_schedules_local_callbacks_in_order_with_nil_values() {
    let (lua, _world, sched) = fixture();
    lua.load(r#"
        event = Instance.new("BindableEvent")
        called = {}
        event.Event:Connect(function(a,b,c) called[1]="first"; called.a=a; called.b=b; called.c=c end)
        event.Event:Connect(function() called[2]="second" end)
        event:Fire(event, nil, 3)
        assert(called[1] == nil)
    "#).exec().unwrap();
    let report = step(&lua, &sched, Instant::now());
    assert_eq!(report.resumed, 2);
    assert_eq!(
        lua.load("return called[1], called[2], called.a == event, called.b, called.c")
            .eval::<(String, String, bool, Value, i64)>()
            .unwrap(),
        ("first".into(), "second".into(), true, Value::Nil, 3)
    );
}

#[test]
fn bindable_function_returns_exact_multi_value_tuple() {
    let (lua, _world, sched) = fixture();
    lua.load(
        r#"
        fn = Instance.new("BindableFunction")
        fn.OnInvoke = function(a,b) return a+1, nil, b.."!" end
        result = nil
        task.spawn(function()
            local a,b,c = fn:Invoke(41, "ok")
            result = {a=a,b=b,c=c}
        end)
    "#,
    )
    .exec()
    .unwrap();
    step(&lua, &sched, Instant::now());
    assert_eq!(
        lua.load("return result.a, result.b, result.c")
            .eval::<(i64, Value, String)>()
            .unwrap(),
        (42, Value::Nil, "ok!".into())
    );
}

#[test]
fn bindable_function_waits_for_yielding_callback_before_returning_values() {
    let (lua, _world, sched) = fixture();
    lua.load(
        r#"
        fn = Instance.new("BindableFunction")
        fn.OnInvoke = function(value) task.wait(0.01); return value, nil, "after" end
        result = nil
        task.spawn(function() result = table.pack(fn:Invoke("before")) end)
    "#,
    )
    .exec()
    .unwrap();
    step(&lua, &sched, Instant::now());
    assert!(lua.load("return result == nil").eval::<bool>().unwrap());
    step(&lua, &sched, Instant::now() + Duration::from_millis(50));
    assert_eq!(
        lua.load("return result[1], result[2], result[3], result.n")
            .eval::<(String, Value, String, i64)>()
            .unwrap(),
        ("before".into(), Value::Nil, "after".into(), 3)
    );
}

#[test]
fn invoke_waits_for_late_callback_and_nil_clears_future_dispatch() {
    let (lua, _world, sched) = fixture();
    lua.load(
        r#"
        fn = Instance.new("BindableFunction")
        task.spawn(function() first = table.pack(fn:Invoke(41, nil, 'argument')) end)
    "#,
    )
    .exec()
    .unwrap();
    assert!(step(&lua, &sched, Instant::now()).errors.is_empty());
    lua.load("assert(first == nil)").exec().unwrap();
    lua.load(r#"fn.OnInvoke = function(a,b,c) assert(a == 41 and b == nil and c == 'argument'); return a+1, nil, c, nil end"#)
        .exec()
        .unwrap();
    assert!(step(&lua, &sched, Instant::now()).errors.is_empty());
    lua.load("assert(first.n == 4 and first[1] == 42 and first[2] == nil and first[3] == 'argument' and first[4] == nil); fn.OnInvoke = nil").exec().unwrap();
    lua.load(
        r#"
        task.spawn(function() second = fn:Invoke(9) end)
    "#,
    )
    .exec()
    .unwrap();
    assert!(step(&lua, &sched, Instant::now()).errors.is_empty());
    lua.load("assert(second == nil); fn.OnInvoke = function(value) return value*2 end").exec().unwrap();
    assert!(step(&lua, &sched, Instant::now()).errors.is_empty());
    lua.load("assert(second == 18)").exec().unwrap();
}

#[test]
fn bindable_callbacks_are_scoped_to_the_world_identity_and_local_instances() {
    let (lua, first_world, _sched) = fixture();
    let second_world = Rc::new(RefCell::new(World::from_snapshot(
        &json!({"version":1,"sequence":0,"nodes":[
            {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{},"server_owned":true},
            {"id":"local:shared-bindable","class_name":"BindableFunction","name":"BindableFunction","parent":null,"properties":{},"server_owned":false}
        ]}), None).unwrap()));
    bindables::install(&lua, &second_world).unwrap();
    let local_id = first_world
        .borrow()
        .nodes
        .get("local:shared-bindable")
        .map(|n| n.id.clone());
    assert!(local_id.is_none());
    let foreign = world::instance(&lua, &second_world, "local:shared-bindable").unwrap();
    lua.globals().set("foreign", foreign).unwrap();
    lua.load(r#"foreign.OnInvoke = function() return "wrong world" end"#)
        .exec()
        .unwrap();
    // An instance in the first world with the same identifier must not see the callback.
    let local_node = world::Node {
        id: "local:shared-bindable".into(),
        name: "BindableFunction".into(),
        class_name: "BindableFunction".into(),
        parent: None,
        destroyed: false,
        server_owned: false,
        received_creation_complete: false,
        attributes_known: true,
        collection_tags_known: true,
        projected: None,
        data: crate::rust_host::world::NodeData {
            properties: Default::default(),
            local_overrides: Default::default(),
            attributes: Default::default(),
            revisions: Default::default(),
            local_revision: 0,
            collection_tags: Vec::new(),
        }.pack(),
    };
    first_world
        .borrow_mut()
        .nodes
        .insert("local:shared-bindable".into(), local_node);
    let local = world::instance(&lua, &first_world, "local:shared-bindable").unwrap();
    lua.globals().set("local_fn", local).unwrap();
    lua.load(
        r#"
        task.spawn(function() identity_result = local_fn:Invoke() end)
    "#,
    )
    .exec()
    .unwrap();
    let sched = lua
        .app_data_ref::<scheduler::SchedulerRef>()
        .unwrap()
        .clone();
    assert!(step(&lua, &sched, Instant::now()).errors.is_empty());
    lua.load("assert(identity_result == nil); local_fn.OnInvoke = function() return 'right world' end").exec().unwrap();
    assert!(step(&lua, &sched, Instant::now()).errors.is_empty());
    assert_eq!(lua.globals().get::<String>("identity_result").unwrap(), "right world");
}

#[test]
fn late_bindable_dispatch_skips_cancelled_callers_and_keeps_the_selected_callback() {
    let (lua, _world, sched) = fixture();
    lua.load(r#"
        fn=Instance.new('BindableFunction'); calls={}; results={}
        cancelled=task.spawn(function() fn:Invoke('cancelled'); error('cancelled caller resumed') end)
        for i=1,3 do task.spawn(function() results[i]=fn:Invoke(i) end) end
    "#).exec().unwrap();
    assert!(step(&lua, &sched, Instant::now()).errors.is_empty());
    lua.load(r#"
        task.cancel(cancelled)
        fn.OnInvoke=function(value) table.insert(calls,value); task.wait(0); return value+10 end
        fn.OnInvoke=nil
        fn.OnInvoke=function(value) return value+100 end
        task.spawn(function() latest=fn:Invoke(4) end)
    "#).exec().unwrap();
    assert!(step(&lua, &sched, Instant::now()).errors.is_empty());
    assert!(step(&lua, &sched, Instant::now()+Duration::from_secs(1)).errors.is_empty());
    lua.load("assert(table.concat(calls,',') == '1,2,3' and results[1] == 11 and results[2] == 12 and results[3] == 13 and latest == 104)")
        .exec().unwrap();
}

#[test]
fn late_bindable_callback_errors_return_to_the_waiting_caller() {
    let (lua, _world, sched) = fixture();
    lua.load("fn=Instance.new('BindableFunction'); task.spawn(function() result=table.pack(pcall(function() fn:Invoke() end)) end)").exec().unwrap();
    assert!(step(&lua, &sched, Instant::now()).errors.is_empty());
    lua.load("assert(result == nil); fn.OnInvoke=function() task.wait(0); error('late callback failed') end").exec().unwrap();
    assert!(step(&lua, &sched, Instant::now()).errors.is_empty());
    assert!(step(&lua, &sched, Instant::now()+Duration::from_secs(1)).errors.is_empty());
    lua.load("assert(result[1] == false and string.find(result[2],'late callback failed'))").exec().unwrap();
}

#[test]
fn destroying_bindable_before_callback_assignment_releases_only_active_callers() {
    let (lua, _world, sched) = fixture();
    lua.load(r#"
        fn=Instance.new('BindableFunction'); cancelled_resumed=false
        cancelled=task.spawn(function() pcall(function() fn:Invoke() end); cancelled_resumed=true end)
        task.spawn(function() result=table.pack(pcall(function() fn:Invoke() end)) end)
    "#).exec().unwrap();
    assert!(step(&lua, &sched, Instant::now()).errors.is_empty());
    lua.load("assert(result == nil); task.cancel(cancelled); fn:Destroy()").exec().unwrap();
    assert!(step(&lua, &sched, Instant::now()).errors.is_empty());
    lua.load("assert(not cancelled_resumed and result[1] == false and string.find(result[2],'destroyed'))").exec().unwrap();
    assert_eq!(scheduler::snapshot(&sched).active_tasks, 0);
}

#[test]
fn unassigned_bindable_waits_keep_a_bounded_queue_and_recover_after_assignment() {
    let (lua, _world, sched) = fixture();
    // A scheduler slice is time-budgeted. Drive it until ready work is drained
    // instead of assuming 513 callers fit into one 100 ms slice on a busy host.
    let drain_ready = || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while scheduler::snapshot(&sched).ready_queue_entries > 0 {
            assert!(Instant::now() < deadline, "{:?}", scheduler::snapshot(&sched));
            let report = step(&lua, &sched, Instant::now());
            assert!(report.errors.is_empty(), "{:?}", report.errors);
        }
    };
    lua.load(r#"
        fn=Instance.new('BindableFunction'); completed=0; failures=0
        for i=1,513 do
            task.spawn(function()
                local ok,value=pcall(function() return fn:Invoke(i) end)
                if ok then assert(value==i); completed+=1
                else assert(string.find(value,'invocation limit')); failures+=1 end
            end)
        end
    "#).exec().unwrap();
    drain_ready();
    assert_eq!(scheduler::snapshot(&sched).waiting, 512);
    lua.load("assert(completed == 0 and failures == 1); fn.OnInvoke=function(value) return value end").exec().unwrap();
    drain_ready();
    lua.load("assert(completed == 512 and failures == 1)").exec().unwrap();
    assert_eq!(scheduler::snapshot(&sched).active_tasks, 0);
}

#[test]
fn snapshot_bindables_execute_callbacks_entirely_inside_the_luau_vm() {
    let lua = Lua::new();
    let snapshot = json!({"version":1,"sequence":0,"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{},"server_owned":true},
        {"id":"received-event","class_name":"BindableEvent","name":"ReceivedEvent","parent":"game","properties":{},"server_owned":true},
        {"id":"received-function","class_name":"BindableFunction","name":"ReceivedFunction","parent":"game","properties":{},"server_owned":true}
    ]});
    let world = Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()));
    let sched = Rc::new(RefCell::new(Scheduler::new()));
    scheduler::install(&lua, &sched).unwrap();
    errors::install(&lua).unwrap();
    signals::install(&lua, &world).unwrap();
    world::install(&lua, &world).unwrap();
    bindables::install(&lua, &world).unwrap();
    let bootstrap_mutations = world.borrow().effects.get("mutations").cloned();
    lua.globals()
        .set(
            "event",
            world::instance(&lua, &world, "received-event").unwrap(),
        )
        .unwrap();
    lua.globals()
        .set(
            "fn",
            world::instance(&lua, &world, "received-function").unwrap(),
        )
        .unwrap();
    lua.load(
        r#"
        event_calls = 0
        event.Event:Connect(function(value) event_calls = event_calls + value end)
        event:Fire(4)
        fn.OnInvoke = function(value) return value * 2, nil end
        result = nil
        task.spawn(function() result = table.pack(fn:Invoke(9)) end)
    "#,
    )
    .exec()
    .unwrap();
    step(&lua, &sched, Instant::now());
    assert_eq!(
        lua.load("return event_calls, result[1], result[2], result.n")
            .eval::<(i64, i64, Value, i64)>()
            .unwrap(),
        (4, 18, Value::Nil, 2)
    );
    assert_eq!(
        world.borrow().effects.get("mutations"),
        bootstrap_mutations.as_ref(),
        "Bindable execution must not add a world/server mutation"
    );
}

#[test]
fn destroying_a_bindable_wakes_invoke_with_a_local_error() {
    let (lua, world, sched) = fixture();
    lua.load(
        r#"
        fn = Instance.new("BindableFunction")
        fn.OnInvoke = function() task.wait(5); return "late" end
        task.spawn(function()
            local ok, err = pcall(function() fn:Invoke() end)
            invoke_error = not ok and string.find(err, "destroyed while Invoke was waiting") ~= nil
        end)
    "#,
    )
    .exec()
    .unwrap();
    step(&lua, &sched, Instant::now());
    let bindable_id = world
        .borrow()
        .nodes
        .values()
        .find(|node| node.class_name == "BindableFunction")
        .unwrap()
        .id
        .clone();
    world::destroy_instance(&lua, &world, &bindable_id).unwrap();
    step(&lua, &sched, Instant::now() + Duration::from_secs(6));
    assert_eq!(lua.globals().get::<bool>("invoke_error").unwrap(), true);
}
