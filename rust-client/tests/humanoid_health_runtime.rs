//! Humanoid health methods and notifications through the official Luau VM.
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
        {"id":"game","class_name":"DataModel","name":"game"},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game"},
        {"id":"char","class_name":"Model","name":"Character","parent":"ws"},
        {"id":"hum","class_name":"Humanoid","name":"Humanoid","parent":"char","properties":{"Health":100,"MaxHealth":100}}
    ]});
    if actor {
        snapshot["nodes"].as_array_mut().unwrap().extend([
            json!({"id":"actor","class_name":"Actor","name":"Worker","parent":"ws"}),
            json!({"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}})
        ]);
        snapshot["script_sources"] = json!({"reader":{"source":r#"
            local h=workspace.Character.Humanoid;local health={};local deaths=0
            h.HealthChanged:Connect(function(...) assert(select('#',...)==1);table.insert(health,(...));script:SetAttribute('Health',table.concat(health,',')) end)
            h.Died:Connect(function(...) assert(select('#',...)==0);deaths+=1;script:SetAttribute('Deaths',deaths) end)
            script.Parent:BindToMessage('damage',function(amount) h:TakeDamage(amount) end)
            script.Parent:BindToMessage('max',function(amount) h.MaxHealth=amount end)
        "#}});
    }
    let mut vm = Runtime::new(snapshot, 32).unwrap();
    execute(
        &mut vm,
        r#"
        h=workspace.Character.Humanoid;health={};deaths=0;properties=0
        h.HealthChanged:Connect(function(...) assert(select('#',...)==1);table.insert(health,(...)) end)
        h.Died:Connect(function(...) assert(select('#',...)==0);deaths+=1 end)
        h:GetPropertyChangedSignal('Health'):Connect(function(...) assert(select('#',...)==0);properties+=1 end)
        task.spawn(function() waited=h.HealthChanged:Wait() end)
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
fn received_health_payloads_preserve_intermediates_and_death_in_root_and_actor() {
    let mut vm = fixture(true);
    let batch = json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":4,"actions":[
        {"kind":"set","id":"hum","sequence":1,"fields":{"Health":75}},
        {"kind":"set","id":"hum","sequence":2,"property":"Health","value":75.0},
        {"kind":"set_property","id":"hum","sequence":3,"key":"Health","value":25},
        {"kind":"set","id":"hum","sequence":4,"fields":{"Health":0}}
    ]}});
    call(&mut vm, batch.clone());
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return table.concat(health,','),deaths,properties,waited,h.Health,workspace.Worker.Reader:GetAttribute('Health'),workspace.Worker.Reader:GetAttribute('Deaths')"),json!(["75,25,0",1.,3.,75.,0.,"75,25,0",1.]));
    call(&mut vm, batch);
    settle(&mut vm);
    assert_eq!(execute(&mut vm, "return #health,deaths"), json!([3., 1.]));
}
#[test]
fn take_damage_heals_clamps_and_local_events_reach_actor_once() {
    let mut vm = fixture(true);
    execute(&mut vm,"h:TakeDamage(40);h:TakeDamage(-10);h:TakeDamage(-1e300);h:TakeDamage(1e300);h:TakeDamage(-20)");
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return table.concat(health,','),deaths,h.Health,workspace.Worker.Reader:GetAttribute('Health'),workspace.Worker.Reader:GetAttribute('Deaths')"),json!(["60,70,100,0",1.,0.,"60,70,100,0",1.]));
}
#[test]
fn actor_damage_commits_to_root_and_notifies_each_vm_once() {
    let mut vm = fixture(true);
    execute(&mut vm, "workspace.Worker:SendMessage('damage',25)");
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return table.concat(health,','),h.Health,workspace.Worker.Reader:GetAttribute('Health')"),json!(["75",75.,"75"]));
}

#[test]
fn max_health_change_and_clamp_notify_once_in_each_vm() {
    let mut vm = fixture(true);
    execute(&mut vm, "h.MaxHealth=200;h.MaxHealth=50");
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return h.Health,h.MaxHealth,table.concat(health,','),workspace.Worker.Reader:GetAttribute('Health')"),json!([50.,50.,"100,50","100,50"]));
    execute(&mut vm, "workspace.Worker:SendMessage('max',20)");
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return h.Health,h.MaxHealth,table.concat(health,','),workspace.Worker.Reader:GetAttribute('Health')"),json!([20.,20.,"100,50,20","100,50,20"]));
}
#[test]
fn forcefields_protect_damage_and_healing_but_not_direct_health_writes() {
    let mut vm = fixture(false);
    assert_eq!(
        execute(
            &mut vm,
            r#"
        local c=workspace.Character
        local root=Instance.new('Part');root.Name='HumanoidRootPart';root.Parent=c
        local shield=Instance.new('ForceField');shield.Visible=false;shield.Parent=c
        h:TakeDamage(10);assert(h.Health==100)
        h.Health=60;h:TakeDamage(-10);assert(h.Health==60)
        shield.Parent=root;h:TakeDamage(10);assert(h.Health==60)
        local group=Instance.new('Model');group.Parent=workspace;c.Parent=group
        shield.Parent=group;h:TakeDamage(10);assert(h.Health==60)
        shield.Parent=workspace;h:TakeDamage(10);assert(h.Health==50)
        local unrelated=Instance.new('Folder');unrelated.Parent=c
        shield.Parent=unrelated;h:TakeDamage(10);assert(h.Health==40)
        shield.Parent=c;shield:Destroy();h:TakeDamage(10);assert(h.Health==30)
        return true
    "#
        ),
        json!([true])
    );
}
#[test]
fn died_requires_workspace_and_enabled_dead_state() {
    let mut vm = fixture(false);
    execute(
        &mut vm,
        "h:SetStateEnabled(Enum.HumanoidStateType.Dead,false);h.Health=0",
    );
    settle(&mut vm);
    assert_eq!(
        execute(&mut vm, "return table.concat(health,','),deaths"),
        json!(["0", 0.])
    );
    let mut vm = fixture(false);
    execute(&mut vm, "workspace.Character.Parent=nil;h.Health=0");
    settle(&mut vm);
    assert_eq!(
        execute(&mut vm, "return table.concat(health,','),deaths"),
        json!(["0", 0.])
    );
}
#[test]
fn received_death_uses_ancestry_at_each_transition() {
    let mut vm = fixture(true);
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":4,"actions":[
            {"kind":"set","id":"char","sequence":1,"property":"Parent","value":null},
            {"kind":"set","id":"hum","sequence":2,"property":"Health","value":0},
            {"kind":"set","id":"char","sequence":3,"property":"Parent","value":{"$ref":"ws"}},
            {"kind":"set","id":"hum","sequence":4,"property":"Health","value":0}
        ]}}),
    );
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return table.concat(health,','),deaths,workspace.Worker.Reader:GetAttribute('Health'),workspace.Worker.Reader:GetAttribute('Deaths')"),json!(["0",0.,"0",null]));
}
#[test]
fn invalid_damage_is_rejected_without_mutation() {
    let mut vm = fixture(false);
    execute(
        &mut vm,
        r#"
        for _, value in ipairs({'1',false,{},math.huge,-math.huge,0/0}) do
            assert(not pcall(function() h:TakeDamage(value) end))
        end
        assert(not pcall(function() h:TakeDamage() end))
        assert(not pcall(function() h:TakeDamage(1,2) end))
        assert(not pcall(function() h.Health=math.huge end))
    "#,
    );
    settle(&mut vm);
    assert_eq!(
        execute(&mut vm, "return h.Health,#health,deaths,properties"),
        json!([100., 0., 0., 0.])
    );
}
#[test]
fn rejected_health_receipt_does_not_publish_or_damage() {
    let mut vm = fixture(true);
    let r=vm.call(json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":2,"actions":[
        {"kind":"set","id":"hum","sequence":1,"fields":{"Health":0}},
        {"kind":"set","id":"missing","sequence":2,"fields":{"Health":0}}
    ]}}));
    assert!(r.is_err() || !r.as_ref().unwrap()["ok"].as_bool().unwrap_or(true));
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return h.Health,#health,deaths,properties,workspace.Worker.Reader:GetAttribute('Health')"),json!([100.,0.,0.,0.,null]));
}
#[test]
fn received_max_health_notifies_without_inventing_a_clamped_health() {
    let mut vm = fixture(true);
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":2,"actions":[
            {"kind":"set","id":"hum","sequence":1,"fields":{"MaxHealth":50}},
            {"kind":"set","id":"hum","sequence":2,"fields":{"Health":110}}
        ]}}),
    );
    settle(&mut vm);
    // Received values remain authoritative. Increasing above a known maximum
    // does not emit HealthChanged, but ordinary property listeners still fire.
    assert_eq!(execute(&mut vm,"return h.Health,h.MaxHealth,table.concat(health,','),properties,workspace.Worker.Reader:GetAttribute('Health')"),json!([110.,50.,"100",1.,"100"]));
}

