//! Engine movement observations must reach existing Actor VMs without script writes.
#![cfg(feature = "rust-host")]
use headless_luau::runtime_owner::{successful, Runtime};
use serde_json::{json, Value};
fn call(vm: &mut Runtime, request: Value) -> Value {
    let reply = vm.call(request).unwrap();
    successful(&reply).unwrap_or_else(|e| panic!("{e}: {reply}"));
    assert!(
        reply["task_errors"].as_array().is_none_or(|a| a.is_empty()),
        "{reply}"
    );
    for actor in reply["actor_runtime"].as_array().into_iter().flatten() {
        assert!(
            actor["task_errors"].as_array().is_none_or(|a| a.is_empty()),
            "{actor}"
        );
        assert!(actor.get("shadow_merge_error").is_none(), "{actor}");
        assert!(
            actor["shadow_sync"]["conflicts"]
                .as_array()
                .is_none_or(|a| a.is_empty()),
            "{actor}"
        );
    }
    reply
}
fn run(vm: &mut Runtime, source: &str) -> Value {
    call(vm, json!({"op":"execute","source":source,"timeout_ms":100}))["values"].clone()
}
fn settle(vm: &mut Runtime) {
    for _ in 0..4 {
        call(vm, json!({"op":"poll","timeout_ms":100}));
    }
}
fn frame(x: i32, turn: bool) -> Value {
    let rot = if turn {
        [0, 0, 1, 0, 1, 0, -1, 0, 0]
    } else {
        [1, 0, 0, 0, 1, 0, 0, 0, 1]
    };
    json!({"$cframe":{"position":[x,0,0],"rotation":rot}})
}
fn fixture() -> Runtime {
    let mut vm=Runtime::new(json!({"version":1,"sequence":0,"local_player":"player","services":{"Workspace":"ws","Players":"players"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game"},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game"},
        {"id":"players","class_name":"Players","name":"Players","parent":"game"},
        {"id":"player","class_name":"Player","name":"LocalPlayer","parent":"players","properties":{"Character":{"$ref":"char"}}},
        {"id":"char","class_name":"Model","name":"Character","parent":"ws"},
        {"id":"root","class_name":"Part","name":"HumanoidRootPart","parent":"char","properties":{"CFrame":frame(0,false)}},
        {"id":"limb","class_name":"Part","name":"Limb","parent":"char","properties":{"CFrame":frame(2,false)}},
        {"id":"attachment","class_name":"Attachment","name":"Attachment","parent":"root","properties":{"CFrame":frame(1,false)}},
        {"id":"loose","class_name":"Part","name":"Loose","parent":"ws","server_owned":true,"properties":{"CFrame":frame(0,false),"Anchored":false,"NetworkOwnerV3":7,"AssemblyLinearVelocity":{"$vector3":[0,0,0]},"AssemblyAngularVelocity":{"$vector3":[0,0,0]}}},
        {"id":"actor","class_name":"Actor","name":"Worker","parent":"ws"},
        {"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes":{},"attributes_known":true}
    ],"script_sources":{"reader":{"source":r#"
        local counts={};local p=workspace.Character.HumanoidRootPart;local loose=workspace.Loose
        local generic=0;p.Changed:Connect(function() generic+=1;script:SetAttribute('GenericChanges',generic) end)
        p:GetPropertyChangedSignal('CFrame'):Connect(function() script:SetAttribute('LimbX',workspace.Character.Limb.Position.X) end)
        p.Attachment:GetPropertyChangedSignal('WorldPosition'):Connect(function()
            script:SetAttribute('AttachmentX',p.Attachment.WorldPosition.X)
            script:SetAttribute('AttachmentChanges',(script:GetAttribute('AttachmentChanges') or 0)+1)
        end)
        for _,item in ipairs({p,loose}) do
            for _,key in ipairs({'CFrame','Position','Orientation','Rotation','AssemblyLinearVelocity','AssemblyAngularVelocity'}) do
                item:GetPropertyChangedSignal(key):Connect(function(...)
                    assert(select('#',...)==0)
                    local k=item.Name..key;counts[k]=(counts[k] or 0)+1;script:SetAttribute(k,counts[k])
                    script:SetAttribute(item.Name..'X',item.Position.X)
                end)
            end
        end
        script.Parent:BindToMessage('sample',function()
            script:SetAttribute('ReadX',p.Position.X);script:SetAttribute('LooseReadX',loose.Position.X)
            script:SetAttribute('VelocityX',loose.AssemblyLinearVelocity.X)
            script:SetAttribute('LimbReadX',workspace.Character.Limb.Position.X)
        end)
    "#}}}),32).unwrap();
    run(
        &mut vm,
        r#"
        p=workspace.Character.HumanoidRootPart;loose=workspace.Loose;a=workspace.Worker.Reader;counts={}
        attachmentChanges=0;genericChanges=0;p.Changed:Connect(function() genericChanges+=1 end)
        p:GetPropertyChangedSignal('CFrame'):Connect(function() limbX=workspace.Character.Limb.Position.X end)
        p.Attachment:GetPropertyChangedSignal('WorldPosition'):Connect(function() attachmentChanges+=1;attachmentX=p.Attachment.WorldPosition.X end)
        for _,item in ipairs({p,loose}) do
            for _,key in ipairs({'CFrame','Position','Orientation','Rotation','AssemblyLinearVelocity','AssemblyAngularVelocity'}) do
                item:GetPropertyChangedSignal(key):Connect(function(...)
                    assert(select('#',...)==0);local k=item.Name..key;counts[k]=(counts[k] or 0)+1
                end)
            end
        end
    "#,
    );
    settle(&mut vm);
    vm
}
fn pose(vm: &mut Runtime, x: i32, turn: bool) -> Value {
    call(
        vm,
        json!({"op":"physics_feedback","feedback":{},"engine_poses":{"character_id":"char","root_id":"root","frames":[{"id":"root","cframe":frame(x,turn)}]}}),
    )
}
fn counts(vm: &mut Runtime, name: &str) -> Value {
    run(vm,&format!("local n='{name}';return counts[n..'CFrame'] or 0,counts[n..'Position'] or 0,counts[n..'Orientation'] or 0,counts[n..'Rotation'] or 0,a:GetAttribute(n..'CFrame') or 0,a:GetAttribute(n..'Position') or 0,a:GetAttribute(n..'Orientation') or 0,a:GetAttribute(n..'Rotation') or 0"))
}
#[test]
fn engine_translation_and_rotation_update_reads_without_property_signals() {
    let mut vm = fixture();
    run(&mut vm,"game:GetService('RunService').PreSimulation:Connect(function() simulatedX=p.Position.X end)");
    pose(&mut vm, 10, false);
    settle(&mut vm);
    run(&mut vm, "workspace.Worker:SendMessage('sample')");
    settle(&mut vm);
    assert_eq!(
        counts(&mut vm, "HumanoidRootPart"),
        json!([0., 0., 0., 0., 0., 0., 0., 0.])
    );
    assert_eq!(
        run(&mut vm, "return p.Position.X,a:GetAttribute('ReadX')"),
        json!([10., 10.])
    );
    assert_eq!(
        run(
            &mut vm,
            "return genericChanges,a:GetAttribute('GenericChanges') or 0,simulatedX"
        ),
        json!([0., 0., 10.])
    );
    pose(&mut vm, 10, false);
    settle(&mut vm);
    assert_eq!(
        counts(&mut vm, "HumanoidRootPart"),
        json!([0., 0., 0., 0., 0., 0., 0., 0.])
    );
    pose(&mut vm, 10, true);
    settle(&mut vm);
    assert_eq!(
        counts(&mut vm, "HumanoidRootPart"),
        json!([0., 0., 0., 0., 0., 0., 0., 0.])
    );
}
#[test]
fn engine_pose_replaces_old_actor_script_overlay_without_authoring_a_mutation() {
    let mut vm = fixture();
    run(&mut vm, "p.CFrame=CFrame.new(50,0,0)");
    settle(&mut vm);
    let reply = pose(&mut vm, 10, false);
    assert_eq!(reply["engine_pose_result"]["status"], "applied");
    let first_poll = call(&mut vm, json!({"op":"poll","timeout_ms":100}));
    assert!(
        first_poll["mutations"]
            .as_array()
            .is_none_or(|rows| rows.iter().all(|r| r["id"] != "root")),
        "{first_poll}"
    );
    settle(&mut vm);
    run(&mut vm, "workspace.Worker:SendMessage('sample')");
    settle(&mut vm);
    assert_eq!(
        run(&mut vm, "return p.Position.X,a:GetAttribute('ReadX')"),
        json!([10., 10.])
    );
}
fn owned(vm: &mut Runtime, seq: u64, x: i32, velocity: i32) -> Value {
    call(
        vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":seq,"actions":[],"engine_owned_part_updates":{"scope":7,"source_revision":seq,"frames":[{"id":"loose","cframe":frame(x,false),"linear_velocity":[velocity,0,0],"angular_velocity":[0,0,0]}]}}}),
    )
}
#[test]
fn owned_body_poses_and_velocity_reach_actor_without_duplicate_events() {
    let mut vm = fixture();
    run(&mut vm, "loose.CFrame=CFrame.new(50,0,0)");
    settle(&mut vm);
    let before = counts(&mut vm, "Loose");
    let reply = owned(&mut vm, 1, 10, 3);
    assert!(
        reply["mutations"]
            .as_array()
            .is_none_or(|rows| rows.iter().all(|r| r["id"] != "loose")),
        "{reply}"
    );
    settle(&mut vm);
    let after = counts(&mut vm, "Loose");
    assert_eq!(after, before);
    run(&mut vm, "workspace.Worker:SendMessage('sample')");
    settle(&mut vm);
    assert_eq!(run(&mut vm,"return loose.Position.X,a:GetAttribute('LooseReadX'),a:GetAttribute('VelocityX'),counts.LooseAssemblyLinearVelocity or 0,a:GetAttribute('LooseAssemblyLinearVelocity') or 0,counts.LooseAssemblyAngularVelocity or 0,a:GetAttribute('LooseAssemblyAngularVelocity') or 0"),json!([10.,10.,3.,0.,0.,0.,0.]));
    owned(&mut vm, 2, 10, 3);
    settle(&mut vm);
    assert_eq!(counts(&mut vm, "Loose"), after);
}

#[test]
fn invalid_or_stale_character_poses_do_not_write_or_notify_either_vm() {
    let mut vm = fixture();
    let stale = call(
        &mut vm,
        json!({"op":"physics_feedback","feedback":{},"engine_poses":{"character_id":"old-character","root_id":"root","frames":[{"id":"root","cframe":frame(10,false)}]}}),
    );
    assert_eq!(stale["engine_pose_result"]["status"], "stale_character");
    let rejected=vm.call(json!({"op":"physics_feedback","feedback":{},"engine_poses":{"character_id":"char","root_id":"root","frames":[{"id":"root","cframe":frame(10,false)},{"id":"missing","cframe":frame(20,false)}]}}));
    assert!(rejected.is_err() || !rejected.unwrap()["ok"].as_bool().unwrap_or(true));
    settle(&mut vm);
    assert_eq!(
        counts(&mut vm, "HumanoidRootPart"),
        json!([0., 0., 0., 0., 0., 0., 0., 0.])
    );
    run(&mut vm, "workspace.Worker:SendMessage('sample')");
    settle(&mut vm);
    assert_eq!(
        run(&mut vm, "return p.Position.X,a:GetAttribute('ReadX')"),
        json!([0., 0.])
    );
}

#[test]
fn stale_owned_body_feedback_cannot_overwrite_a_newer_received_frame() {
    let mut vm = fixture();
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":1,"actions":[{"kind":"set","sequence":1,"id":"loose","fields":{"CFrame":frame(8,false)}}]}}),
    );
    settle(&mut vm);
    let before = counts(&mut vm, "Loose");
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":2,"actions":[],"engine_owned_part_updates":{"scope":7,"source_revision":0,"frames":[{"id":"loose","cframe":frame(20,false),"linear_velocity":[3,0,0],"angular_velocity":[0,0,0]}]}}}),
    );
    settle(&mut vm);
    assert_eq!(counts(&mut vm, "Loose"), before);
    run(&mut vm, "workspace.Worker:SendMessage('sample')");
    settle(&mut vm);
    assert_eq!(
        run(
            &mut vm,
            "return loose.Position.X,a:GetAttribute('LooseReadX'),a:GetAttribute('VelocityX')"
        ),
        json!([8., 8., 0.])
    );
}

#[test]
fn multi_part_feedback_keeps_reads_current_and_refreshes_attachment_observations() {
    let mut vm = fixture();
    call(
        &mut vm,
        json!({"op":"physics_feedback","feedback":{},"engine_poses":{"character_id":"char","root_id":"root","frames":[{"id":"root","cframe":frame(10,false)},{"id":"limb","cframe":frame(12,false)}]}}),
    );
    settle(&mut vm);
    run(&mut vm, "workspace.Worker:SendMessage('sample')");
    settle(&mut vm);
    assert_eq!(run(&mut vm,"return workspace.Character.Limb.Position.X,a:GetAttribute('LimbReadX'),attachmentX,a:GetAttribute('AttachmentX'),attachmentChanges,a:GetAttribute('AttachmentChanges'),limbX==nil,a:GetAttribute('LimbX')==nil"),json!([12.,12.,11.,11.,1.,1.,true,true]));
    call(
        &mut vm,
        json!({"op":"physics_feedback","feedback":{},"engine_poses":{"character_id":"char","root_id":"root","frames":[{"id":"root","cframe":frame(10,false)},{"id":"limb","cframe":frame(12,false)}]}}),
    );
    settle(&mut vm);
    assert_eq!(
        run(
            &mut vm,
            "return attachmentChanges,a:GetAttribute('AttachmentChanges')"
        ),
        json!([1., 1.])
    );
}
