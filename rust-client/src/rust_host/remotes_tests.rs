use crate::rust_host::{
    scheduler::{self, Scheduler},
    signals,
    world::{self, World},
};
use mlua::Lua;
use serde_json::json;
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

fn runtime() -> (Lua, world::WorldRef, scheduler::SchedulerRef) {
    let snapshot = json!({"version":1,"sequence":1,"services":{"Workspace":"ws"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{},"server_owned":true},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{},"server_owned":true},
        {"id":"event","class_name":"RemoteEvent","name":"Event","parent":"ws","properties":{},"server_owned":true},
        {"id":"function","class_name":"RemoteFunction","name":"Function","parent":"ws","properties":{},"server_owned":true}
    ]});
    let ctx = Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()));
    let lua = Lua::new();
    let scheduler = Rc::new(RefCell::new(Scheduler::new()));
    lua.set_app_data(scheduler.clone());
    scheduler::install(&lua, &scheduler).unwrap();
    signals::install(&lua, &ctx).unwrap();
    world::install(&lua, &ctx).unwrap();
    super::install(&lua, &ctx).unwrap();
    (lua, ctx, scheduler)
}

#[test]
fn cancelled_remote_callers_stay_cancelled_after_reply_or_destruction() {
    for destroy in [false, true] {
        let (lua, ctx, sched) = runtime();
        lua.load("cancelled_resumed=false; active_result=nil; cancelled=task.spawn(function() pcall(function() workspace.Function:InvokeServer('cancelled') end); cancelled_resumed=true end); task.spawn(function() active_result=table.pack(pcall(function() return workspace.Function:InvokeServer('active') end)) end)")
            .exec().unwrap();
        let tick = || scheduler::step(&lua, &sched, Instant::now(), Duration::from_millis(100));
        assert!(tick().errors.is_empty());
        let queued = super::drain_outbound(&ctx);
        assert_eq!(queued["remote_server_calls"].as_array().unwrap().len(), 2);
        lua.load("task.cancel(cancelled)").exec().unwrap();
        if destroy {
            super::cleanup_instance(&lua, &ctx, "function").unwrap();
        } else {
            assert_eq!(super::complete_server_results(&lua, &ctx, &[
                json!({"node":"function","request_id":1,"ok":true,"values":[1]}),
                json!({"node":"function","request_id":2,"ok":true,"values":[2,null,"done"]}),
            ]).unwrap(), 2);
        }
        let report = tick();
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(
            !lua.globals().get::<bool>("cancelled_resumed").unwrap(),
            "destroy={destroy}"
        );
        assert_eq!(super::active_requests(&lua).unwrap(), json!([]));
        if destroy {
            lua.load(
                "assert(active_result[1] == false and string.find(active_result[2], 'destroyed'))",
            )
            .exec()
            .unwrap();
        } else {
            lua.load("assert(active_result.n == 4 and active_result[1] == true and active_result[2] == 2 and active_result[3] == nil and active_result[4] == 'done')").exec().unwrap();
        }
    }
}

#[test]
fn cancelled_remote_callers_release_unsent_queue_capacity() {
    let (lua, ctx, sched) = runtime();
    lua.load("resumed=0; function start() caller=task.spawn(function() workspace.Function:InvokeServer('request'); resumed+=1 end) end; function cancel() task.cancel(caller) end")
        .exec().unwrap();
    let start: mlua::Function = lua.globals().get("start").unwrap();
    let cancel: mlua::Function = lua.globals().get("cancel").unwrap();
    for _ in 0..70 {
        start.call::<()>(()).unwrap();
        let report = scheduler::step(&lua, &sched, Instant::now(), Duration::from_millis(100));
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        cancel.call::<()>(()).unwrap();
    }
    start.call::<()>(()).unwrap();
    assert!(
        scheduler::step(&lua, &sched, Instant::now(), Duration::from_millis(100))
            .errors
            .is_empty()
    );
    let queued = super::drain_outbound(&ctx);
    assert_eq!(queued["remote_server_calls"].as_array().unwrap().len(), 1);
    assert_eq!(queued["remote_server_calls"][0]["request_id"], 71);
    assert_eq!(super::active_requests(&lua).unwrap(), json!([71]));
    assert_eq!(lua.globals().get::<i64>("resumed").unwrap(), 0);
}

