//! Subtree hierarchy changes through received transactions and local Luau.
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
fn fixture(parent: Value) -> Runtime {
    Runtime::new(
        json!({"version":1,"sequence":0,"services":{"Workspace":"ws"},"nodes":[
            {"id":"game","class_name":"DataModel","name":"game","properties":{}},
            {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
            {"id":"a","class_name":"Folder","name":"A","parent":"ws","properties":{}},
            {"id":"b","class_name":"Folder","name":"B","parent":"ws","properties":{}},
            {"id":"root","class_name":"Folder","name":"Root","parent":parent,"properties":{}},
            {"id":"nested","class_name":"Folder","name":"Nested","parent":"root","properties":{}},
            {"id":"leaf","class_name":"Part","name":"Leaf","parent":"nested","properties":{}}
        ]}),
        32,
    )
    .unwrap()
}
fn observe(vm: &mut Runtime) {
    execute(
        vm,
        r#"
        root=nil
        events={}; changes=0
        local function listen(target,event,label)
            target[event]:Connect(function(item,parent)
                table.insert(events,label..':'..item.Name..(event=='AncestryChanged' and '>'..(parent and parent.Name or 'nil') or ''))
            end)
        end
        listen(workspace,'DescendantAdded','ws+')
        listen(workspace,'DescendantRemoving','ws-')
        listen(workspace.A,'DescendantAdded','a+')
        listen(workspace.A,'DescendantRemoving','a-')
        listen(workspace.A,'ChildRemoved','a child-')
        listen(workspace.B,'DescendantAdded','b+')
        listen(workspace.B,'DescendantRemoving','b-')
    "#,
    );
}
fn attach_root_observers(vm: &mut Runtime) {
    execute(
        vm,
        r#"
        root.AncestryChanged:Connect(function(item,parent) table.insert(events,'root ancestry:'..item.Name..'>'..(parent and parent.Name or 'nil')) end)
        root.Nested.Leaf.AncestryChanged:Connect(function(item,parent) table.insert(events,'leaf ancestry:'..item.Name..'>'..(parent and parent.Name or 'nil')) end)
        root:GetPropertyChangedSignal('Parent'):Connect(function() changes+=1 end)
    "#,
    );
}
fn counts(vm: &mut Runtime) -> Value {
    execute(
        vm,
        r#"
        local added,removed,ancestry,ordering={},{},{},{}
        for _,event in ipairs(events) do
            if string.sub(event,1,4)=='ws+:' then table.insert(added,string.sub(event,5)) end
            if string.sub(event,1,4)=='ws-:' then table.insert(removed,string.sub(event,5)) end
            if string.sub(event,1,14)=='leaf ancestry:' then table.insert(ancestry,string.sub(event,15)) end
            if string.sub(event,1,3)=='a-:' or string.sub(event,1,8)=='a child-' then table.insert(ordering,event) end
        end
        return table.concat(added,','),table.concat(removed,','),table.concat(ancestry,','),changes,table.concat(ordering,',')
    "#,
    )
}
#[test]
fn local_subtree_moves_notify_descendants_without_shared_ancestor_noise() {
    let mut vm = fixture(Value::Null);
    observe(&mut vm);
    execute(&mut vm,"root=Instance.new('Folder');root.Name='Root';local n=Instance.new('Folder',root);n.Name='Nested';local p=Instance.new('Part',n);p.Name='Leaf'");
    attach_root_observers(&mut vm);
    execute(
        &mut vm,
        "root.Parent=workspace.A;root.Parent=workspace.B;root.Parent=nil",
    );
    assert_eq!(
        counts(&mut vm),
        json!([
            "Root,Nested,Leaf",
            "Root,Nested,Leaf",
            "Root>A,Root>B,Root>nil",
            3.0,
            "a-:Root,a-:Nested,a-:Leaf,a child-:Root"
        ])
    );
}
#[test]
fn received_batch_preserves_intermediate_parent_changes_and_stale_retries_are_silent() {
    let mut vm = fixture(json!("a"));
    observe(&mut vm);
    execute(&mut vm, "root=workspace.A.Root");
    attach_root_observers(&mut vm);
    let request = json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":3,"actions":[
        {"kind":"set","sequence":1,"id":"root","fields":{"Parent":{"$ref":"b"}}},
        {"kind":"set_property","sequence":2,"id":"root","key":"Parent","value":null},
        {"kind":"set","sequence":3,"id":"root","property":"Parent","value":{"$ref":"a"}}
    ]}});
    call(&mut vm, request.clone());
    let expected = json!([
        "Root,Nested,Leaf",
        "Root,Nested,Leaf",
        "Root>B,Root>nil,Root>A",
        3.0,
        "a-:Root,a-:Nested,a-:Leaf,a child-:Root"
    ]);
    assert_eq!(counts(&mut vm), expected);
    call(&mut vm, request);
    assert_eq!(counts(&mut vm), expected);
}
#[test]
fn received_subtree_destruction_removes_each_descendant_once_and_wakes_ancestry_waiters() {
    let mut vm = fixture(json!("a"));
    observe(&mut vm);
    execute(&mut vm, "root=workspace.A.Root");
    attach_root_observers(&mut vm);
    execute(
        &mut vm,
        r#"task.spawn(function() local changed,parent=root.Nested.Leaf.AncestryChanged:Wait(); waited=changed==root and parent==nil end)"#,
    );
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":1,"actions":[
            {"kind":"destroy","sequence":1,"ids":["nested","root"]}
        ]}}),
    );
    assert_eq!(
        counts(&mut vm),
        json!([
            "",
            "Root,Nested,Leaf",
            "Root>nil,Nested>nil,Leaf>nil",
            1.0,
            "a-:Root,a-:Nested,a-:Leaf,a child-:Root"
        ])
    );
    assert_eq!(
        execute(&mut vm, "return waited,root.Parent==nil"),
        json!([true, true])
    );
}
#[test]
fn rejected_hierarchy_receipt_publishes_no_events() {
    let mut vm = fixture(json!("a"));
    observe(&mut vm);
    execute(&mut vm, "root=workspace.A.Root");
    attach_root_observers(&mut vm);
    let failed = vm
        .call(
            json!({"op":"update","delta":{"version":1,"delta":true,"sequence":2,"actions":[
                {"kind":"set","sequence":1,"id":"root","fields":{"Parent":{"$ref":"b"}}},
                {"kind":"unsupported","sequence":2}
            ]}}),
        )
        .unwrap();
    assert_eq!(failed["ok"], false);
    assert_eq!(
        execute(&mut vm, "return #events,changes,root.Parent==workspace.A"),
        json!([0.0, 0.0, true])
    );
}

