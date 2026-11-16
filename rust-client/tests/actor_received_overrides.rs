//! A received update must replace old script overlays in every Actor VM.
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
fn settle(vm: &mut Runtime) {
    for _ in 0..4 {
        call(vm, json!({"op":"poll","timeout_ms":100}));
    }
}
fn frame(x: i32) -> Value {
    json!({"$cframe":{"position":[x,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}})
}
fn fixture() -> Runtime {
    let mut vm=Runtime::new(json!({"version":1,"sequence":0,"services":{"Workspace":"ws"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game"},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game"},
        {"id":"p","class_name":"Part","name":"Part","parent":"ws","properties":{"CFrame":frame(0),"Color3uint8":{"$color3":[0,0,0]},"Transparency":0,"Anchored":false}},
        {"id":"h","class_name":"Humanoid","name":"Humanoid","parent":"ws","properties":{"Health":100,"MaxHealth":100}},
        {"id":"attachment","class_name":"Attachment","name":"Attachment","parent":"p","properties":{"CFrame":frame(0)}},
        {"id":"other","class_name":"Folder","name":"Other","parent":"ws"},
        {"id":"actor","class_name":"Actor","name":"Worker","parent":"ws"},
        {"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}}
    ],"script_sources":{"reader":{"source":r#"
        local p=workspace.Part;local h=workspace.Humanoid;local att=p.Attachment
        local function sample()
            script:SetAttribute('X',p.Position.X)
            script:SetAttribute('Red',p.Color.R)
            script:SetAttribute('Health',h.Health)
            script:SetAttribute('Transparency',p.Transparency)
        end
        p:GetPropertyChangedSignal('Position'):Connect(sample)
        p:GetPropertyChangedSignal('Color'):Connect(sample)
        h.HealthChanged:Connect(sample)
        script.Parent:BindToMessage('sample',sample)
        script.Parent:BindToMessage('write',function()
            p.CFrame=CFrame.new(50,0,0);p.Color=Color3.new(1,0,0)
            h.Health=75;p.Transparency=0.5
        end)
        script.Parent:BindToMessage('sample-extra',function()
            script:SetAttribute('PartName',p.Name);script:SetAttribute('ParentName',p.Parent.Name)
            script:SetAttribute('AttachmentX',att.Position.X);script:SetAttribute('AxisX',att.Axis.X)
        end)
    "#}}}),32).unwrap();
    execute(
        &mut vm,
        "p=workspace.Part;h=workspace.Humanoid;a=workspace.Worker.Reader",
    );
    settle(&mut vm);
    vm
}
fn received(sequence: u64) -> Value {
    json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":sequence+2,"actions":[
        {"kind":"set","id":"p","sequence":sequence,"fields":{"CFrame":frame(10)}},
        {"kind":"set_property","id":"p","sequence":sequence+1,"key":"Color3uint8","value":{"$color3":[0,1,0]}},
        {"kind":"set","id":"h","sequence":sequence+2,"property":"Health","value":40}
    ]}})
}
fn sample(vm: &mut Runtime) -> Value {
    execute(vm, "workspace.Worker:SendMessage('sample')");
    settle(vm);
    execute(vm,"return p.Position.X,p.Color.R,h.Health,p.Transparency,a:GetAttribute('X'),a:GetAttribute('Red'),a:GetAttribute('Health'),a:GetAttribute('Transparency')")
}
#[test]
fn received_updates_replace_root_authored_overlays_in_actor_reads_and_callbacks() {
    let mut vm = fixture();
    execute(
        &mut vm,
        "p.CFrame=CFrame.new(50,0,0);p.Color=Color3.new(1,0,0);h.Health=75;p.Transparency=0.5",
    );
    settle(&mut vm);
    assert_eq!(
        sample(&mut vm),
        json!([50., 1., 75., 0.5, 50., 1., 75., 0.5])
    );
    call(&mut vm, received(1));
    settle(&mut vm);
    // Check callback reads before asking the Actor to take a new sample.
    assert_eq!(
        execute(
            &mut vm,
            "return a:GetAttribute('X'),a:GetAttribute('Red'),a:GetAttribute('Health')"
        ),
        json!([10., 0., 40.])
    );
    assert_eq!(
        sample(&mut vm),
        json!([10., 0., 40., 0.5, 10., 0., 40., 0.5])
    );
}
#[test]
fn received_updates_replace_actor_authored_overlays_and_allow_later_local_writes() {
    let mut vm = fixture();
    execute(&mut vm, "workspace.Worker:SendMessage('write')");
    settle(&mut vm);
    assert_eq!(
        sample(&mut vm),
        json!([50., 1., 75., 0.5, 50., 1., 75., 0.5])
    );
    call(&mut vm, received(1));
    settle(&mut vm);
    assert_eq!(
        sample(&mut vm),
        json!([10., 0., 40., 0.5, 10., 0., 40., 0.5])
    );
    execute(&mut vm, "workspace.Worker:SendMessage('write')");
    settle(&mut vm);
    assert_eq!(
        sample(&mut vm),
        json!([50., 1., 75., 0.5, 50., 1., 75., 0.5])
    );
    // Retrying an old receipt cannot undo a newer script write.
    call(&mut vm, received(1));
    settle(&mut vm);
    assert_eq!(
        sample(&mut vm),
        json!([50., 1., 75., 0.5, 50., 1., 75., 0.5])
    );
    call(&mut vm, received(4));
    settle(&mut vm);
    assert_eq!(
        sample(&mut vm),
        json!([10., 0., 40., 0.5, 10., 0., 40., 0.5])
    );
}

#[test]
fn received_headers_and_attachment_aliases_replace_old_local_values() {
    let mut vm = fixture();
    execute(&mut vm,"p.Name='LocalPart';p.Parent=workspace.Other;p.Attachment.CFrame=CFrame.new(50,0,0)*CFrame.Angles(0,math.pi/2,0)");
    settle(&mut vm);
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":2,"actions":[
            {"kind":"set","sequence":1,"id":"p","fields":{"Name":"ReceivedPart","Parent":{"$ref":"ws"}}},
            {"kind":"set","sequence":2,"id":"attachment","fields":{"CFrame":frame(5)}}
        ]}}),
    );
    execute(&mut vm, "workspace.Worker:SendMessage('sample-extra')");
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return p.Name,p.Parent.Name,p.Attachment.Position.X,p.Attachment.Axis.X,a:GetAttribute('PartName'),a:GetAttribute('ParentName'),a:GetAttribute('AttachmentX'),a:GetAttribute('AxisX')"),json!(["ReceivedPart","Workspace",5.,1.,"ReceivedPart","Workspace",5.,1.]));
}

#[test]
fn rejected_receipts_preserve_both_vms_local_values() {
    let mut vm = fixture();
    execute(&mut vm, "workspace.Worker:SendMessage('write')");
    settle(&mut vm);
    let mut request = received(1);
    request["delta"]["actions"][2]["id"] = json!("missing");
    let result = vm.call(request);
    assert!(result.is_err() || !result.unwrap()["ok"].as_bool().unwrap_or(true));
    assert_eq!(
        sample(&mut vm),
        json!([50., 1., 75., 0.5, 50., 1., 75., 0.5])
    );
}

#[test]
fn identical_wire_values_still_replace_actor_overrides() {
    let mut vm = fixture();
    execute(&mut vm, "workspace.Worker:SendMessage('write')");
    settle(&mut vm);
    let mut request = received(1);
    request["delta"]["actions"][0]["fields"]["CFrame"] = frame(0);
    request["delta"]["actions"][1]["value"] = json!({"$color3":[0,0,0]});
    request["delta"]["actions"][2]["value"] = json!(100);
    call(&mut vm, request);
    assert_eq!(
        sample(&mut vm),
        json!([0., 0., 100., 0.5, 0., 0., 100., 0.5])
    );
}