#[test]
fn frame_releases_cancelled_remote_calls_without_a_reply_and_preserves_active_calls() {
    let (lua, ctx, sched) = runtime();
    lua.load("resumed=false; cancelled=task.spawn(function() workspace.Function:InvokeServer('cancelled'); resumed=true end); task.spawn(function() result=workspace.Function:InvokeServer('active') end)")
        .exec().unwrap();
    assert!(
        scheduler::step(&lua, &sched, Instant::now(), Duration::from_millis(100))
            .errors
            .is_empty()
    );
    lua.load("task.cancel(cancelled)").exec().unwrap();
    let report = crate::rust_host::frame_phases::step(
        &lua,
        &ctx,
        &sched,
        Instant::now(),
        1.0 / 60.0,
        Duration::from_millis(100),
    )
    .unwrap();
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(super::active_requests(&lua).unwrap(), json!([2]));
    let queued = super::drain_outbound(&ctx);
    assert_eq!(queued["remote_server_calls"].as_array().unwrap().len(), 1);
    assert_eq!(queued["remote_server_calls"][0]["request_id"], 2);
    super::complete_server_results(
        &lua,
        &ctx,
        &[json!({"node":"function","request_id":2,"ok":true,"values":[42]})],
    )
    .unwrap();
    assert!(
        scheduler::step(&lua, &sched, Instant::now(), Duration::from_millis(100))
            .errors
            .is_empty()
    );
    lua.load("assert(resumed == false and result == 42)")
        .exec()
        .unwrap();
}

#[test]
fn fires_event_with_bounded_remote_table_shapes_and_drains_once() {
    let (lua, ctx, _) = runtime();
    lua.load("workspace.Event:FireServer('hi', {1, 2}, {x = true}, {})")
        .exec()
        .unwrap();
    let events = super::drain_outbound(&ctx);
    assert_eq!(
        events["remote_server_events"],
        json!([{
            "node":"event","arguments":["hi",{"$remote_array":[1.0,2.0]},
            {"$remote_map":[["x",true]]},{"$remote_array":[]}]
        }])
    );
    assert_eq!(
        super::drain_outbound(&ctx)["remote_server_events"],
        json!([])
    );
}

#[test]
fn server_invoke_parks_then_resumes_with_the_exact_result_tuple() {
    let (lua, ctx, scheduler_ref) = runtime();
    lua.load("result = {}; task.spawn(function() local a,b = workspace.Function:InvokeServer('q'); result={a,b} end)").exec().unwrap();
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    let queued = super::drain_outbound(&ctx);
    let call = &queued["remote_server_calls"][0];
    assert_eq!(call["node"], "function");
    assert_eq!(call["request_id"], 1);
    assert_eq!(call["arguments"], json!(["q"]));
    assert_eq!(
        super::complete_server_results(
            &lua,
            &ctx,
            &[json!({
                "node":"function","request_id":1,"ok":true,"values":[7,"done"]
            })]
        )
        .unwrap(),
        1
    );
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    let result: mlua::Table = lua.globals().get("result").unwrap();
    let value: (i64, String) = (result.get(1).unwrap(), result.get(2).unwrap());
    assert_eq!(value, (7, "done".into()));
}

