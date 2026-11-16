//! Public property events for decoded wire aliases.
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

fn fixture(actor: bool, explicit: bool) -> Runtime {
    let mut properties =
        json!({"Color3uint8":{"$color3":[0,0,0]},"CollisionGroupReplicate":{"$bytes_hex":"00"}});
    if explicit {
        properties["Color"] = json!({"$color3":[0,1,0]});
        properties["CollisionGroup"] = json!("Explicit");
    }
    let mut snapshot = json!({"version":1,"sequence":0,"services":{"Workspace":"ws"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","properties":{}},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
        {"id":"p","class_name":"Part","name":"Part","parent":"ws","properties":properties}
    ]});
    if actor {
        snapshot["nodes"].as_array_mut().unwrap().extend([
            json!({"id":"actor","class_name":"Actor","name":"Worker","parent":"ws","properties":{}}),
            json!({"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}})
        ]);
        snapshot["script_sources"] = json!({"reader":{"source":r#"
            local colors,groups=0,0
            workspace.Part:GetPropertyChangedSignal('Color'):Connect(function(...) assert(select('#',...)==0);colors+=1;script:SetAttribute('Colors',colors) end)
            workspace.Part:GetPropertyChangedSignal('CollisionGroup'):Connect(function(...) assert(select('#',...)==0);groups+=1;script:SetAttribute('Groups',groups) end)
        "#}});
    }
    let mut vm = Runtime::new(snapshot, 32).unwrap();
    execute(
        &mut vm,
        r#"
        colors=0;groups=0;names={}
        workspace.Part:GetPropertyChangedSignal('Color'):Connect(function(...) assert(select('#',...)==0); colors+=1 end)
        workspace.Part:GetPropertyChangedSignal('CollisionGroup'):Connect(function(...) assert(select('#',...)==0); groups+=1 end)
        workspace.Part.Changed:Connect(function(name) table.insert(names,name) end)
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
        {"kind":"set","sequence":1,"id":"p","fields":{"Color3uint8":{"$color3":[1,0,0]}}},
        {"kind":"set","sequence":2,"id":"p","property":"Color3uint8","value":{"$color3":[1.0,0.0,0.0]}},
        {"kind":"set_property","sequence":3,"id":"p","key":"Color3uint8","value":{"$color3":[0,0,0]}},
        {"kind":"set","sequence":4,"id":"p","fields":{"CollisionGroupReplicate":{"$bytes_hex":"0147686f7374"}}},
        {"kind":"set","sequence":5,"id":"p","fields":{"CollisionGroupReplicate":{"$bytes_hex":"0247686f7374"}}},
        {"kind":"set","sequence":6,"id":"p","property":"CollisionGroupReplicate","value":{"$bytes_hex":"00"}}
    ]}})
}
#[test]
fn wire_updates_notify_public_names_and_compare_decoded_values_in_root_and_actor() {
    let mut vm = fixture(true, false);
    let request = batch();
    call(&mut vm, request.clone());
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return colors,groups,table.concat(names,','),workspace.Part.Color.R,workspace.Part.CollisionGroup,workspace.Worker.Reader:GetAttribute('Colors'),workspace.Worker.Reader:GetAttribute('Groups')"),json!([2.0,2.0,"BrickColor,Color,BrickColor,Color,CollisionGroup,CollisionGroup",0.0,"Default",2.0,2.0]));
    call(&mut vm, request);
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return colors,groups,workspace.Worker.Reader:GetAttribute('Colors'),workspace.Worker.Reader:GetAttribute('Groups')"),json!([2.0,2.0,2.0,2.0]));
}
#[test]
fn unchanged_wire_value_still_notifies_when_it_clears_a_local_public_override() {
    let mut vm = fixture(true, false);
    execute(
        &mut vm,
        "workspace.Part.Color=Color3.new(1,0,0);workspace.Part.CollisionGroup='Ghost'",
    );
    settle(&mut vm);
    execute(&mut vm, "colors=0;groups=0;names={}");
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":1,"actions":[
            {"kind":"set","sequence":1,"id":"p","fields":{"Color3uint8":{"$color3":[0,0,0]},"CollisionGroupReplicate":{"$bytes_hex":"00"}}}
        ]}}),
    );
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return colors,groups,workspace.Part.Color.R,workspace.Part.CollisionGroup,workspace.Worker.Reader:GetAttribute('Colors'),workspace.Worker.Reader:GetAttribute('Groups')"),json!([1.0,1.0,0.0,"Default",2.0,2.0]));
}
#[test]
fn explicit_public_properties_keep_precedence_and_suppress_masked_wire_notifications() {
    let mut vm = fixture(false, true);
    call(&mut vm, batch());
    assert_eq!(
        execute(
            &mut vm,
            "return colors,groups,workspace.Part.Color.G,workspace.Part.CollisionGroup"
        ),
        json!([0.0, 0.0, 1.0, "Explicit"])
    );
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":7,"actions":[
            {"kind":"set","sequence":7,"id":"p","fields":{"Color3uint8":{"$color3":[1,0,0]},"Color":{"$color3":[0,0,1]},"CollisionGroupReplicate":{"$bytes_hex":"0147686f7374"},"CollisionGroup":"Public"}}
        ]}}),
    );
    assert_eq!(execute(&mut vm,"return colors,groups,workspace.Part.Color.B,workspace.Part.CollisionGroup,table.concat(names,',')"),json!([1.0,1.0,1.0,"Public","BrickColor,CollisionGroup,Color"]));
}
#[test]
fn invalid_wire_alias_rolls_back_without_public_events() {
    let mut vm = fixture(false, false);
    let result=vm.call(json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":2,"actions":[
        {"kind":"set","sequence":1,"id":"p","fields":{"Color3uint8":{"$color3":[1,0,0]}}},
        {"kind":"set","sequence":2,"id":"p","fields":{"CollisionGroupReplicate":{"$bytes_hex":""}}}
    ]}}));
    assert!(result.is_err() || !result.as_ref().unwrap()["ok"].as_bool().unwrap_or(true));
    assert_eq!(
        execute(
            &mut vm,
            "return colors,groups,workspace.Part.Color.R,workspace.Part.CollisionGroup"
        ),
        json!([0.0, 0.0, 0.0, "Default"])
    );
}

#[test]
fn public_alias_waiters_and_local_round_trips_work_after_received_updates() {
    let mut vm = fixture(true, false);
    execute(
        &mut vm,
        r#"
        task.spawn(function() colorArgs=table.pack(workspace.Part:GetPropertyChangedSignal('Color'):Wait()).n end)
        task.spawn(function() groupArgs=table.pack(workspace.Part:GetPropertyChangedSignal('CollisionGroup'):Wait()).n end)
    "#,
    );
    call(&mut vm, batch());
    settle(&mut vm);
    assert_eq!(
        execute(&mut vm, "return colorArgs,groupArgs"),
        json!([0.0, 0.0])
    );
    execute(&mut vm,"workspace.Part.Color=Color3.new(1,0,0);workspace.Part.Color=Color3.new(0,0,0);workspace.Part.CollisionGroup='Ghost';workspace.Part.CollisionGroup='Default'");
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return colors,groups,workspace.Worker.Reader:GetAttribute('Colors'),workspace.Worker.Reader:GetAttribute('Groups')"),json!([4.0,4.0,4.0,4.0]));
}
