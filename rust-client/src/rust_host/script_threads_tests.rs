use super::*;
use crate::rust_host::{closures, scheduler, world};
use mlua::ObjectLike;
use mlua::{Function, MultiValue, Table};
use serde_json::json;
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

fn setup() -> (Lua, scheduler::SchedulerRef, world::WorldRef, Value) {
    let snapshot = json!({"version":1,"sequence":0,"nodes":[
        {"id":"game","class_name":"DataModel","name":"game"},
        {"id":"script","class_name":"Script","name":"owner","parent":"game"}
    ]});
    let world = Rc::new(RefCell::new(
        world::World::from_snapshot(&snapshot, None).unwrap(),
    ));
    let lua = Lua::new();
    world::install(&lua, &world).unwrap();
    closures::install(&lua).unwrap();
    let scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
    scheduler::install(&lua, &scheduler).unwrap();
    super::super::lifecycle::install(&lua, &world).unwrap();
    install(&lua).unwrap();
    let script = world::instance(&lua, &world, "script").unwrap();
    (lua, scheduler, world, script)
}

fn registry_contains(lua: &Lua, thread: &Thread) -> bool {
    lua.globals()
        .get::<Function>("getreg")
        .unwrap()
        .call::<Table>(())
        .unwrap()
        .pairs::<Value, Value>()
        .any(|row| row.is_ok_and(|(_, value)| matches!(value, Value::Thread(ref candidate) if candidate.to_pointer() == thread.to_pointer())))
}

#[test]
fn script_owner_survives_mutated_environment_and_destroyed_script_instance() {
    let (lua, scheduler, world, script) = setup();
    let environment = lua.create_table().unwrap();
    environment.set("script", script.clone()).unwrap();
    let fallback = lua.create_table().unwrap();
    fallback.set("__index", lua.globals()).unwrap();
    environment.set_metatable(Some(fallback)).unwrap();
    let function: Function = lua
        .load(
            r#"
            local original_script = script
            task.spawn(function()
                script = 'junk'
                spawn_owner = getcallingscript()
                spawn_thread = getcurrentthread()
                task.wait(60)
            end)
            task.defer(function()
                script = 'junk'
                defer_owner = getcallingscript()
                defer_thread = getcurrentthread()
                task.wait(60)
            end)
            -- Give both child coroutines a scheduler turn so Destroy has
            -- existing owner work to clean up, not merely queued callbacks.
            task.wait(0.01)
            original_script:Destroy()
            script = 'junk'
            parent_owner = getcallingscript()
            task.wait(60)
        "#,
        )
        .set_environment(environment.clone())
        .into_function()
        .unwrap();
    let parent =
        scheduler::enqueue_owned(&lua, function, MultiValue::new(), "script:script".into())
            .unwrap();
    assert!(registry_contains(&lua, &parent));

    let started_at = Instant::now();
    let step = scheduler::step(&lua, &scheduler, started_at, Duration::from_millis(25));
    assert!(step.errors.is_empty(), "{:?}", step.errors);
    let spawn: Thread = environment.get("spawn_thread").unwrap();
    let defer: Thread = environment.get("defer_thread").unwrap();
    for thread in [&parent, &spawn, &defer] {
        assert!(
            registry_contains(&lua, thread),
            "active script-owned thread missing from real registry"
        );
        let owner = scheduler::script_for_thread(&lua, thread).unwrap();
        assert!(world::compare_instance_references(&owner, &script).unwrap());
    }
    assert!(matches!(
        spawn.status(),
        mlua::thread::ThreadStatus::Resumable
    ));
    assert!(matches!(
        defer.status(),
        mlua::thread::ThreadStatus::Resumable
    ));

    // The parent resumes after its short wait and destroys its owner. Its
    // current thread survives, while already-running child waiters are closed.
    let destroy_step = scheduler::step(
        &lua,
        &scheduler,
        started_at + Duration::from_secs(1),
        Duration::from_millis(25),
    );
    assert!(destroy_step.errors.is_empty(), "{:?}", destroy_step.errors);
    for name in ["parent_owner", "spawn_owner", "defer_owner"] {
        let owner: Value = environment.get(name).unwrap();
        assert!(
            world::compare_instance_references(&owner, &script).unwrap(),
            "{name}"
        );
        let name: String = owner.as_userdata().unwrap().get("Name").unwrap();
        assert_eq!(
            name, "owner",
            "destroying the source must not erase the handle metadata"
        );
    }
    assert_eq!(world.borrow().nodes.get("script").unwrap().destroyed, true);
    assert_eq!(parent.status(), mlua::thread::ThreadStatus::Resumable);
    assert_ne!(spawn.status(), mlua::thread::ThreadStatus::Resumable);
    assert_ne!(defer.status(), mlua::thread::ThreadStatus::Resumable);
    assert!(registry_contains(&lua, &parent));
    assert!(!registry_contains(&lua, &spawn));
    assert!(!registry_contains(&lua, &defer));
    let from_thread: Value = lua
        .globals()
        .get::<Function>("getscriptfromthread")
        .unwrap()
        .call(spawn.clone())
        .unwrap();
    assert!(world::compare_instance_references(&from_thread, &script).unwrap());

    scheduler::step(
        &lua,
        &scheduler,
        started_at + Duration::from_secs(62),
        Duration::from_millis(25),
    );
    for thread in [&parent, &spawn, &defer] {
        assert!(
            !registry_contains(&lua, thread),
            "finished script thread retained in registry"
        );
    }
}