#[test]
fn hooked_instance_namecall_forwards_remote_invoke_yield_and_error_arity() {
    let (lua, ctx, scheduler_ref) = runtime();
    // The focused runtime helper installs the world and remote bindings; add
    // the same closure hooks used by the production host before hooking the
    // actual RemoteFunction userdata below.
    crate::rust_host::closures::install(&lua).unwrap();
    lua.load(
        r#"
        hook_methods = {}
        local old
        old = hookmetamethod(workspace.Function, '__namecall', function(self, ...)
            local method = getnamecallmethod()
            hook_methods[#hook_methods + 1] = method
            return old(self, ...)
        end)
        success = table.pack()
        failure = table.pack()
        task.spawn(function()
            success = table.pack(pcall(function()
                return workspace.Function:InvokeServer('success')
            end))
        end)
        task.spawn(function()
            failure = table.pack(pcall(function()
                return workspace.Function:InvokeServer('failure')
            end))
        end)
        "#,
    )
    .exec()
    .unwrap();

    let report = scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let queued = super::drain_outbound(&ctx);
    let calls = queued["remote_server_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["node"], "function");
    assert_eq!(calls[0]["request_id"], 1);
    assert_eq!(calls[0]["arguments"], json!(["success"]));
    assert_eq!(calls[1]["node"], "function");
    assert_eq!(calls[1]["request_id"], 2);
    assert_eq!(calls[1]["arguments"], json!(["failure"]));

    assert_eq!(
        super::complete_server_results(
            &lua,
            &ctx,
            &[
                json!({"node":"function","request_id":1,"ok":true,"values":[7,null,"done"]}),
                json!({"node":"function","request_id":2,"ok":false,"error":"server rejected"}),
            ],
        )
        .unwrap(),
        2
    );
    let resumed = scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    assert!(resumed.errors.is_empty(), "{:?}", resumed.errors);
    lua.load(
        r#"
        assert(#hook_methods == 2)
        assert(hook_methods[1] == 'InvokeServer' and hook_methods[2] == 'InvokeServer')
        assert(success.n == 4 and success[1] == true)
        assert(success[2] == 7 and success[3] == nil and success[4] == 'done')
        assert(failure.n == 2 and failure[1] == false)
        assert(string.find(failure[2], 'server rejected', 1, true) ~= nil)
        "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn server_invoke_rejects_mismatched_receiver_and_fails_only_matching_pending_id() {
    let (lua, ctx, scheduler_ref) = runtime();
    lua.load("task.spawn(function() workspace.Function:InvokeServer() end)")
        .exec()
        .unwrap();
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    assert_eq!(
        super::complete_server_results(
            &lua,
            &ctx,
            &[json!({
                "node":"event","request_id":1,"ok":true,"values":[]
            })]
        )
        .unwrap(),
        0
    );
    assert_eq!(
        super::complete_server_results(
            &lua,
            &ctx,
            &[json!({
                "node":"function","request_id":1,"ok":false,"error":"no server reply"
            })]
        )
        .unwrap(),
        1
    );
}

#[test]
fn incoming_server_callback_runs_and_serializes_real_results() {
    let (lua, ctx, scheduler_ref) = runtime();
    lua.load("workspace.Function.OnClientInvoke = function(a, b) task.wait(0); return a+b, nil, {ok=true} end")
        .exec()
        .unwrap();
    super::queue_client_invocation(&lua, &ctx, "function", 41, json!([2, 3])).unwrap();
    super::dispatch_client_invocations(&lua, &ctx).unwrap();
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now() + Duration::from_millis(1),
        Duration::from_millis(50),
    );
    let replies = super::collect_client_invocations(&lua, &ctx, &scheduler_ref).unwrap();
    assert_eq!(
        replies,
        vec![json!({"node":"function","request_id":41,"ok":true,
        "values":[5.0,null,{"$remote_map":[["ok",true]]}]})]
    );
}

#[test]
fn incoming_server_callback_error_preserves_raw_multiline_message() {
    let (lua, ctx, scheduler_ref) = runtime();
    lua.load("workspace.Function.OnClientInvoke = function() error('expected failure\\nsecond line') end")
        .set_name("=persistent-source")
        .exec()
        .unwrap();
    super::queue_client_invocation(&lua, &ctx, "function", 42, json!([])).unwrap();
    super::dispatch_client_invocations(&lua, &ctx).unwrap();
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    let replies = super::collect_client_invocations(&lua, &ctx, &scheduler_ref).unwrap();
    assert_eq!(
        replies,
        vec![json!({
            "node":"function",
            "request_id":42,
            "ok":false,
            "error":"persistent-source:1: expected failure\nsecond line"
        })]
    );
}

#[test]
fn callback_error_conversion_does_not_execute_authored_metamethods_in_host_drain() {
    let (lua, ctx, scheduler_ref) = runtime();
    lua.load(
        r#"
        tostring_called = false
        workspace.Function.OnClientInvoke = function()
            error(setmetatable({}, {__tostring = function()
                tostring_called = true
                error('must remain inside the scheduler')
            end}))
        end
    "#,
    )
    .exec()
    .unwrap();
    super::queue_client_invocation(&lua, &ctx, "function", 43, json!([])).unwrap();
    super::dispatch_client_invocations(&lua, &ctx).unwrap();
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    let replies = super::collect_client_invocations(&lua, &ctx, &scheduler_ref).unwrap();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["ok"], false);
    assert_eq!(replies[0]["request_id"], 43);
    assert_eq!(
        replies[0]["error"],
        "RemoteFunction callback failed with a non-string error"
    );
    assert!(!lua.globals().get::<bool>("tostring_called").unwrap());
}

#[test]
fn replication_events_fire_remote_signals_and_queue_remote_function_invokes() {
    let (lua, ctx, scheduler_ref) = runtime();
    lua.load("workspace.Event.OnClientEvent:Connect(function(value) received=value end); workspace.Function.OnClientInvoke=function(x) return x+1 end")
        .exec().unwrap();
    let delta = json!({"version":1,"delta":true,"sequence":5,"actions":[
        {"sequence":4,"kind":"event","id":"event","name":"OnClientEvent","arguments":["hello"]},
        {"sequence":5,"kind":"event","id":"function","name":"RemoteOnInvokeClient","arguments":[41,[2]]}
    ]});
    let report = super::process_delta_events(&lua, &ctx, &delta, 1).unwrap();
    assert_eq!(report.received, 2);
    assert_eq!(report.queued_invocations, 1);
    super::dispatch_client_invocations(&lua, &ctx).unwrap();
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    let replies = super::collect_client_invocations(&lua, &ctx, &scheduler_ref).unwrap();
    assert_eq!(
        replies,
        vec![json!({"node":"function","request_id":41,"ok":true,"values":[3.0]})]
    );
    assert_eq!(lua.globals().get::<String>("received").unwrap(), "hello");
    assert_eq!(
        super::process_delta_events(&lua, &ctx, &delta, 5)
            .unwrap()
            .received,
        0
    );
}

#[test]
fn typed_remote_event_variants_are_normalized_at_the_callback_boundary() {
    let (lua, ctx, scheduler_ref) = runtime();
    lua.load("workspace.Event.OnClientEvent:Connect(function(value) received=value end)")
        .exec()
        .unwrap();
    let typed = json!({"$remote_variant":{"type":30,"value":[
        {"$remote_variant":{"type":12,"value":2.5}},
        {"$remote_variant":{"type":2,"value":"ok"}}
    ]}});
    let normalized = rove_client_core::runtime_delta::normalize_remote_variant(&typed, 0).unwrap();
    let delta = json!({"version":1,"delta":true,"sequence":3,"actions":[
        {"sequence":2,"kind":"event","id":"event","name":"OnClientEvent","arguments":[typed]},
        {"sequence":3,"kind":"event","id":"event","name":"OnClientEvent","arguments":[normalized]}
    ]});

    let report = super::process_delta_events(&lua, &ctx, &delta, 1).unwrap();
    assert_eq!(report.received, 2);
    assert_eq!(report.rejected_payloads, 0);
    let stepped = scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    assert!(stepped.errors.is_empty(), "{:?}", stepped.errors);
    lua.load("assert(type(received) == 'table' and received[1] == 2.5 and received[2] == 'ok')")
        .exec()
        .unwrap();
}

#[test]
fn unobserved_server_events_do_not_decode_arguments_or_replay_to_late_listeners() {
    let (lua, ctx, scheduler_ref) = runtime();
    let unknown = json!([{"$unsupported_remote_type":7}]);
    super::fire_client_event(&lua, &ctx, "event", &unknown).unwrap();
    let delta = json!({"version":1,"delta":true,"sequence":2,"actions":[
        {"sequence":2,"kind":"event","id":"event","name":"OnClientEvent","arguments":unknown}
    ]});
    assert_eq!(
        super::process_delta_events(&lua, &ctx, &delta, 1)
            .unwrap()
            .received,
        1
    );
    lua.load(
        "received=0; connection=workspace.Event.OnClientEvent:Connect(function() received+=1 end)",
    )
    .exec()
    .unwrap();
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    assert_eq!(lua.globals().get::<i64>("received").unwrap(), 0);
    assert!(super::fire_client_event(&lua, &ctx, "event", &unknown).is_err());
    lua.load("connection:Disable()").exec().unwrap();
    super::fire_client_event(&lua, &ctx, "event", &unknown).unwrap();
    lua.load("connection:Enable()").exec().unwrap();
    super::fire_client_event(&lua, &ctx, "event", &json!(["delivered"])).unwrap();
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    assert_eq!(lua.globals().get::<i64>("received").unwrap(), 1);
    lua.load("connection:Disconnect()").exec().unwrap();
    super::fire_client_event(&lua, &ctx, "event", &unknown).unwrap();
}

#[test]
fn unsupported_remote_event_is_reported_and_does_not_block_later_event() {
    let (lua, ctx, scheduler_ref) = runtime();
    crate::rust_host::source_helpers::install(&lua, &ctx).unwrap();
    lua.load("warn=function() error('hooked warn must not run') end; received={}; workspace.Event.OnClientEvent:Connect(function(value) table.insert(received, value) end)")
        .exec().unwrap();
    let delta = json!({"version":1,"delta":true,"sequence":3,"actions":[
        {"sequence":2,"kind":"event","id":"event","name":"OnClientEvent","arguments":[["safe-prefix",{"$unsupported_remote_type":7}]]},
        {"sequence":3,"kind":"event","id":"event","name":"OnClientEvent","arguments":[["kept-going",[true,9]]]}
    ]});
    let report = super::process_delta_events(&lua, &ctx, &delta, 1).unwrap();
    assert_eq!(report.received, 2);
    assert_eq!(report.rejected_payloads, 1);
    let stepped = scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    assert!(stepped.errors.is_empty(), "{:?}", stepped.errors);
    lua.load("assert(#received == 1 and type(received[1]) == 'table' and received[1][1] == 'kept-going' and received[1][2][1] == true and received[1][2][2] == 9)")
        .exec().unwrap();
    let warnings = crate::rust_host::source_helpers::drain_console(&lua);
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].0, "warn");
    assert!(warnings[0].1.contains("RemoteEvent.OnClientEvent"));
    assert!(warnings[0].1.contains("argument #1"));
    assert!(warnings[0].1.contains("$[2]"));
    assert!(warnings[0].1.contains("$unsupported_remote_type"));
    assert!(!warnings[0].1.contains("7"));
}

