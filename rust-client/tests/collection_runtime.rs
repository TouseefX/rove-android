//! Collection notifications observed through the real host and private Actor VM.
#![cfg(feature = "rust-host")]
use headless_luau::runtime_owner::{successful, Runtime};
use serde_json::{json, Value};
fn call(vm: &mut Runtime, request: Value) -> Value {
    let reply = vm.call(request).unwrap();
    successful(&reply).unwrap_or_else(|error| panic!("{error}: {reply}"));
    for actor in reply["actor_runtime"].as_array().into_iter().flatten() {
        assert!(
            actor["task_errors"]
                .as_array()
                .is_none_or(|errors| errors.is_empty()),
            "{actor}"
        );
    }
    assert!(
        reply["task_errors"]
            .as_array()
            .is_none_or(|errors| errors.is_empty()),
        "{reply}"
    );
    reply
}
fn execute(vm: &mut Runtime, source: &str) -> Value {
    call(vm, json!({"op":"execute","source":source,"timeout_ms":100}))["values"].clone()
}
fn poll(vm: &mut Runtime) {
    for _ in 0..6 {
        call(vm, json!({"op":"poll","timeout_ms":100}));
    }
}
fn fixture() -> Runtime {
    Runtime::new(json!({"version":1,"sequence":0,"services":{"Workspace":"ws","CollectionService":"cs"},
        "script_sources":{"reader":{"source":r#"
            local cs=game:GetService('CollectionService'); local events={}
            local function record(prefix,item)
                table.insert(events,prefix..item.Name); script:SetAttribute('Events',table.concat(events,','))
            end
            cs:GetInstanceAddedSignal('watch'):Connect(function(item) record('+',item) end)
            cs:GetInstanceRemovedSignal('watch'):Connect(function(item) record('-',item) end)
            script.Parent:BindToMessage('make',function()
                local folder=Instance.new('Folder'); folder.Name='LocalContainer'
                local item=Instance.new('Part',folder); item.Name='actor'; item:AddTag('watch')
                folder.Parent=workspace
            end)
        "#}},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","properties":{}},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
        {"id":"cs","class_name":"CollectionService","name":"CollectionService","parent":"game","properties":{}},
        {"id":"actor","class_name":"Actor","name":"Worker","parent":"ws","properties":{}},
        {"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}}
    ]}),32).unwrap()
}
fn watch(vm: &mut Runtime) {
    execute(
        vm,
        r#"
        cs=game:GetService('CollectionService'); events={}
        cs:GetInstanceAddedSignal('watch'):Connect(function(item) table.insert(events,'+'..item.Name) end)
        cs:GetInstanceRemovedSignal('watch'):Connect(function(item) table.insert(events,'-'..item.Name) end)
    "#,
    );
    poll(vm);
}
#[test]
fn received_tagged_subtree_enters_and_leaves_both_root_and_actor_vms() {
    let mut vm = fixture();
    watch(&mut vm);
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":4,"actions":[
            {"kind":"create","sequence":1,"node":{"id":"folder","class_name":"Folder","name":"Container","parent":null}},
            {"kind":"create","sequence":2,"node":{"id":"part","class_name":"Part","name":"received","parent":"folder","collection_tags_known":true,"collection_tags":["watch"]}},
            {"kind":"set","sequence":3,"id":"folder","fields":{"Parent":{"$ref":"ws"}}},
            {"kind":"destroy","sequence":4,"id":"folder"}
        ]}}),
    );
    poll(&mut vm);
    assert_eq!(execute(&mut vm,"return table.concat(events,','),workspace.Worker.Reader:GetAttribute('Events'),#cs:GetTagged('watch')"),json!(["+received,-received","+received,-received",0.0]));
}
#[test]
fn actor_tagged_creation_and_root_cleanup_notify_each_vm_once() {
    let mut vm = fixture();
    watch(&mut vm);
    execute(&mut vm, "workspace.Worker:SendMessage('make')");
    poll(&mut vm);
    assert_eq!(execute(&mut vm,"return table.concat(events,','),workspace.Worker.Reader:GetAttribute('Events'),#cs:GetTagged('watch')"),json!(["+actor","+actor",1.0]));
    execute(&mut vm, "workspace.LocalContainer:ClearAllChildren()");
    poll(&mut vm);
    assert_eq!(execute(&mut vm,"return table.concat(events,','),workspace.Worker.Reader:GetAttribute('Events'),#cs:GetTagged('watch')"),json!(["+actor,-actor","+actor,-actor",0.0]));
}

#[test]
fn root_callback_cleanup_follows_received_addition_in_actor_event_order() {
    let mut vm = fixture();
    watch(&mut vm);
    execute(
        &mut vm,
        "cs:GetInstanceAddedSignal('watch'):Connect(function(item) item:Destroy() end)",
    );
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":1,"actions":[
            {"kind":"create","sequence":1,"node":{"id":"part","class_name":"Part","name":"ephemeral","parent":"ws","collection_tags_known":true,"collection_tags":["watch"]}}
        ]}}),
    );
    poll(&mut vm);
    assert_eq!(execute(&mut vm,"return table.concat(events,','),workspace.Worker.Reader:GetAttribute('Events'),#cs:GetTagged('watch')"),json!(["+ephemeral,-ephemeral","+ephemeral,-ephemeral",0.0]));
}
