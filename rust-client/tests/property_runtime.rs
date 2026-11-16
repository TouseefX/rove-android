//! Property notifications through the official VM and received transactions.
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
    let mut snapshot = json!({"version":1,"sequence":0,"services":{"Workspace":"ws"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","properties":{}},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
        {"id":"p","class_name":"Part","name":"Part","parent":"ws","properties":{"Transparency":0,"Anchored":false}},
        {"id":"n","class_name":"NumberValue","name":"Score","parent":"ws","properties":{"Value":0}}
    ]});
    if actor {
        snapshot["nodes"].as_array_mut().unwrap().extend([
            json!({"id":"actor","class_name":"Actor","name":"Worker","parent":"ws","properties":{}}),
            json!({"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}})
        ]);
        snapshot["script_sources"] = json!({"reader":{"source":r#"
            local changes=0;local values={}
            workspace.Part:GetPropertyChangedSignal('Transparency'):Connect(function(...)
                assert(select('#',...)==0); changes+=1;script:SetAttribute('Count',changes)
            end)
            workspace.Score.Changed:Connect(function(value)
                assert(typeof(value)=='number'); table.insert(values,tostring(value));script:SetAttribute('Values',table.concat(values,','))
            end)
            script.Parent:BindToMessage('change',function() workspace.Score.Value=9;workspace.Score.Value=0 end)
        "#}});
    }
    let mut vm = Runtime::new(snapshot, 32).unwrap();
    execute(
        &mut vm,
        r#"
        changes=0;properties={};values={}
        workspace.Part:GetPropertyChangedSignal('Transparency'):Connect(function(...) assert(select('#',...)==0);changes+=1 end)
        workspace.Part.Changed:Connect(function(name) table.insert(properties,name) end)
        workspace.Score.Changed:Connect(function(value) table.insert(values,tostring(value)) end)
    "#,
    );
    vm
}
fn settle(vm: &mut Runtime) {
    for _ in 0..6 {
        call(vm, json!({"op":"poll","timeout_ms":100}));
    }
}
fn batch() -> Value {
    json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":6,"actions":[
        {"kind":"set","sequence":1,"id":"p","fields":{"Transparency":0.5}},
        {"kind":"set_property","sequence":2,"id":"p","fields":{"Transparency":0.25},"property":"Transparency","value":0.5},
        {"kind":"set_property","sequence":3,"id":"p","key":"Transparency","value":0},
        {"kind":"set","sequence":4,"id":"n","fields":{"Value":4}},
        {"kind":"set_property","sequence":5,"id":"n","key":"Value","value":0},
        {"kind":"set","sequence":6,"id":"n","property":"Name","value":"Score"}
    ]}})
}
#[test]
fn received_properties_keep_intermediate_changes_and_value_payloads() {
    let mut vm = fixture(false);
    let request = batch();
    call(&mut vm, request.clone());
    let expected = json!([2.0, "Transparency,Transparency", "4,0"]);
    assert_eq!(
        execute(
            &mut vm,
            "return changes,table.concat(properties,','),table.concat(values,',')"
        ),
        expected
    );
    call(&mut vm, request);
    assert_eq!(
        execute(
            &mut vm,
            "return changes,table.concat(properties,','),table.concat(values,',')"
        ),
        expected
    );
}
#[test]
fn value_instances_only_send_value_changes_with_typed_payloads() {
    let mut vm = fixture(false);
    execute(
        &mut vm,
        r#"
        n=Instance.new('NumberValue'); typed={};named=0;parented=0
        n.Changed:Connect(function(value) table.insert(typed,typeof(value)..':'..tostring(value)) end)
        n:GetPropertyChangedSignal('Name'):Connect(function(...) assert(select('#',...)==0);named+=1 end)
        n:GetPropertyChangedSignal('Parent'):Connect(function() parented+=1 end)
        n.Value=3;n.Name='Renamed';n.Parent=workspace;n.Value=0;n.Value=0
    "#,
    );
    assert_eq!(
        execute(&mut vm, "return table.concat(typed,','),named,parented"),
        json!(["number:3,number:0", 1.0, 1.0])
    );
}
#[test]
fn actor_observes_received_and_local_property_round_trips() {
    let mut vm = fixture(true);
    call(&mut vm, batch());
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return workspace.Worker.Reader:GetAttribute('Count'),workspace.Worker.Reader:GetAttribute('Values')"),json!([2.0,"4,0"]));
    execute(&mut vm, "workspace.Score.Value=7;workspace.Score.Value=0");
    settle(&mut vm);
    execute(&mut vm, "workspace.Worker:SendMessage('change')");
    settle(&mut vm);
    assert_eq!(
        execute(
            &mut vm,
            "return workspace.Worker.Reader:GetAttribute('Values'),table.concat(values,',')"
        ),
        json!(["4,0,7,0,9,0", "4,0,7,0,9,0"])
    );
}
#[test]
fn rejected_property_batch_has_no_signal_effects() {
    let mut vm = fixture(false);
    let result=vm.call(json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":2,"actions":[
        {"kind":"set","sequence":1,"id":"p","property":"Transparency","value":0.5},
        {"kind":"set","sequence":2,"id":"missing","property":"Value","value":2}
    ]}}));
    assert!(result.is_err() || !result.as_ref().unwrap()["ok"].as_bool().unwrap_or(true));
    assert_eq!(
        execute(&mut vm, "return changes,workspace.Part.Transparency"),
        json!([0.0, 0.0])
    );
}