#[test]
fn unsupported_remote_function_result_fails_only_its_waiter() {
    let (lua, ctx, scheduler_ref) = runtime();
    crate::rust_host::source_helpers::install(&lua, &ctx).unwrap();
    lua.load(
        "bad=table.pack(); good=table.pack(); task.spawn(function() bad=table.pack(pcall(function() return workspace.Function:InvokeServer('bad') end)) end); task.spawn(function() good=table.pack(pcall(function() return workspace.Function:InvokeServer('good') end)) end)",
    )
    .exec().unwrap();
    let stepped = scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    assert!(stepped.errors.is_empty(), "{:?}", stepped.errors);
    assert_eq!(super::active_requests(&lua).unwrap(), json!([1, 2]));
    assert_eq!(
        super::complete_server_results(
            &lua,
            &ctx,
            &[
                json!({"node":"function","request_id":1,"ok":true,"values":[["safe-prefix",{"$unsupported_remote_type":7}]]}),
                json!({"node":"function","request_id":2,"ok":true,"values":[["still-alive",3]]}),
            ],
        )
        .unwrap(),
        2
    );
    let resumed = scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    assert!(resumed.errors.is_empty(), "{:?}", resumed.errors);
    lua.load(
        "assert(bad.n == 2 and bad[1] == false and string.find(bad[2], 'argument #1', 1, true)); assert(good.n == 2 and good[1] == true and type(good[2]) == 'table' and good[2][1] == 'still-alive' and good[2][2] == 3)",
    )
    .exec().unwrap();
    let warnings = crate::rust_host::source_helpers::drain_console(&lua);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].1.contains("RemoteFunction.InvokeServer result"));
    assert!(warnings[0].1.contains("$[2]"));
    assert!(warnings[0].1.contains("$unsupported_remote_type"));
    assert!(!warnings[0].1.contains("7"));
}

