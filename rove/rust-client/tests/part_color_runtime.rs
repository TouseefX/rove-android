//! Linked BasePart Color and BrickColor behavior in the official Luau VM.
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
        {"id":"p","class_name":"Part","name":"Part","parent":"ws","properties":{"Color3uint8":{"$color3":[0,0,0]}}},
        {"id":"unknown","class_name":"Part","name":"Unknown","parent":"ws","properties":{}}
    ]});
    if actor {
        snapshot["nodes"].as_array_mut().unwrap().extend([
            json!({"id":"actor","class_name":"Actor","name":"Worker","parent":"ws","properties":{}}),
            json!({"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}})
        ]);
        snapshot["script_sources"] = json!({"reader":{"source":r#"
            local count=0
            workspace.Part:GetPropertyChangedSignal('BrickColor'):Connect(function(...) assert(select('#',...)==0);count+=1;script:SetAttribute('Count',count) end)
            script.Parent:BindToMessage('color',function() workspace.Part.BrickColor=BrickColor.new('Really red') end)
        "#}});
    }
    let mut vm = Runtime::new(snapshot, 32).unwrap();
    execute(
        &mut vm,
        r#"
        colors=0;bricks=0
        workspace.Part:GetPropertyChangedSignal('Color'):Connect(function(...) assert(select('#',...)==0);colors+=1 end)
        workspace.Part:GetPropertyChangedSignal('BrickColor'):Connect(function(...) assert(select('#',...)==0);bricks+=1 end)
    "#,
    );
    vm
}
fn settle(vm: &mut Runtime) {
    for _ in 0..6 {
        call(vm, json!({"op":"poll","timeout_ms":100}));
    }
}
#[test]
fn local_color_and_brickcolor_stay_linked_and_do_not_change_unrelated_value_instances() {
    let mut vm = fixture(false);
    assert_eq!(
        execute(
            &mut vm,
            r#"
        local p=Instance.new('Part')
        p.BrickColor=BrickColor.new('Really red')
        assert(p.Color==BrickColor.new('Really red').Color)
        p.Color=Color3.new(0.11,0.27,0.83)
        assert(p.BrickColor==BrickColor.new(p.Color))
        local clone=p:Clone();assert(clone.Color==p.Color and clone.BrickColor==p.BrickColor)
        p:ResetPropertyToDefault('BrickColor')
        assert(p.BrickColor==BrickColor.new(194) and p.Color==BrickColor.new(194).Color)
        local v=Instance.new('BrickColorValue');v.Value=BrickColor.new('Really red');assert(v.Value.Number==1004)
        return true
    "#
        ),
        json!([true])
    );
}
#[test]
fn received_color_changes_notify_brickcolor_only_when_the_palette_match_changes() {
    let mut vm = fixture(true);
    execute(&mut vm,"assert(workspace.Part.BrickColor==BrickColor.new('Really black'));task.spawn(function() brickWait=table.pack(workspace.Part:GetPropertyChangedSignal('BrickColor'):Wait()).n end)");
    let request = json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":3,"actions":[
        {"kind":"set","sequence":1,"id":"p","fields":{"Color3uint8":{"$color3":[1,0,0]}}},
        {"kind":"set","sequence":2,"id":"p","fields":{"Color3uint8":{"$color3":[0.9999,0,0]}}},
        {"kind":"set","sequence":3,"id":"p","fields":{"Color3uint8":{"$color3":[0,0,0]}}}
    ]}});
    call(&mut vm, request.clone());
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return colors,bricks,brickWait,workspace.Part.BrickColor.Number,workspace.Worker.Reader:GetAttribute('Count')"),json!([3.0,2.0,0.0,1003.0,2.0]));
    call(&mut vm, request);
    settle(&mut vm);
    assert_eq!(execute(&mut vm, "return colors,bricks"), json!([3.0, 2.0]));
}
#[test]
fn actor_brickcolor_writes_update_root_color_and_preserve_one_notification_per_change() {
    let mut vm = fixture(true);
    execute(&mut vm, "workspace.Worker:SendMessage('color')");
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return workspace.Part.Color==BrickColor.new('Really red').Color,workspace.Part.BrickColor.Number,colors,bricks,workspace.Worker.Reader:GetAttribute('Count')"),json!([true,1004.0,1.0,1.0,1.0]));
    execute(&mut vm, "workspace.Part.Color=Color3.new(0,0,0)");
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return workspace.Part.BrickColor.Number,colors,bricks,workspace.Worker.Reader:GetAttribute('Count')"),json!([1003.0,2.0,2.0,2.0]));
}
#[test]
fn invalid_color_writes_are_atomic_and_missing_received_color_stays_unknown() {
    let mut vm = fixture(false);
    assert_eq!(
        execute(
            &mut vm,
            r#"
        local ok=pcall(function() return workspace.Unknown.BrickColor end);assert(not ok)
        local old=workspace.Part.Color
        assert(not pcall(function() workspace.Part.BrickColor='Really red' end))
        assert(not pcall(function() workspace.Part.Color='red' end))
        return workspace.Part.Color==old
    "#
        ),
        json!([true])
    );
    assert_eq!(execute(&mut vm, "return colors,bricks"), json!([0.0, 0.0]));
}

#[test]
fn received_subclasses_inherit_color_linking_and_notifications() {
    let mut vm=Runtime::new(json!({"version":1,"sequence":0,"services":{"Workspace":"ws"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","properties":{}},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
        {"id":"p","class_name":"NegateOperation","name":"Part","parent":"ws","properties":{"Color3uint8":{"$color3":[0,0,0]}}}
    ]}),32).unwrap();
    execute(&mut vm,"assert(workspace.Part:IsA('BasePart'));assert(workspace.Part.BrickColor.Number==1003);changed=0;workspace.Part:GetPropertyChangedSignal('BrickColor'):Connect(function() changed+=1 end)");
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":1,"actions":[
            {"kind":"set","sequence":1,"id":"p","fields":{"Color3uint8":{"$color3":[1,0,0]}}}
        ]}}),
    );
    assert_eq!(
        execute(
            &mut vm,
            "return changed,workspace.Part.Color.R,workspace.Part.BrickColor.Number"
        ),
        json!([1.0, 1.0, 1004.0])
    );
}
