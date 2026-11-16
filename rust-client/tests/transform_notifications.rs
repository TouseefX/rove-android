//! Received canonical transforms must notify derived properties across receipts.
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

fn frame(x: f64, turned: bool) -> Value {
    let rotation = if turned {
        [0., 0., 1., 0., 1., 0., -1., 0., 0.]
    } else {
        [1., 0., 0., 0., 1., 0., 0., 0., 1.]
    };
    json!({"$cframe":{"position":[x,0,0],"rotation":rotation}})
}
fn fixture() -> Runtime {
    let snapshot = json!({"version":1,"sequence":0,"services":{"Workspace":"ws"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game"},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game"},
        {"id":"p","class_name":"Part","name":"Part","parent":"ws","properties":{"CFrame":frame(0.,false)}},
        {"id":"actor","class_name":"Actor","name":"Worker","parent":"ws"},
        {"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}}
    ],"script_sources":{"reader":{"source":r#"
        for _, key in ipairs({'CFrame','Position','Orientation','Rotation'}) do
            local count=0
            workspace.Part:GetPropertyChangedSignal(key):Connect(function(...)
                assert(select('#',...)==0)
                count+=1; script:SetAttribute(key,count)
            end)
        end
    "#}}});
    let mut vm = Runtime::new(snapshot, 32).unwrap();
    execute(
        &mut vm,
        r#"
        counts={CFrame=0,Position=0,Orientation=0,Rotation=0}
        for key in pairs(counts) do
            workspace.Part:GetPropertyChangedSignal(key):Connect(function(...)
                assert(select('#',...)==0);counts[key]+=1
            end)
        end
        task.spawn(function() positionWait=table.pack(workspace.Part:GetPropertyChangedSignal('Position'):Wait()).n end)
    "#,
    );
    vm
}
fn settle(vm: &mut Runtime) {
    for _ in 0..6 {
        call(vm, json!({"op":"poll","timeout_ms":100}));
    }
}
fn counts(vm: &mut Runtime) -> Value {
    execute(vm,"local a=workspace.Worker.Reader; return counts.CFrame,counts.Position,counts.Orientation,counts.Rotation,a:GetAttribute('CFrame'),a:GetAttribute('Position'),a:GetAttribute('Orientation'),a:GetAttribute('Rotation')")
}
#[test]
fn intermediate_transforms_notify_root_and_actor_even_when_final_pose_is_unchanged() {
    let mut vm = fixture();
    let batch = json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":4,"actions":[
        {"kind":"set","id":"p","sequence":1,"fields":{"CFrame":frame(4.,true)}},
        {"kind":"set","id":"p","sequence":2,"fields":{"CFrame":frame(4.,true)}},
        {"kind":"set_property","id":"p","sequence":3,"key":"CFrame","value":frame(8.,true)},
        {"kind":"set","id":"p","sequence":4,"fields":{"CFrame":frame(0.,false)}}
    ]}});
    call(&mut vm, batch.clone());
    settle(&mut vm);
    assert_eq!(counts(&mut vm), json!([3., 3., 2., 2., 3., 3., 2., 2.]));
    assert_eq!(
        execute(
            &mut vm,
            "return positionWait,workspace.Part.Position.X,workspace.Part.Orientation.Y"
        ),
        json!([0., 0., 0.])
    );
    call(&mut vm, batch);
    settle(&mut vm);
    assert_eq!(counts(&mut vm), json!([3., 3., 2., 2., 3., 3., 2., 2.]));
}
#[test]
fn received_translation_notifies_position_in_actor_without_rotation_events() {
    let mut vm = fixture();
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":1,"actions":[
            {"kind":"set","id":"p","sequence":1,"property":"CFrame","value":frame(4.,false)}
        ]}}),
    );
    settle(&mut vm);
    assert_eq!(counts(&mut vm), json!([1., 1., 0., 0., 1., 1., null, null]));
}

#[test]
fn received_frame_replaces_local_pose_and_coalesces_explicit_aliases() {
    let mut vm = fixture();
    execute(
        &mut vm,
        "workspace.Part.CFrame=CFrame.new(50,0,0)*CFrame.Angles(0,math.pi/2,0)",
    );
    settle(&mut vm);
    let before = counts(&mut vm);
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":1,"actions":[
            {"kind":"set","id":"p","sequence":1,"fields":{"CFrame":frame(0.,false),"Position":{"$vector3":[999,0,0]},"Orientation":{"$vector3":[0,80,0]},"Rotation":{"$vector3":[0,80,0]}}}
        ]}}),
    );
    settle(&mut vm);
    let after = counts(&mut vm);
    for i in 0..8 {
        assert_eq!(
            after[i].as_f64().unwrap(),
            before[i].as_f64().unwrap_or(0.) + 1.,
            "index {i}: {before} -> {after}"
        );
    }
    assert_eq!(execute(&mut vm,"return workspace.Part.Position.X,workspace.Part.Orientation.Y,workspace.Part.Rotation.Y"),json!([0.,0.,0.]));
}
#[test]
fn rejected_transform_batch_does_not_notify_or_move_either_vm() {
    let mut vm = fixture();
    let result=vm.call(json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":2,"actions":[
        {"kind":"set","id":"p","sequence":1,"fields":{"CFrame":frame(4.,true)}},
        {"kind":"set","id":"missing","sequence":2,"fields":{"CFrame":frame(4.,true)}}
    ]}}));
    assert!(result.is_err() || !result.as_ref().unwrap()["ok"].as_bool().unwrap_or(true));
    settle(&mut vm);
    assert_eq!(
        counts(&mut vm),
        json!([0., 0., 0., 0., null, null, null, null])
    );
    assert_eq!(
        execute(
            &mut vm,
            "return workspace.Part.Position.X,positionWait==nil"
        ),
        json!([0., true])
    );
}
#[test]
fn explicit_cframe_value_overrides_fields_and_duplicate_pose_is_silent() {
    let mut vm = fixture();
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":1,"actions":[
            {"kind":"set","id":"p","sequence":1,"fields":{"CFrame":frame(4.,true)},"property":"CFrame","value":frame(0.,false)}
        ]}}),
    );
    settle(&mut vm);
    assert_eq!(
        counts(&mut vm),
        json!([0., 0., 0., 0., null, null, null, null])
    );
}