#[test]
fn disabled_dead_state_remains_disabled_when_actor_observes_root_health_change() {
    let mut vm = fixture(true);
    execute(
        &mut vm,
        "h:SetStateEnabled(Enum.HumanoidStateType.Dead,false);h.Health=0;h:SetStateEnabled(Enum.HumanoidStateType.Dead,true)",
    );
    settle(&mut vm);
    assert_eq!(execute(&mut vm,"return deaths,workspace.Worker.Reader:GetAttribute('Deaths'),workspace.Worker.Reader:GetAttribute('Health')"),json!([0.,null,"0"]));
}
#[test]
fn missing_received_health_stays_unknown_and_first_zero_does_not_invent_death() {
    let mut vm=Runtime::new(json!({"version":1,"sequence":0,"services":{"Workspace":"ws"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game"},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game"},
        {"id":"hum","class_name":"Humanoid","name":"Humanoid","parent":"ws","server_owned":true,"properties":{}}
    ]}),32).unwrap();
    execute(
        &mut vm,
        r#"
        h=workspace.Humanoid;health={};deaths=0
        assert(not pcall(function() return h.Health end))
        assert(not pcall(function() h:TakeDamage(1) end))
        h.HealthChanged:Connect(function(value) table.insert(health,value) end)
        h.Died:Connect(function() deaths+=1 end)
    "#,
    );
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":1,"actions":[
            {"kind":"set","id":"hum","sequence":1,"fields":{"Health":0}}
        ]}}),
    );
    settle(&mut vm);
    assert_eq!(
        execute(&mut vm, "return h.Health,table.concat(health,','),deaths"),
        json!([0., "0", 0.])
    );
}
