//! Humanoid state observations through the official Luau VM.
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

fn snapshot() -> Value {
    json!({"version":1,"sequence":0,"local_player":"player","services":{"Workspace":"ws","Players":"players"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game"},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game"},
        {"id":"players","class_name":"Players","name":"Players","parent":"game"},
        {"id":"player","class_name":"Player","name":"LocalPlayer","parent":"players","properties":{"Character":{"$ref":"char"}}},
        {"id":"char","class_name":"Model","name":"Character","parent":"ws"},
        {"id":"hum","class_name":"Humanoid","name":"Humanoid","parent":"char","properties":{"Health":100,"MaxHealth":100,"HumanoidStateType":8}},
        {"id":"root","class_name":"Part","name":"HumanoidRootPart","parent":"char","properties":{"AssemblyLinearVelocity":{"$vector3":[0,0,0]}}}
    ]})
}
fn fixture() -> Runtime {
    Runtime::new(snapshot(), 32).unwrap()
}
fn actor_fixture() -> Runtime {
    let mut snapshot = snapshot();
    snapshot["nodes"].as_array_mut().unwrap().extend([
        json!({"id":"actor","class_name":"Actor","name":"Worker","parent":"ws"}),
        json!({"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}})
    ]);
    snapshot["script_sources"] = json!({"reader":{"source":r#"
        local h=workspace.Character.Humanoid;local states={};local speeds={}
        local results={}
        h.MoveToFinished:Connect(function(reached)
            table.insert(results,tostring(reached))
            script:SetAttribute('MoveToResults',table.concat(results,','))
        end)
        task.spawn(function() script:SetAttribute('MoveToWait',h.MoveToFinished:Wait()) end)
        h.StateChanged:Connect(function(old,new)
            assert(old==Enum.HumanoidStateType[old.Name] and new==Enum.HumanoidStateType[new.Name])
            table.insert(states,old.Name..'>'..new.Name);script:SetAttribute('States',table.concat(states,','))
            script:SetAttribute('Current',h:GetState().Value)
        end)
        h.Running:Connect(function(speed)table.insert(speeds,speed);script:SetAttribute('Speeds',table.concat(speeds,',')) end)
        local root=workspace.Character.HumanoidRootPart;local velocities=0
        root:GetPropertyChangedSignal('AssemblyLinearVelocity'):Connect(function(...)
            assert(select('#',...)==0);velocities+=1
            script:SetAttribute('VelocityChanges',velocities);script:SetAttribute('VelocityX',root.AssemblyLinearVelocity.X)
        end)
        script.Parent:BindToMessage('velocity-read',function() script:SetAttribute('ReadVelocityX',root.AssemblyLinearVelocity.X) end)
        script.Parent:BindToMessage('state',function(value)script:SetAttribute('EnumMessage',value==Enum.HumanoidStateType.Jumping)end)
    "#}});
    let mut vm = Runtime::new(snapshot, 32).unwrap();
    settle(&mut vm);
    vm
}
fn settle(vm: &mut Runtime) {
    for _ in 0..4 {
        call(vm, json!({"op":"poll","timeout_ms":100}));
    }
}
fn feedback(vm: &mut Runtime, value: Value) {
    call(vm, json!({"op":"physics_feedback","feedback":value}));
    settle(vm);
}
#[test]
fn move_to_completion_resumes_root_and_actor_waiters_once_in_order() {
    let mut vm = actor_fixture();
    execute(&mut vm, "results={};local h=workspace.Character.Humanoid;h.MoveToFinished:Connect(function(reached) table.insert(results,tostring(reached)) end);task.spawn(function() waited=h.MoveToFinished:Wait() end)");
    settle(&mut vm);
    let events = json!({"humanoid_id":"hum","move_to_finished":[
        {"sequence":4,"reached":true},{"sequence":8,"reached":false}
    ]});
    feedback(&mut vm, events.clone());
    feedback(&mut vm, events);
    assert_eq!(execute(&mut vm,
        "return table.concat(results,','),waited,workspace.Worker.Reader:GetAttribute('MoveToResults'),workspace.Worker.Reader:GetAttribute('MoveToWait')"),
        json!(["true,false",true,"true,false",true]));
    feedback(&mut vm, json!({"humanoid_id":"hum","move_to_finished":[{"sequence":9,"reached":true}]}));
    assert_eq!(execute(&mut vm, "return table.concat(results,','),workspace.Worker.Reader:GetAttribute('MoveToResults')"),
        json!(["true,false,true","true,false,true"]));
}

#[test]
fn malformed_move_to_results_do_not_commit_other_feedback_or_fire_events() {
    let mut vm = fixture();
    execute(&mut vm, "finished=0;workspace.Character.Humanoid.MoveToFinished:Connect(function() finished+=1 end)");
    for events in [
        json!([{"sequence":1,"reached":true},{"sequence":1,"reached":false}]),
        json!([{"sequence":1,"reached":"true"}]),
        json!([{"sequence":0,"reached":true}]),
        json!({"sequence":1,"reached":true}),
    ] {
        let reply = vm.call(json!({"op":"physics_feedback","feedback":{
            "humanoid_id":"hum","state":{"name":"Jumping","value":3},
            "move_to_finished":events}}));
        assert!(reply.is_err() || reply.unwrap()["ok"] == false);
    }
    settle(&mut vm);
    assert_eq!(execute(&mut vm, "return finished,workspace.Character.Humanoid:GetState().Value"), json!([0.,8.]));
    feedback(&mut vm, json!({"humanoid_id":"hum","move_to_finished":[{"sequence":1,"reached":false}]}));
    assert_eq!(execute(&mut vm, "return finished"), json!([1.]));
}

#[test]
fn movement_feedback_validation_is_atomic_and_direction_events_ignore_strength_changes() {
    let mut vm = fixture();
    execute(&mut vm, "h=workspace.Character.Humanoid;h.WalkSpeed=16;changes=0;h:GetPropertyChangedSignal('MoveDirection'):Connect(function()changes+=1 end)");
    for (sequence,input) in [(10,json!([0.25,0,0])),(11,json!([0.5,0,0])),(12,json!([0,0,0]))] {
        feedback(&mut vm, json!({"humanoid_id":"hum","move_input":input,"move_sequence":sequence}));
    }
    assert_eq!(execute(&mut vm, "return changes,h:GetMoveVelocity().Magnitude"), json!([2.,0.]));
    for input in [json!([2,0,0]),json!([0,1,0]),json!([0,0]),json!(["bad",0,0])] {
        let reply = vm.call(json!({"op":"physics_feedback","feedback":{"humanoid_id":"hum","state":{"name":"Jumping","value":3},"move_input":input,"move_sequence":13}}));
        assert!(reply.is_err() || reply.unwrap()["ok"]==false);
    }
    let reply = vm.call(json!({"op":"physics_feedback","feedback":{"humanoid_id":"hum","move_input":[1,0,0]}}));
    assert!(reply.is_err() || reply.unwrap()["ok"]==false);
    assert_eq!(execute(&mut vm, "return changes,h:GetState().Value,h.MoveDirection.Magnitude"), json!([2.,8.,0.]));
}

#[test]
fn get_state_returns_the_canonical_enum_item() {
    let mut vm = fixture();
    assert_eq!(execute(&mut vm,"local s=workspace.Character.Humanoid:GetState(); return s==Enum.HumanoidStateType.Running,s.EnumType==Enum.HumanoidStateType,typeof(s),typeof(s.EnumType),s.EnumType:FromValue(8)==s"),json!([true,true,"EnumItem","Enum",true]));
}

#[test]
fn character_motion_feedback_updates_reads_without_velocity_property_signals() {
    let mut vm=actor_fixture();
    execute(&mut vm,"velocityChanges=0;local r=workspace.Character.HumanoidRootPart;r:GetPropertyChangedSignal('AssemblyLinearVelocity'):Connect(function(...) assert(select('#',...)==0);velocityChanges+=1;velocityX=r.AssemblyLinearVelocity.X end)");
    for speed in [3,3,7] {
        feedback(&mut vm,json!({"humanoid_id":"hum","motion":{"root_id":"root","linear_velocity":[speed,0,0]}}));
    }
    execute(&mut vm,"workspace.Worker:SendMessage('velocity-read')");settle(&mut vm);
    assert_eq!(execute(&mut vm,"return velocityChanges,velocityX==nil,workspace.Worker.Reader:GetAttribute('VelocityChanges')==nil,workspace.Character.HumanoidRootPart.AssemblyLinearVelocity.X,workspace.Worker.Reader:GetAttribute('ReadVelocityX')"),json!([0.,true,true,7.,7.]));
}
#[test]
fn first_physics_transition_uses_received_state_and_emits_active_state_events() {
    let mut vm = fixture();
    execute(
        &mut vm,
        r#"
        h=workspace.Character.Humanoid;trace={}
        h.StateChanged:Connect(function(old,new)
            assert(old==Enum.HumanoidStateType[old.Name] and new==Enum.HumanoidStateType[new.Name])
            table.insert(trace,old.Name..'>'..new.Name)
        end)
        h.Jumping:Connect(function(active) table.insert(trace,'jump:'..tostring(active)) end)
        h.FreeFalling:Connect(function(active) table.insert(trace,'fall:'..tostring(active)) end)
    "#,
    );
    feedback(
        &mut vm,
        json!({"humanoid_id":"hum","state":{"name":"Jumping","value":3}}),
    );
    feedback(
        &mut vm,
        json!({"humanoid_id":"hum","state":{"name":"Freefall","value":5}}),
    );
    feedback(
        &mut vm,
        json!({"humanoid_id":"hum","state":{"name":"Freefall","value":5}}),
    );
    assert_eq!(
        execute(
            &mut vm,
            "return table.concat(trace,','),h:GetState()==Enum.HumanoidStateType.Freefall"
        ),
        json!([
            "Running>Jumping,jump:true,Jumping>Freefall,jump:false,fall:true",
            true
        ])
    );
}
#[test]
fn running_is_a_typed_speed_signal_and_equal_feedback_is_silent() {
    let mut vm = fixture();
    execute(&mut vm,"speeds={};workspace.Character.Humanoid.Running:Connect(function(...) assert(select('#',...)==1);table.insert(speeds,(...)) end)");
    for speed in [4., 4., 0.] {
        feedback(&mut vm, json!({"humanoid_id":"hum","running_speed":speed}));
    }
    assert_eq!(
        execute(&mut vm, "return table.concat(speeds,',')"),
        json!(["4,0"])
    );
}
#[test]
fn invalid_later_feedback_fields_leave_all_observations_unchanged() {
    let mut vm = fixture();
    let reply=vm.call(json!({"op":"physics_feedback","feedback":{"humanoid_id":"hum","running_speed":4,"state":{"name":"Jumping","value":3},"motion":{"root_id":"root","linear_velocity":[1,2]}}}));
    assert!(reply.is_err() || !reply.as_ref().unwrap()["ok"].as_bool().unwrap_or(true));
    assert_eq!(execute(&mut vm,"local h=workspace.Character.Humanoid;local ok=pcall(function() return h.__HumanoidRunningFeedback end);return h:GetState().Value,workspace.Character.HumanoidRootPart.AssemblyLinearVelocity.X,ok"),json!([8.,0.,false]));
}
#[test]
fn received_states_preserve_intermediate_events() {
    let mut vm = fixture();
    execute(&mut vm,"h=workspace.Character.Humanoid;states={};h.StateChanged:Connect(function(old,new)table.insert(states,old.Name..'>'..new.Name)end)");
    let batch = json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":3,"actions":[
        {"kind":"set","id":"hum","sequence":1,"fields":{"HumanoidStateType":3}},
        {"kind":"set","id":"hum","sequence":2,"fields":{"HumanoidStateType":5}},
        {"kind":"set","id":"hum","sequence":3,"fields":{"HumanoidStateType":8}}
    ]}});
    call(&mut vm, batch.clone());
    settle(&mut vm);
    assert_eq!(
        execute(&mut vm, "return table.concat(states,',')"),
        json!(["Running>Jumping,Jumping>Freefall,Freefall>Running"])
    );
    call(&mut vm, batch);
    settle(&mut vm);
    assert_eq!(execute(&mut vm, "return #states"), json!([3.]));
}

#[test]
fn physics_and_received_states_reach_actor_once_and_clear_stale_feedback() {
    let mut vm = actor_fixture();
    feedback(
        &mut vm,
        json!({"humanoid_id":"hum","running_speed":4,"state":{"name":"Jumping","value":3},"motion":{"root_id":"root","linear_velocity":[4,2,0]}}),
    );
    execute(
        &mut vm,
        "workspace.Worker:SendMessage('state',workspace.Character.Humanoid:GetState())",
    );
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"local a=workspace.Worker.Reader;return a:GetAttribute('States'),a:GetAttribute('Current'),a:GetAttribute('Speeds'),a:GetAttribute('EnumMessage')"),json!(["Running>Jumping",3.,"4",true]));
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":1,"actions":[
            {"kind":"set","id":"hum","sequence":1,"fields":{"HumanoidStateType":{"$enum":{"type":"HumanoidStateType","name":"Freefall","value":5}}}}
        ]}}),
    );
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return workspace.Character.Humanoid:GetState()==Enum.HumanoidStateType.Freefall,workspace.Worker.Reader:GetAttribute('States'),workspace.Worker.Reader:GetAttribute('Current')"),json!([true,"Running>Jumping,Jumping>Freefall",5.]));
}
#[test]
fn state_enabled_changes_have_canonical_state_and_boolean_arguments() {
    let mut vm = fixture();
    execute(
        &mut vm,
        r#"
        local h=workspace.Character.Humanoid;enabled={}
        h.StateEnabledChanged:Connect(function(state,active)
            assert(state==Enum.HumanoidStateType.Jumping and typeof(active)=='boolean')
            table.insert(enabled,tostring(active))
        end)
        h:SetStateEnabled(Enum.HumanoidStateType.Jumping,false)
        h:SetStateEnabled(Enum.HumanoidStateType.Jumping,false)
        h:SetStateEnabled(Enum.HumanoidStateType.Jumping,true)
    "#,
    );
    settle(&mut vm);
    assert_eq!(
        execute(&mut vm, "return table.concat(enabled,',')"),
        json!(["false,true"])
    );
}
#[test]
fn boolean_state_events_report_enter_and_leave_without_fabricating_speed() {
    let mut vm = fixture();
    execute(
        &mut vm,
        r#"
        local h=workspace.Character.Humanoid;events={}
        for _,key in ipairs({'FallingDown','Ragdoll','GettingUp','Jumping','FreeFalling','Strafing','PlatformStanding'}) do
            h[key]:Connect(function(active)table.insert(events,key..':'..tostring(active))end)
        end
    "#,
    );
    for (name, value) in [
        ("FallingDown", 0),
        ("Ragdoll", 1),
        ("GettingUp", 2),
        ("Jumping", 3),
        ("Freefall", 5),
        ("StrafingNoPhysics", 11),
        ("PlatformStanding", 14),
        ("Running", 8),
    ] {
        feedback(
            &mut vm,
            json!({"humanoid_id":"hum","state":{"name":name,"value":value}}),
        );
    }
    assert_eq!(execute(&mut vm,"return table.concat(events,',')"),json!(["FallingDown:true,FallingDown:false,Ragdoll:true,Ragdoll:false,GettingUp:true,GettingUp:false,Jumping:true,Jumping:false,FreeFalling:true,FreeFalling:false,Strafing:true,Strafing:false,PlatformStanding:true,PlatformStanding:false"]));
}
#[test]
fn invalid_state_and_motion_feedback_never_change_root_or_actor_observations() {
    let mut vm = actor_fixture();
    for value in [
        json!({"state":{"name":"Running","value":8}}),
        json!({"humanoid_id":"hum","running_speed":4,"state":{"name":"Running","value":4294967304u64}}),
        json!({"humanoid_id":"hum","running_speed":4,"state":{"name":"Running","value":3}}),
        json!({"humanoid_id":"hum","running_speed":4,"state":{"name":"Jumping","value":3},"motion":{"root_id":"ws","linear_velocity":[1,2,3]}}),
    ] {
        let result = vm.call(json!({"op":"physics_feedback","feedback":value}));
        assert!(result.is_err() || !result.as_ref().unwrap()["ok"].as_bool().unwrap_or(true));
    }
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return workspace.Character.Humanoid:GetState().Value,workspace.Worker.Reader:GetAttribute('States'),workspace.Worker.Reader:GetAttribute('Speeds')"),json!([8.,null,null]));
}
#[test]
fn unknown_state_stays_unknown_until_observed_and_does_not_invent_a_transition() {
    let mut snapshot = snapshot();
    snapshot["nodes"][5]["properties"]
        .as_object_mut()
        .unwrap()
        .remove("HumanoidStateType");
    let mut vm = Runtime::new(snapshot, 32).unwrap();
    execute(&mut vm,"h=workspace.Character.Humanoid;count=0;assert(not pcall(function()h:GetState()end));h.StateChanged:Connect(function()count+=1 end)");
    feedback(
        &mut vm,
        json!({"humanoid_id":"hum","state":{"name":"Jumping","value":3}}),
    );
    assert_eq!(
        execute(
            &mut vm,
            "return h:GetState()==Enum.HumanoidStateType.Jumping,count"
        ),
        json!([true, 0.])
    );
    feedback(
        &mut vm,
        json!({"humanoid_id":"hum","state":{"name":"Freefall","value":5}}),
    );
    assert_eq!(execute(&mut vm, "return count"), json!([1.]));
}
#[test]
fn rejected_received_state_batches_publish_nothing() {
    let mut vm = fixture();
    execute(
        &mut vm,
        "count=0;workspace.Character.Humanoid.StateChanged:Connect(function()count+=1 end)",
    );
    let result=vm.call(json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":2,"actions":[
        {"kind":"set","id":"hum","sequence":1,"fields":{"HumanoidStateType":3}},
        {"kind":"set","id":"missing","sequence":2,"fields":{"Health":0}}
    ]}}));
    assert!(result.is_err() || !result.as_ref().unwrap()["ok"].as_bool().unwrap_or(true));
    settle(&mut vm);
    assert_eq!(
        execute(
            &mut vm,
            "return workspace.Character.Humanoid:GetState().Value,count"
        ),
        json!([8., 0.])
    );
}