#[test]
fn malformed_remote_event_envelope_remains_an_error() {
    let (lua, ctx, _) = runtime();
    let delta = json!({"version":1,"delta":true,"sequence":2,"actions":[
        {"sequence":2,"kind":"event","id":"event","name":"OnClientEvent","arguments":"not-a-tuple"}
    ]});
    assert!(super::process_delta_events(&lua, &ctx, &delta, 1).is_err());
}

#[test]
fn incoming_remote_event_still_resumes_a_waiter_without_a_callback() {
    let (lua, ctx, scheduler_ref) = runtime();
    lua.load("task.spawn(function() received=workspace.Event.OnClientEvent:Wait() end)")
        .exec()
        .unwrap();
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    super::fire_client_event(&lua, &ctx, "event", &json!(["wake"])).unwrap();
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    assert_eq!(lua.globals().get::<String>("received").unwrap(), "wake");
}

#[test]
fn cleanup_cancels_pending_and_incoming_calls_after_local_destruction() {
    let (lua, ctx, sched) = runtime();
    lua.load(r#"
        remote=workspace.Function
        remote.OnClientInvoke=function() error('destroyed callback must not run') end
        task.spawn(function() ok,message=pcall(function() return remote:InvokeServer('pending') end) end)
    "#).exec().unwrap();
    scheduler::step(&lua, &sched, Instant::now(), Duration::from_millis(50));
    super::queue_client_invocation(&lua, &ctx, "function", 91, json!([])).unwrap();
    assert_eq!(super::active_requests(&lua).unwrap(), json!([1]));
    world::destroy_instance(&lua, &ctx, "function").unwrap();
    // Local destruction releases the outbound caller immediately. The incoming
    // request stays queued only so the host can return its destruction error.
    assert_eq!(super::active_requests(&lua).unwrap(), json!([]));
    assert!(super::app_state(&lua)
        .unwrap()
        .borrow()
        .callbacks
        .is_empty());
    assert_eq!(
        super::drain_outbound(&ctx)["remote_server_calls"],
        json!([])
    );
    assert_eq!(super::cleanup_destroyed_instances(&lua, &ctx).unwrap(), 1);
    super::dispatch_client_invocations(&lua, &ctx).unwrap();
    let step = scheduler::step(&lua, &sched, Instant::now(), Duration::from_millis(50));
    assert!(step.errors.is_empty(), "{:?}", step.errors);
    assert_eq!(lua.globals().get::<bool>("ok").unwrap(), false);
    assert!(lua
        .globals()
        .get::<String>("message")
        .unwrap()
        .contains("destroyed"));
    assert_eq!(super::active_requests(&lua).unwrap(), json!([]));
    assert!(super::app_state(&lua)
        .unwrap()
        .borrow()
        .callbacks
        .is_empty());
    let replies = super::collect_client_invocations(&lua, &ctx, &sched).unwrap();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["ok"], false);
    assert_eq!(replies[0]["request_id"], 91);
    assert_eq!(super::cleanup_destroyed_instances(&lua, &ctx).unwrap(), 0);
}

