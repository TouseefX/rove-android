//! Attribute notifications across received receipts and isolated Actor VMs.
#![cfg(feature = "rust-host")]
use headless_luau::runtime_owner::{successful, Runtime};
use serde_json::{json, Value};
fn call(vm: &mut Runtime, request: Value) -> Value {
    let reply = vm.call(request).unwrap();
    successful(&reply).unwrap_or_else(|e| panic!("{e}: {reply}"));
    assert!(
        reply["task_errors"].as_array().is_none_or(|e| e.is_empty()),
        "{reply}"
    );
    for actor in reply["actor_runtime"].as_array().into_iter().flatten() {
        assert!(
            actor["task_errors"].as_array().is_none_or(|e| e.is_empty()),
            "{actor}"
        );
        assert!(actor.get("shadow_merge_error").is_none(), "{actor}");
        assert!(
            actor["shadow_sync"]["conflicts"]
                .as_array()
                .is_none_or(|e| e.is_empty()),
            "{actor}"
        );
    }
    reply
}
fn execute(vm: &mut Runtime, source: &str) -> Value {
    call(vm, json!({"op":"execute","source":source,"timeout_ms":100}))["values"].clone()
}

fn fixture(actor: bool) -> Runtime {
    let source = r#"
        local part=workspace.Part
        local trace={}; local count=0
        part.AttributeChanged:Connect(function(name)
            table.insert(trace,name); script:SetAttribute('Trace',table.concat(trace,','))
        end)
        part:GetAttributeChangedSignal('Score'):Connect(function(...)
            assert(select('#',...)==0); count+=1; script:SetAttribute('Count',count)
        end)
        script.Parent:BindToMessage('change',function()
            part:SetAttribute('Score',8); part:SetAttribute('Score',0)
        end)
    "#;
    let mut snapshot = json!({"version":1,"sequence":0,"services":{"Workspace":"ws"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","properties":{}},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
        {"id":"p","class_name":"Part","name":"Part","parent":"ws","properties":{},"attributes_known":true,"attributes":{"Score":0,"Other":4}}
    ]});
    if actor {
        snapshot["nodes"].as_array_mut().unwrap().extend([
            json!({"id":"actor","class_name":"Actor","name":"Worker","parent":"ws","properties":{}}),
            json!({"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}})
        ]);
        snapshot["script_sources"] = json!({"reader":{"source":source}});
    }
    let mut vm = Runtime::new(snapshot, 32).unwrap();
    execute(
        &mut vm,
        r#"
        trace={};count=0
        workspace.Part.AttributeChanged:Connect(function(name) table.insert(trace,name) end)
        workspace.Part:GetAttributeChangedSignal('Score'):Connect(function(...) assert(select('#',...)==0); count+=1 end)
    "#,
    );
    vm
}
fn settle(vm: &mut Runtime) {
    for _ in 0..6 {
        call(vm, json!({"op":"poll","timeout_ms":100}));
    }
}
fn received_batch() -> Value {
    json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":5,"actions":[
        {"kind":"attribute","sequence":1,"id":"p","attribute_update":{"name":"Score","known":true,"value":8}},
        {"kind":"attribute","sequence":2,"id":"p","attribute_update":{"name":"Score","known":true,"value":8.0}},
        {"kind":"set","sequence":3,"id":"p","fields":{},"attribute_snapshot":{"attributes_known":true,"attributes":{"Score":0,"Other":4}}},
        {"kind":"set_property","sequence":4,"id":"p","fields":{},"attribute_snapshot":{"attributes_known":true,"attributes":{"Score":0}}},
        {"kind":"attribute","sequence":5,"id":"p","attribute_update":{"name":"Score","known":true,"value":null}}
    ]}})
}
#[test]
fn received_attribute_batches_preserve_transitions_and_ignore_equal_values_and_retries() {
    let mut vm = fixture(false);
    let request = received_batch();
    call(&mut vm, request.clone());
    assert_eq!(
        execute(
            &mut vm,
            "return table.concat(trace,','),count,workspace.Part:GetAttribute('Score')==nil"
        ),
        json!(["Score,Score,Other,Score", 3.0, true])
    );
    call(&mut vm, request);
    assert_eq!(
        execute(&mut vm, "return table.concat(trace,','),count"),
        json!(["Score,Score,Other,Score", 3.0])
    );
}
#[test]
fn actor_observes_received_and_local_attribute_round_trips() {
    let mut vm = fixture(true);
    call(&mut vm, received_batch());
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return workspace.Worker.Reader:GetAttribute('Trace'),workspace.Worker.Reader:GetAttribute('Count')"),json!(["Score,Score,Other,Score",3.0]));
    execute(
        &mut vm,
        "workspace.Part:SetAttribute('Score',8);workspace.Part:SetAttribute('Score',nil)",
    );
    settle(&mut vm);
    assert_eq!(
        execute(
            &mut vm,
            "return workspace.Worker.Reader:GetAttribute('Count'),count"
        ),
        json!([5.0, 5.0])
    );
    execute(&mut vm, "workspace.Worker:SendMessage('change')");
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return workspace.Worker.Reader:GetAttribute('Count'),count,workspace.Part:GetAttribute('Score')"),json!([7.0,7.0,0.0]));
}
#[test]
fn rejected_attribute_transaction_emits_nothing_and_keeps_state() {
    let mut vm = fixture(false);
    let result=vm.call(json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":2,"actions":[
        {"kind":"attribute","sequence":1,"id":"p","attribute_update":{"name":"Score","known":true,"value":8}},
        {"kind":"attribute","sequence":2,"id":"missing","attribute_update":{"name":"Score","known":true,"value":1}}
    ]}}));
    assert!(result.is_err() || !result.as_ref().unwrap()["ok"].as_bool().unwrap_or(true));
    assert_eq!(
        execute(
            &mut vm,
            "return count,table.concat(trace,','),workspace.Part:GetAttribute('Score')"
        ),
        json!([0.0, "", 0.0])
    );
}