#[test]
fn script_thread_owner_metadata_is_isolated_between_luau_vms() {
    let (lua, scheduler, _world, script) = setup();
    let env = lua.create_table().unwrap();
    env.set("script", "not an Instance").unwrap();
    let function: Function = lua
        .load("return getcallingscript()")
        .set_environment(env)
        .into_function()
        .unwrap();
    let unowned = scheduler::enqueue(&lua, function, MultiValue::new()).unwrap();
    assert!(!registry_contains(&lua, &unowned));
    let current = lua
        .load("return getcallingscript()")
        .eval::<Value>()
        .unwrap();
    assert!(current.is_nil());

    let (other, _other_scheduler, _other_world, _other_script) = setup();
    assert!(other
        .load("return getcallingscript()")
        .eval::<Value>()
        .unwrap()
        .is_nil());
    assert!(!registry_contains(&other, &unowned));

    let owner_env = lua.create_table().unwrap();
    owner_env.set("script", script).unwrap();
    let fallback = lua.create_table().unwrap();
    fallback.set("__index", lua.globals()).unwrap();
    owner_env.set_metatable(Some(fallback)).unwrap();
    let owner_task: Function = lua
        .load("task.wait(60)")
        .set_environment(owner_env)
        .into_function()
        .unwrap();
    let owned =
        scheduler::enqueue_owned(&lua, owner_task, MultiValue::new(), "script:script".into())
            .unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(25));
    let running_in_owner_vm: bool = lua
        .load("table.find(getrunningscripts(), game.owner) ~= nil")
        .eval()
        .unwrap();
    assert!(running_in_owner_vm);
    assert_eq!(owned.status(), mlua::thread::ThreadStatus::Resumable);
    let running_in_other_vm: bool = other.load("#getrunningscripts() > 0").eval().unwrap();
    assert!(
        !running_in_other_vm,
        "running scripts must stay within their VM"
    );
}

#[test]
fn ordinary_coroutines_inherit_calling_script_and_running_script_visibility() {
    let (lua, scheduler, world, script) = setup();
    let environment = lua.create_table().unwrap();
    environment.set("script", script.clone()).unwrap();
    let fallback = lua.create_table().unwrap();
    fallback.set("__index", lua.globals()).unwrap();
    environment.set_metatable(Some(fallback)).unwrap();
    let function: Function = lua
        .load(
            r#"
            -- The visible global is mutable; lineage must come from thread
            -- context rather than reading this value again in each coroutine.
            script = 'changed by source'
            local child = coroutine.create(function()
                coroutine_owner = getcallingscript()
                coroutine.yield()
            end)
            local resumed = coroutine.resume(child)
            assert(resumed)
            child_thread = child

            local wrapped = coroutine.wrap(function()
                wrapped_owner = getcallingscript()
            end)
            wrapped()
            parent_owner = getcallingscript()
        "#,
        )
        .set_environment(environment.clone())
        .into_function()
        .unwrap();
    let parent =
        scheduler::enqueue_owned(&lua, function, MultiValue::new(), "script:script".into())
            .unwrap();
    let started = Instant::now();
    let step = scheduler::step(&lua, &scheduler, started, Duration::from_millis(25));
    assert!(step.errors.is_empty(), "{:?}", step.errors);

    for name in ["coroutine_owner", "wrapped_owner", "parent_owner"] {
        let owner: Value = environment.get(name).unwrap();
        assert!(
            world::compare_instance_references(&owner, &script).unwrap(),
            "{name}"
        );
    }
    let child: Thread = environment.get("child_thread").unwrap();
    assert_eq!(child.status(), mlua::thread::ThreadStatus::Resumable);
    assert!(super::running_script_ids(&lua)
        .unwrap()
        .contains(&"script".to_owned()));

    // A started and suspended ordinary coroutine keeps its real source
    // visible after the scheduler-owned parent returns. Completion removes
    // that last live source from the result.
    assert_eq!(parent.status(), mlua::thread::ThreadStatus::Finished);
    let resume: Function = lua
        .globals()
        .get::<Table>("coroutine")
        .unwrap()
        .get("resume")
        .unwrap();
    let resumed: MultiValue = resume.call(child.clone()).unwrap();
    assert!(resumed.front().unwrap().as_boolean().unwrap());
    assert_eq!(child.status(), mlua::thread::ThreadStatus::Finished);
    assert!(!super::running_script_ids(&lua)
        .unwrap()
        .contains(&"script".to_owned()));
}