#[test]
fn cleanup_preserves_live_callbacks_and_releases_callbacks_for_purged_nodes() {
    let (lua, ctx, _) = runtime();
    lua.load("workspace.Function.OnClientInvoke=function(value) return value end")
        .exec()
        .unwrap();
    assert_eq!(super::cleanup_destroyed_instances(&lua, &ctx).unwrap(), 0);
    assert!(super::app_state(&lua)
        .unwrap()
        .borrow()
        .callbacks
        .contains_key("function"));
    ctx.borrow_mut().nodes.remove("function");
    super::cleanup_destroyed_instances(&lua, &ctx).unwrap();
    assert!(super::app_state(&lua)
        .unwrap()
        .borrow()
        .callbacks
        .is_empty());
}

#[test]
fn deferred_client_callback_preserves_its_registrars_budget_and_default_cap() {
    for extended in [false, true] {
        let (lua, ctx, sched) = runtime();
        lua.globals()
            .set(
                "callback_work",
                lua.create_function(|_, ()| {
                    std::thread::sleep(Duration::from_millis(300));
                    Ok(())
                })
                .unwrap(),
            )
            .unwrap();
        let registration = lua.load("workspace.Function.OnClientInvoke=function() callback_work(); local sum=0; for i=1,10000 do sum+=i end; return sum end").into_function().unwrap();
        let mut policy = scheduler::TaskPolicy::default();
        if extended {
            policy.total = Duration::from_secs(2);
        }
        scheduler::enqueue_with_policy(&lua, registration, mlua::MultiValue::new(), policy)
            .unwrap();
        scheduler::step(&lua, &sched, Instant::now(), Duration::from_millis(50));
        // Invocation happens after the registering thread has completed.
        super::queue_client_invocation(&lua, &ctx, "function", 17, json!([])).unwrap();
        super::dispatch_client_invocations(&lua, &ctx).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let reply = loop {
            scheduler::step(&lua, &sched, Instant::now(), Duration::from_millis(5));
            let replies = super::collect_client_invocations(&lua, &ctx, &sched).unwrap();
            if let Some(reply) = replies.first() {
                break reply.clone();
            }
            assert!(Instant::now() < deadline, "callback did not complete");
        };
        assert_eq!(reply["ok"], extended, "{reply}");
        if extended {
            assert_eq!(reply["values"][0].as_f64(), Some(50005000.0));
        } else {
            assert!(
                reply["error"].as_str().unwrap().contains("CPU budget"),
                "{reply}"
            );
        }
    }
}
