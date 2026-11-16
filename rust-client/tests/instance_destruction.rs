//! Destruction through real host commands and the Actor mutation journal.
#![cfg(feature = "rust-host")]

use headless_luau::runtime_owner::{successful, Runtime};
use serde_json::{json, Value};

fn call(vm: &mut Runtime, request: Value) -> Value {
    let reply=vm.call(request).unwrap();
    successful(&reply).unwrap_or_else(|error|panic!("{error}: {reply}"));
    assert!(reply["task_errors"].as_array().is_none_or(|errors|errors.is_empty()),"{reply}");
    for actor in reply["actor_runtime"].as_array().into_iter().flatten() {
        assert!(actor["task_errors"].as_array().is_none_or(|errors|errors.is_empty()),"{actor}");
        assert!(actor["shadow_sync"]["conflicts"].as_array().is_none_or(|errors|errors.is_empty()),"{actor}");
        assert!(actor.get("shadow_merge_error").is_none(),"{actor}");
    }
    reply
}
fn execute(vm: &mut Runtime, source: &str) -> Value {
    call(vm,json!({"op":"execute","source":source,"timeout_ms":100}))["values"].clone()
}
fn poll(vm: &mut Runtime) {
    for _ in 0..6 { call(vm,json!({"op":"poll","timeout_ms":100})); }
}

#[test]
fn debris_and_clear_all_children_release_waiters_in_received_subtrees() {
    for operation in ["game:GetService('Debris'):AddItem(target,0)","container:ClearAllChildren()"] {
        let mut vm=Runtime::new(json!({"version":1,"sequence":0,"services":{"Workspace":"ws"},"nodes":[
            {"id":"game","class_name":"DataModel","name":"game","properties":{}},
            {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
            {"id":"container","class_name":"Folder","name":"Container","parent":"ws","properties":{}},
            {"id":"child","class_name":"Folder","name":"Child","parent":"container","properties":{}},
            {"id":"fn","class_name":"BindableFunction","name":"Function","parent":"child","properties":{}}
        ]}),32).unwrap();
        execute(&mut vm,r#"
            container=workspace.Container; target=container.Child; fn=target.Function
            destroyed=0; removed={}
            for _,item in ipairs({target,fn}) do item.Destroying:Connect(function() destroyed+=1 end) end
            connection=fn.Changed:Connect(function() end)
            workspace.DescendantRemoving:Connect(function(item) table.insert(removed,item.Name) end)
            task.spawn(function() child_done=target:WaitForChild('never')==nil end)
            task.spawn(function() local ok,err=pcall(function() fn:Invoke() end); invoke_done=not ok and string.find(err,'destroyed')~=nil end)
        "#);
        execute(&mut vm,operation);
        poll(&mut vm);
        assert_eq!(execute(&mut vm,"return destroyed,table.concat(removed,','),child_done,invoke_done,connection.Connected,container.Parent==workspace,#container:GetChildren()"),json!([2.0,"Child,Function",true,true,false,true,0.0]),"{operation}");
    }
}

#[test]
fn actor_debris_destruction_replays_into_root_and_completes_actor_waiters() {
    let mut vm=Runtime::new(json!({"version":1,"sequence":0,"services":{"Workspace":"ws"},
        "script_sources":{"reader":{"source":r#"
            script.Parent:BindToMessage('begin',function()
            local root=Instance.new('Folder',script.Parent); root.Name='Ephemeral'
            local child=Instance.new('Part',root)
            local connection=child.Changed:Connect(function() end)
            child.Destroying:Connect(function() script:SetAttribute('Destroyed',true) end)
            task.spawn(function()
                local missing=root:WaitForChild('never')
                script:SetAttribute('Released',missing==nil and not connection.Connected)
            end)
            task.defer(function() game:GetService('Debris'):AddItem(root,0) end)
            end)
        "#}},"nodes":[
            {"id":"game","class_name":"DataModel","name":"game","properties":{}},
            {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
            {"id":"actor","class_name":"Actor","name":"Worker","parent":"ws","properties":{}},
            {"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes":{},"attributes_known":true}
        ]}),32).unwrap();
    execute(&mut vm,"workspace.Worker:SendMessage('begin')");
    poll(&mut vm);
    assert_eq!(execute(&mut vm,"return workspace.Worker:FindFirstChild('Ephemeral')==nil,workspace.Worker.Reader:GetAttribute('Destroyed'),workspace.Worker.Reader:GetAttribute('Released')"),json!([true,true,true]));
}
