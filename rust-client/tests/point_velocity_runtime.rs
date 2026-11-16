//! Point velocity queries use current assembly observations in the official VM.
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
    for a in reply["actor_runtime"].as_array().into_iter().flatten() {
        assert!(
            a["task_errors"].as_array().is_none_or(|a| a.is_empty()),
            "{a}"
        );
        assert!(a.get("shadow_merge_error").is_none(), "{a}");
        assert!(
            a["shadow_sync"]["conflicts"]
                .as_array()
                .is_none_or(|a| a.is_empty()),
            "{a}"
        );
    }
    reply
}
fn run(vm: &mut Runtime, s: &str) -> Value {
    call(vm, json!({"op":"execute","source":s,"timeout_ms":100}))["values"].clone()
}
fn snapshot(class: &str, properties: Value) -> Value {
    json!({"version":1,"sequence":0,"services":{"Workspace":"ws"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game"},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game"},
        {"id":"p","class_name":class,"name":"Part","parent":"ws","properties":properties}
    ]})
}
fn fixture(class: &str, properties: Value) -> Runtime {
    Runtime::new(snapshot(class, properties), 32).unwrap()
}
fn props() -> Value {
    json!({"AssemblyLinearVelocity":{"$vector3":[1,2,3]},"AssemblyAngularVelocity":{"$vector3":[0,2,0]},"AssemblyCenterOfMass":{"$vector3":[10,20,30]}})
}
#[test]
fn received_point_velocity_uses_world_offset_from_center_and_cross_product() {
    for class in ["Part", "MeshPart", "WedgePart"] {
        let mut vm = fixture(class, props());
        assert_eq!(run(&mut vm,"local p=workspace.Part;local v=p:GetVelocityAtPosition(Vector3.new(13,20,34));local c=p:GetVelocityAtPosition(Vector3.new(10,20,30));return typeof(v),v.X,v.Y,v.Z,c.X,c.Y,c.Z"),json!(["Vector3",9.,2.,-3.,1.,2.,3.]));
    }
    let mut vm = fixture(
        "Part",
        json!({"AssemblyLinearVelocity":{"$vector3":[-1,-2,-3]},"AssemblyAngularVelocity":{"$vector3":[2,3,4]},"AssemblyCenterOfMass":{"$vector3":[-10,-20,-30]}}),
    );
    assert_eq!(run(&mut vm,"local v=workspace.Part:GetVelocityAtPosition(Vector3.new(-5,-14,-23));return v.X,v.Y,v.Z"),json!([-4.,4.,-6.]));
}
#[test]
fn pure_translation_needs_no_center_but_unknown_motion_is_not_zero() {
    let mut p = props();
    p.as_object_mut().unwrap().remove("AssemblyCenterOfMass");
    p["AssemblyAngularVelocity"] = json!({"$vector3":[0,0,0]});
    let mut vm = fixture("Part", p.clone());
    assert_eq!(run(&mut vm,"local v=workspace.Part:GetVelocityAtPosition(Vector3.new(100,-20,30));return v.X,v.Y,v.Z"),json!([1.,2.,3.]));
    for key in ["AssemblyLinearVelocity", "AssemblyAngularVelocity"] {
        let mut missing = p.clone();
        missing.as_object_mut().unwrap().remove(key);
        let mut vm = fixture("Part", missing);
        assert_eq!(run(&mut vm,"return pcall(function() return workspace.Part:GetVelocityAtPosition(Vector3.zero) end)" )[0],false);
    }
    p["AssemblyAngularVelocity"] = json!({"$vector3":[0,1,0]});
    let mut vm = fixture("Part", p);
    assert_eq!(run(&mut vm,"return pcall(function() return workspace.Part:GetVelocityAtPosition(Vector3.zero) end)")[0],false);
}
#[test]
fn authored_rigid_members_query_the_selected_roots_motion() {
    let mut vm = fixture("Part", props());
    assert_eq!(
        run(
            &mut vm,
            r#"
        local a=Instance.new('Part',workspace);a.Size=Vector3.new(2,2,2);a.Position=Vector3.zero;a.RootPriority=10
        local b=Instance.new('Part',workspace);b.Size=Vector3.new(2,2,2);b.Position=Vector3.new(4,0,0)
        local weld=Instance.new('WeldConstraint',a);weld.Part0=a;weld.Part1=b
        a.AssemblyLinearVelocity=Vector3.new(1,0,0);a.AssemblyAngularVelocity=Vector3.new(0,0,2)
        local v=b:GetVelocityAtPosition(b.Position);local center=a.AssemblyCenterOfMass
        return v.X,v.Y,v.Z,center.X,center.Y,center.Z
    "#
        ),
        json!([1., 4., 0., 2., 0., 0.])
    );
}
#[test]
fn point_queries_validate_arguments_liveness_and_finite_result() {
    let mut vm = fixture("Part", props());
    assert_eq!(
        run(
            &mut vm,
            r#"
        local p=workspace.Part;local f=p.GetVelocityAtPosition
        local bad1=pcall(function() p:GetVelocityAtPosition({1,2,3}) end)
        local bad2=pcall(function() p:GetVelocityAtPosition(Vector3.new(math.huge,0,0)) end)
        local bad3=pcall(function() f(workspace,Vector3.zero) end)
        p:Destroy();local dead=pcall(function() f(p,Vector3.zero) end)
        return bad1,bad2,bad3,dead
    "#
        ),
        json!([false, false, false, false])
    );
    let mut p = props();
    p["AssemblyAngularVelocity"] = json!({"$vector3":[0,3e38,0]});
    let mut vm = fixture("Part", p);
    assert_eq!(run(&mut vm,"return pcall(function() return workspace.Part:GetVelocityAtPosition(Vector3.new(1e10,0,0)) end)")[0],false);
}
#[test]
fn received_velocity_changes_are_observed_on_the_next_query() {
    let mut vm = fixture("Part", props());
    assert_eq!(
        run(
            &mut vm,
            "return workspace.Part:GetVelocityAtPosition(Vector3.new(13,20,34)).X"
        ),
        json!([9.])
    );
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":1,"actions":[{"kind":"set","sequence":1,"id":"p","fields":{"AssemblyLinearVelocity":{"$vector3":[5,0,0]},"AssemblyAngularVelocity":{"$vector3":[0,0,0]}}}]}}),
    );
    assert_eq!(run(&mut vm,"local v=workspace.Part:GetVelocityAtPosition(Vector3.new(13,20,34));return v.X,v.Y,v.Z"),json!([5.,0.,0.]));
}