#[test]
fn orphaned_received_subtree_is_announced_when_its_parent_arrives() {
    let mut vm = fixture(Value::Null);
    observe(&mut vm);
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":3,"actions":[
            {"kind":"create","sequence":1,"node":{"id":"n2","class_name":"Folder","name":"NewNested","parent":"r2"}},
            {"kind":"create","sequence":2,"node":{"id":"l2","class_name":"Part","name":"NewLeaf","parent":"n2"}},
            {"kind":"create","sequence":3,"node":{"id":"r2","class_name":"Folder","name":"NewRoot","parent":"a"}}
        ]}}),
    );
    assert_eq!(
        counts(&mut vm),
        json!(["NewRoot,NewNested,NewLeaf", "", "", 0.0, ""])
    );
}

#[test]
fn actor_and_root_observe_local_and_received_ancestor_moves() {
    let mut vm=Runtime::new(json!({"version":1,"sequence":0,"services":{"Workspace":"ws"},
        "script_sources":{"reader":{"source":r#"
            local root=workspace.A.Root; local events={}
            root.Nested.Leaf.AncestryChanged:Connect(function(changed,parent)
                table.insert(events,changed.Name..'>'..(parent and parent.Name or 'nil'))
                script:SetAttribute('Trace',table.concat(events,','))
            end)
            script.Parent:BindToMessage('move',function() root.Parent=workspace.B; root.Parent=workspace.A end)
        "#}},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","properties":{}},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
        {"id":"a","class_name":"Folder","name":"A","parent":"ws","properties":{}},
        {"id":"b","class_name":"Folder","name":"B","parent":"ws","properties":{}},
        {"id":"root","class_name":"Folder","name":"Root","parent":"a","properties":{}},
        {"id":"nested","class_name":"Folder","name":"Nested","parent":"root","properties":{}},
        {"id":"leaf","class_name":"Part","name":"Leaf","parent":"nested","properties":{}},
        {"id":"actor","class_name":"Actor","name":"Worker","parent":"ws","properties":{}},
        {"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}}
    ]}),32).unwrap();
    observe(&mut vm);
    execute(&mut vm, "root=workspace.A.Root");
    attach_root_observers(&mut vm);
    execute(&mut vm, "workspace.Worker:SendMessage('move')");
    for _ in 0..6 {
        call(&mut vm, json!({"op":"poll","timeout_ms":100}));
    }
    assert_eq!(
        execute(
            &mut vm,
            "return workspace.Worker.Reader:GetAttribute('Trace')"
        ),
        json!(["Root>B,Root>A"])
    );
    let request = json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":3,"actions":[
        {"kind":"set","sequence":1,"id":"root","fields":{"Parent":{"$ref":"b"}}},
        {"kind":"set_property","sequence":2,"id":"root","key":"Parent","value":null},
        {"kind":"set","sequence":3,"id":"root","property":"Parent","value":{"$ref":"a"}}
    ]}});
    call(&mut vm, request.clone());
    for _ in 0..6 {
        call(&mut vm, json!({"op":"poll","timeout_ms":100}));
    }
    assert_eq!(
        execute(
            &mut vm,
            "return workspace.Worker.Reader:GetAttribute('Trace')"
        ),
        json!(["Root>B,Root>A,Root>B,Root>nil,Root>A"])
    );
    let before = counts(&mut vm);
    assert_eq!(before[2], json!("Root>B,Root>A,Root>B,Root>nil,Root>A"));
    assert_eq!(before[3], json!(5.0));
    call(&mut vm, request);
    assert_eq!(counts(&mut vm), before);
}