#[test]
fn received_attribute_round_trip_does_not_disappear_into_the_final_snapshot() {
    let mut vm = fixture(false);
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":2,"actions":[
            {"kind":"attribute","sequence":1,"id":"p","attribute_update":{"name":"Score","known":true,"value":8}},
            {"kind":"attribute","sequence":2,"id":"p","attribute_update":{"name":"Score","known":true,"value":0}}
        ]}}),
    );
    assert_eq!(
        execute(
            &mut vm,
            "return count,table.concat(trace,','),workspace.Part:GetAttribute('Score')"
        ),
        json!([2.0, "Score,Score", 0.0])
    );
}

#[test]
fn attribute_waiters_resume_with_documented_argument_tuples() {
    let mut vm = fixture(false);
    execute(
        &mut vm,
        r#"
        task.spawn(function() local args=table.pack(workspace.Part.AttributeChanged:Wait()); waitedName=args[1]; generalCount=args.n end)
        task.spawn(function() local args=table.pack(workspace.Part:GetAttributeChangedSignal('Score'):Wait()); specificCount=args.n end)
    "#,
    );
    call(&mut vm, received_batch());
    assert_eq!(
        execute(&mut vm, "return waitedName,generalCount,specificCount"),
        json!(["Score", 1.0, 0.0])
    );
}

#[test]
fn unknown_and_known_nil_attributes_stay_distinct_without_duplicate_nil_events() {
    let mut vm = fixture(true);
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":1,"actions":[
            {"kind":"attribute","sequence":1,"id":"p","attribute_update":{"name":"Score","known":false}}
        ]}}),
    );
    assert_eq!(
        execute(
            &mut vm,
            "return pcall(function() return workspace.Part:GetAttribute('Score') end)"
        )[0],
        json!(false)
    );
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":3,"actions":[
            {"kind":"attribute","sequence":2,"id":"p","attribute_update":{"name":"Score","known":true,"value":null}},
            {"kind":"attribute","sequence":3,"id":"p","attribute_update":{"name":"Score","known":true,"value":null}}
        ]}}),
    );
    assert_eq!(execute(&mut vm,"return workspace.Part:GetAttribute('Score')==nil,pcall(function() return workspace.Part:GetAttributes() end)")[0],json!(true));
    assert_eq!(
        execute(
            &mut vm,
            "return pcall(function() return workspace.Part:GetAttributes() end)"
        )[0],
        json!(false)
    );
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":4,"actions":[
            {"kind":"set","sequence":4,"id":"p","fields":{},"attribute_snapshot":{"attributes_known":true,"attributes":{}}}
        ]}}),
    );
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return count,table.concat(trace,','),workspace.Worker.Reader:GetAttribute('Count'),next(workspace.Part:GetAttributes())==nil"),json!([2.0,"Score,Score,Other",2.0,true]));
    execute(
        &mut vm,
        "workspace.Part:SetAttribute('Score',nil);workspace.Part:SetAttribute('Score',nil)",
    );
    settle(&mut vm);
    assert_eq!(
        execute(
            &mut vm,
            "return count,workspace.Worker.Reader:GetAttribute('Count')"
        ),
        json!([2.0, 2.0])
    );
}