#[test]
fn actor_queries_use_new_motion_inside_received_change_callbacks() {
    let mut snapshot = snapshot("Part", props());
    snapshot["nodes"].as_array_mut().unwrap().extend([
        json!({"id":"actor","class_name":"Actor","name":"Worker","parent":"ws"}),
        json!({"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}})
    ]);
    snapshot["script_sources"] = json!({"reader":{"source":r#"
        local p=workspace.Part
        local function sample()
            local v=p:GetVelocityAtPosition(Vector3.new(13,20,34));assert(typeof(v)=='Vector3');script:SetAttribute('X',v.X)
        end
        sample();p:GetPropertyChangedSignal('AssemblyLinearVelocity'):Connect(sample)
    "#}});
    let mut vm = Runtime::new(snapshot, 32).unwrap();
    for _ in 0..4 {
        call(&mut vm, json!({"op":"poll","timeout_ms":100}));
    }
    assert_eq!(
        run(&mut vm, "return workspace.Worker.Reader:GetAttribute('X')"),
        json!([9.])
    );
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":1,"actions":[{"kind":"set","sequence":1,"id":"p","fields":{"AssemblyLinearVelocity":{"$vector3":[5,0,0]},"AssemblyAngularVelocity":{"$vector3":[0,0,0]}}}]}}),
    );
    for _ in 0..4 {
        call(&mut vm, json!({"op":"poll","timeout_ms":100}));
    }
    assert_eq!(
        run(&mut vm, "return workspace.Worker.Reader:GetAttribute('X')"),
        json!([5.])
    );
}