#[test]
fn property_waiters_wake_for_both_fields_and_explicit_property() {
    let mut vm = fixture(false);
    execute(
        &mut vm,
        r#"
        part=workspace.Part
        task.spawn(function() local args=table.pack(part:GetPropertyChangedSignal('Transparency'):Wait()); transparencyArgs=args.n end)
        task.spawn(function() local args=table.pack(part:GetPropertyChangedSignal('Name'):Wait()); nameArgs=args.n end)
    "#,
    );
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":1,"actions":[
            {"kind":"set","sequence":1,"id":"p","fields":{"Transparency":0.4},"property":"Name","value":"Renamed"}
        ]}}),
    );
    assert_eq!(
        execute(
            &mut vm,
            "return transparencyArgs,nameArgs,changes,workspace.Renamed==part,part.Transparency"
        ),
        json!([0.0, 0.0, 1.0, true, 0.4])
    );
}

#[test]
fn value_changed_preserves_instance_nil_boolean_and_vector_types() {
    let mut vm = fixture(false);
    execute(
        &mut vm,
        r#"
        local object=Instance.new('ObjectValue');local flag=Instance.new('BoolValue');local vector=Instance.new('Vector3Value')
        objectValues={};boolValues={};vectors={}
        object.Changed:Connect(function(...) local args=table.pack(...);assert(args.n==1);table.insert(objectValues,args[1]==workspace.Part and 'part' or (args[1]==nil and 'nil' or 'wrong')) end)
        flag.Changed:Connect(function(value) assert(typeof(value)=='boolean');table.insert(boolValues,tostring(value)) end)
        vector.Changed:Connect(function(value) assert(typeof(value)=='Vector3');table.insert(vectors,value.X) end)
        task.spawn(function() objectWait=table.pack(object.Changed:Wait()) end)
        task.defer(function() object.Value=workspace.Part;object.Value=nil;flag.Value=true;flag.Value=false;vector.Value=Vector3.new(5,2,3);vector.Value=Vector3.zero end)
    "#,
    );
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return table.concat(objectValues,','),table.concat(boolValues,','),vectors[1],vectors[2],objectWait.n,objectWait[1]==workspace.Part"),json!(["part,nil","true,false",5.0,0.0,1.0,true]));
}
