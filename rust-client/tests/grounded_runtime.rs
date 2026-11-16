//! Grounded means fixed by the current assembly, not merely touching a floor.
#![cfg(feature = "rust-host")]
use headless_luau::runtime_owner::{successful, Runtime};
use serde_json::{json, Value};
fn call(vm: &mut Runtime, request: Value) -> Value {
    let r = vm.call(request).unwrap();
    successful(&r).unwrap_or_else(|e| panic!("{e}: {r}"));
    assert!(
        r["task_errors"].as_array().is_none_or(|a| a.is_empty()),
        "{r}"
    );
    for a in r["actor_runtime"].as_array().into_iter().flatten() {
        assert!(
            a["task_errors"].as_array().is_none_or(|a| a.is_empty()),
            "{a}"
        );
        assert!(a.get("shadow_merge_error").is_none(), "{a}");
    }
    r
}
fn run(vm: &mut Runtime, s: &str) -> Value {
    call(vm, json!({"op":"execute","source":s,"timeout_ms":100}))["values"].clone()
}
fn snapshot() -> Value {
    json!({"version":1,"sequence":0,"services":{"Workspace":"ws"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game"},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game"},
        {"id":"p","class_name":"Part","name":"Part","parent":"ws","properties":{"Anchored":false}},
        {"id":"q","class_name":"Part","name":"Anchor","parent":"ws","properties":{"Anchored":true}},
        {"id":"j","class_name":"WeldConstraint","name":"Joint","parent":"ws","properties":{"Part0":{"$ref":"p"},"Part1":{"$ref":"q"},"Enabled":true}}
    ]})
}
#[test]
fn received_grounding_follows_anchor_and_joint_updates() {
    let mut vm = Runtime::new(snapshot(), 32).unwrap();
    assert_eq!(
        run(
            &mut vm,
            "return workspace.Part:IsGrounded(),workspace.Anchor:IsGrounded()"
        ),
        json!([true, true])
    );
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":1,"actions":[{"kind":"set","sequence":1,"id":"q","fields":{"Anchored":false}}]}}),
    );
    assert_eq!(
        run(
            &mut vm,
            "return workspace.Part:IsGrounded(),workspace.Anchor:IsGrounded()"
        ),
        json!([false, false])
    );
    run(
        &mut vm,
        "workspace.Anchor.Anchored=true;workspace.Joint.Enabled=false",
    );
    assert_eq!(
        run(
            &mut vm,
            "return workspace.Part:IsGrounded(),workspace.Anchor:IsGrounded()"
        ),
        json!([false, true])
    );
}
#[test]
fn local_chain_changes_and_joint_destruction_are_observed() {
    let mut vm = Runtime::new(snapshot(), 32).unwrap();
    assert_eq!(
        run(
            &mut vm,
            r#"
        local a=Instance.new('Part',workspace);local b=Instance.new('Part',workspace);local c=Instance.new('Part',workspace)
        local ab=Instance.new('WeldConstraint',a);ab.Part0=a;ab.Part1=b
        local bc=Instance.new('WeldConstraint',b);bc.Part0=b;bc.Part1=c;c.Anchored=true
        assert(a:IsGrounded() and b:IsGrounded());c.Anchored=false
        assert(not a:IsGrounded() and not b:IsGrounded());b.Anchored=true
        assert(a:IsGrounded() and c:IsGrounded());ab:Destroy();bc:Destroy()
        return a:IsGrounded(),b:IsGrounded(),c:IsGrounded()
    "#
        ),
        json!([false, true, false])
    );
}
#[test]
fn floor_contact_and_nonrigid_rope_do_not_anchor_an_assembly() {
    let mut vm = Runtime::new(snapshot(), 32).unwrap();
    assert_eq!(
        run(
            &mut vm,
            r#"
        local a=Instance.new('Part',workspace);local b=Instance.new('Part',workspace);b.Anchored=true
        a.CFrame=CFrame.new(0,1,0);b.CFrame=CFrame.new(0,-1,0)
        local aa=Instance.new('Attachment',a);local bb=Instance.new('Attachment',b)
        local rope=Instance.new('RopeConstraint',a);rope.Attachment0=aa;rope.Attachment1=bb
        return a:IsGrounded(),b:IsGrounded()
    "#
        ),
        json!([false, true])
    );
}
#[test]
fn unknown_anchor_or_joint_state_is_not_fabricated_as_false() {
    for key in ["Anchored", "Enabled"] {
        let mut s = snapshot();
        let index = if key == "Anchored" { 2 } else { 4 };
        s["nodes"][index]["properties"]
            .as_object_mut()
            .unwrap()
            .remove(key);
        let mut vm = Runtime::new(s, 32).unwrap();
        assert_eq!(
            run(
                &mut vm,
                "return pcall(function() return workspace.Part:IsGrounded() end)"
            )[0],
            false
        );
        assert_eq!(
            run(&mut vm, "return workspace.Anchor:IsGrounded()"),
            json!([true])
        );
    }
}
#[test]
fn invalid_receivers_and_destroyed_parts_are_rejected() {
    let mut vm = Runtime::new(snapshot(), 32).unwrap();
    assert_eq!(run(&mut vm,"local p=workspace.Part;local f=p.IsGrounded;local bad=pcall(function() f(workspace) end);p:Destroy();local dead=pcall(function() f(p) end);return bad,dead"),json!([false,false]));
}

#[test]
fn grounding_queries_in_actor_callbacks_use_the_current_received_assembly() {
    let mut s = snapshot();
    s["nodes"].as_array_mut().unwrap().extend([
        json!({"id":"actor","class_name":"Actor","name":"Worker","parent":"ws"}),
        json!({"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}})
    ]);
    s["script_sources"] = json!({"reader":{"source":r#"
        local function sample() script:SetAttribute('Grounded',workspace.Part:IsGrounded()) end
        sample();workspace.Anchor:GetPropertyChangedSignal('Anchored'):Connect(sample)
    "#}});
    let mut vm = Runtime::new(s, 32).unwrap();
    for _ in 0..4 {
        call(&mut vm, json!({"op":"poll","timeout_ms":100}));
    }
    assert_eq!(
        run(
            &mut vm,
            "return workspace.Worker.Reader:GetAttribute('Grounded')"
        ),
        json!([true])
    );
    call(
        &mut vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":1,"actions":[{"kind":"set","sequence":1,"id":"q","fields":{"Anchored":false}}]}}),
    );
    for _ in 0..4 {
        call(&mut vm, json!({"op":"poll","timeout_ms":100}));
    }
    assert_eq!(
        run(
            &mut vm,
            "return workspace.Worker.Reader:GetAttribute('Grounded')"
        ),
        json!([false])
    );
}

#[test]
fn joints_outside_a_common_world_root_do_not_ground_parts() {
    let mut vm = Runtime::new(snapshot(), 32).unwrap();
    assert_eq!(
        run(
            &mut vm,
            r#"
        local a=Instance.new('Part',workspace);local model=Instance.new('WorldModel',workspace)
        local b=Instance.new('Part',model);b.Anchored=true
        local weld=Instance.new('WeldConstraint',a);weld.Part0=a;weld.Part1=b
        local separate=a:IsGrounded();b.Parent=workspace;local joined=a:IsGrounded()
        a.Parent=nil;return separate,joined,a:IsGrounded()
    "#
        ),
        json!([false, true, false])
    );
}
