//! Late callback assignment through the real host command and Actor pumps.
#![cfg(feature = "rust-host")]

use headless_luau::runtime_owner::{successful, Runtime};
use serde_json::{json, Value};

fn call(vm: &mut Runtime, request: Value) -> Value {
    let reply = vm.call(request).unwrap();
    successful(&reply).unwrap_or_else(|error| panic!("{error}: {reply}"));
    assert!(reply["task_errors"].as_array().is_none_or(|errors| errors.is_empty()), "{reply}");
    reply
}

fn execute(vm: &mut Runtime, source: &str) -> Value {
    call(vm, json!({"op":"execute","source":source,"timeout_ms":100}))["values"].clone()
}

fn poll(vm: &mut Runtime) {
    for _ in 0..4 { call(vm, json!({"op":"poll","timeout_ms":100})); }
}

#[test]
fn received_bindable_waits_across_commands_then_returns_the_complete_tuple() {
    let mut vm = Runtime::new(json!({"version":1,"sequence":0,"services":{"Workspace":"ws"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","properties":{}},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
        {"id":"bridge","class_name":"BindableFunction","name":"Bridge","parent":"ws","properties":{},"server_owned":true}
    ]}),32).unwrap();
    execute(&mut vm, "task.spawn(function() result=table.pack(workspace.Bridge:Invoke(41,nil,'argument')) end)");
    poll(&mut vm);
    assert_eq!(execute(&mut vm,"return result == nil"),json!([true]));
    execute(&mut vm,"workspace.Bridge.OnInvoke=function(a,b,c) assert(b==nil and c=='argument'); task.wait(0); return a+1,nil,c,nil end");
    poll(&mut vm);
    assert_eq!(execute(&mut vm,"return result.n,result[1],result[2],result[3],result[4]"),json!([4.0,42.0,null,"argument",null]));
}

#[test]
fn actor_waits_for_its_own_callback_even_after_root_assigns_one() {
    let mut vm = Runtime::new(json!({"version":1,"sequence":0,"services":{"Workspace":"ws"},
        "script_sources":{"reader":{"source":r#"
            local actor=script.Parent
            local cancelled=task.spawn(function() actor.Bridge:Invoke(999); error('cancelled Actor caller resumed') end)
            actor:BindToMessage('bind',function()
                task.cancel(cancelled)
                script:SetAttribute('CancelledState',coroutine.status(cancelled))
                actor.Bridge.OnInvoke=function(value) return value*7 end
            end)
            local answer=actor.Bridge:Invoke(6)
            script:SetAttribute('Result',answer)
        "#}},"nodes":[
            {"id":"game","class_name":"DataModel","name":"game","properties":{}},
            {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
            {"id":"actor","class_name":"Actor","name":"Worker","parent":"ws","properties":{}},
            {"id":"bridge","class_name":"BindableFunction","name":"Bridge","parent":"actor","properties":{}},
            {"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes":{},"attributes_known":true}
        ]}),32).unwrap();
    poll(&mut vm);
    execute(&mut vm,"workspace.Worker.Bridge.OnInvoke=function(value) return value+100 end");
    poll(&mut vm);
    assert_eq!(execute(&mut vm,"return workspace.Worker.Reader:GetAttribute('Result') == nil"),json!([true]));
    execute(&mut vm,"workspace.Worker:SendMessage('bind')");
    poll(&mut vm);
    assert_eq!(execute(&mut vm,"return workspace.Worker.Reader:GetAttribute('Result'),workspace.Worker.Bridge:Invoke(6),workspace.Worker.Reader:GetAttribute('CancelledState')"),json!([42.0,106.0,"dead"]));
}

#[test]
fn cancellation_closes_pending_bindable_threads_across_commands() {
    let mut vm=Runtime::new(json!({"version":1,"sequence":0,"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","properties":{}}
    ]}),32).unwrap();
    execute(&mut vm,"fn=Instance.new('BindableFunction'); caller=task.spawn(function() fn:Invoke(); escaped=true end)");
    poll(&mut vm);
    execute(&mut vm,"task.cancel(caller)");
    assert_eq!(execute(&mut vm,"return coroutine.status(caller),coroutine.resume(caller),escaped==nil"),json!(["dead",false,true]));
    execute(&mut vm,"calls=0; fn.OnInvoke=function() calls+=1; return 42 end; task.spawn(function() result=fn:Invoke() end)");
    poll(&mut vm);
    assert_eq!(execute(&mut vm,"return calls,result,escaped==nil"),json!([1.0,42.0,true]));
}