#[test]
fn cancelled_script_task_is_not_reported_as_running() {
    let (lua, scheduler, _world, script) = setup();
    let environment = lua.create_table().unwrap();
    let fallback = lua.create_table().unwrap();
    fallback.set("__index", lua.globals()).unwrap();
    environment.set_metatable(Some(fallback)).unwrap();
    environment.set("script", script).unwrap();
    let function: Function = lua
        .load("task.wait(60)")
        .set_environment(environment)
        .into_function()
        .unwrap();
    let thread =
        scheduler::enqueue_owned(&lua, function, MultiValue::new(), "script:script".into())
            .unwrap();
    let started = Instant::now();
    scheduler::step(&lua, &scheduler, started, Duration::from_millis(25));
    assert!(super::running_script_ids(&lua)
        .unwrap()
        .contains(&"script".to_owned()));
    lua.globals()
        .get::<Table>("task")
        .unwrap()
        .get::<Function>("cancel")
        .unwrap()
        .call::<()>(thread)
        .unwrap();
    assert!(!super::running_script_ids(&lua)
        .unwrap()
        .contains(&"script".to_owned()));
}

#[test]
fn full_bindings_keep_nested_resume_preemptible_and_preserve_owner() {
    let snapshot = serde_json::json!({
        "version": 1,
        "sequence": 0,
        "nodes": [
            {"id":"game","class_name":"DataModel","name":"game"},
            {"id":"script","class_name":"Script","name":"owner","parent":"game"}
        ]
    });
    let world = Rc::new(RefCell::new(
        world::World::from_snapshot(&snapshot, None).unwrap(),
    ));
    let lua = Lua::new();
    let scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
    scheduler::install(&lua, &scheduler).unwrap();
    super::super::install_bindings(&lua, &world).unwrap();
    lua.load(
        r#"
        for _, name in ipairs({"create", "resume", "wrap"}) do
            assert(iscclosure(coroutine[name]))
            assert(not isexecutorclosure(coroutine[name]))
        end
    "#,
    )
    .exec()
    .unwrap();

    let side_effects = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&side_effects);
    lua.globals()
        .set(
            "native_tick",
            lua.create_function(move |_, ()| {
                thread::sleep(Duration::from_millis(1));
                observed.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })
            .unwrap(),
        )
        .unwrap();

    let environment = lua.create_table().unwrap();
    environment
        .set("script", world::instance(&lua, &world, "script").unwrap())
        .unwrap();
    let fallback = lua.create_table().unwrap();
    fallback.set("__index", lua.globals()).unwrap();
    environment.set_metatable(Some(fallback)).unwrap();
    let function: Function = lua
        .load(
            r#"
            local child = coroutine.create(function()
                local sum = 0
                for i = 1, 16 do
                    native_tick()
                    sum += i
                end
                child_owner = getcallingscript()
                return sum
            end)
            child_thread = child
            local ok, sum = coroutine.resume(child)
            resumed, child_sum = ok, sum
            parent_owner = getcallingscript()
        "#,
        )
        .set_environment(environment.clone())
        .into_function()
        .unwrap();
    let parent =
        scheduler::enqueue_owned(&lua, function, MultiValue::new(), "script:script".into())
            .unwrap();

    let mut preemptions = 0;
    for _ in 0..100 {
        let report = scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(5));
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        preemptions += report.preempted;
        if parent.status() == mlua::thread::ThreadStatus::Finished {
            break;
        }
        assert!(
            parent.is_resumable(),
            "source task stopped before completion"
        );
    }
    assert!(
        preemptions >= 2,
        "nested resume saw only {preemptions} preemptions"
    );
    assert_eq!(side_effects.load(Ordering::Relaxed), 16);
    assert_eq!(environment.get::<bool>("resumed").unwrap(), true);
    assert_eq!(environment.get::<i64>("child_sum").unwrap(), 136);
    for name in ["child_owner", "parent_owner"] {
        let owner: Value = environment.get(name).unwrap();
        assert!(
            world::compare_instance_references(
                &owner,
                &world::instance(&lua, &world, "script").unwrap()
            )
            .unwrap(),
            "{name}"
        );
    }
}
