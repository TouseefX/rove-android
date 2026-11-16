#![cfg(feature = "rust-host")]
use headless_luau::runtime_owner::{successful, Runtime};
use serde_json::json;
use std::time::{Duration, Instant};

fn run(source: &str) {
    let mut vm = Runtime::new(
        json!({"version":1,"sequence":1,
            "services":{"Workspace":"workspace"},"nodes":[
            {"id":"workspace","class_name":"Workspace","name":"Workspace","properties":{}}
        ]}),
        32,
    )
    .unwrap();
    let setup = r#"
        local m=Instance.new('Model', workspace)
        local function part(parent,cf,size)
            local p=Instance.new('Part',parent or m)
            p.Anchored=true;p.CFrame=cf or CFrame.new();p.Size=size or Vector3.new(2,2,2)
            return p
        end
        local function same(a,b)
            local av,bv={a:GetComponents()},{b:GetComponents()}
            for i=1,12 do assert(math.abs(av[i]-bv[i])<.0001,tostring(a)..' ~= '..tostring(b)) end
        end
    "#;
    let reply = vm
        .call(json!({"op":"execute","source":format!("{setup}\n{source}"),"timeout_ms":2000}))
        .unwrap();
    successful(&reply).unwrap();
    let job = reply["job_id"].as_u64().unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let status = vm.status(job).unwrap();
        successful(&status).unwrap();
        if status["completed"] == true {
            break;
        }
        assert!(Instant::now() < until, "{status}");
        vm.poll().unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        vm.take_movement_intents().is_empty(),
        "ordinary models must stay local"
    );
}

#[test]
fn primary_origin_ignores_pivot_offset_resets_rotation_and_preserves_part_offsets() {
    run(r#"
        local root=part(nil,CFrame.new(2,3,4)*CFrame.Angles(.2,.7,.1))
        root.PivotOffset=CFrame.new(7,8,9)*CFrame.Angles(.3,.2,.4)
        local relative=CFrame.new(0,3,1)*CFrame.Angles(.1,.2,.3)
        local head=part(nil,root.CFrame*relative)
        m.PrimaryPart=root
        assert(m:MoveTo(Vector3.new(20,30,40))==nil)
        same(root.CFrame,CFrame.new(20,30,40))
        same(head.CFrame,root.CFrame*relative)
        same(m:GetPivot(),root.CFrame*root.PivotOffset)
        same(root.PivotOffset,CFrame.new(7,8,9)*CFrame.Angles(.3,.2,.4))
    "#);
}

#[test]
fn fallback_root_aliases_and_empty_models_work() {
    run(r#"
        m:MoveTo(Vector3.new(9,8,7))
        local root=part(nil,CFrame.new(1,2,3)*CFrame.Angles(0,1,0))
        m:moveTo(Vector3.new(4,5,6));same(root.CFrame,CFrame.new(4,5,6))
        m:move(Vector3.new(7,8,9));same(root.CFrame,CFrame.new(7,8,9))
        local pivot=m:GetPivot()
        root.CFrame=CFrame.new(40,50,60);same(m:GetPivot(),pivot)
    "#);
}

#[test]
fn obstructed_destination_lifts_past_stacked_parts_and_does_not_drift() {
    run(r#"
        local root=part(nil,CFrame.new(-20,1,0));m.PrimaryPart=root
        part(workspace,CFrame.new(0,1,0),Vector3.new(10,2,10))
        part(workspace,CFrame.new(0,4,0),Vector3.new(10,2,10))
        m:MoveTo(Vector3.new(0,1,0));same(root.CFrame,CFrame.new(0,6,0))
        m:MoveTo(root.Position);same(root.CFrame,CFrame.new(0,6,0))
        m:MoveTo(Vector3.new(20,1,0));same(root.CFrame,CFrame.new(20,1,0))
    "#);
}

#[test]
fn collision_placement_ignores_noncollidable_storage_and_other_world_roots() {
    run(r#"
        local root=part(nil,CFrame.new(-20,1,0));m.PrimaryPart=root
        local ghost=part(workspace,CFrame.new(0,1,0),Vector3.new(10,10,10));ghost.CanCollide=false
        local detached=Instance.new('Folder');part(detached,CFrame.new(0,1,0),Vector3.new(10,10,10))
        local other=Instance.new('WorldModel',workspace);part(other,CFrame.new(0,1,0),Vector3.new(10,10,10))
        m:MoveTo(Vector3.new(0,1,0));same(root.CFrame,CFrame.new(0,1,0))
    "#);
}

#[test]
fn failed_arguments_or_foreign_primary_do_not_move_any_part() {
    run(r#"
        local root=part(nil,CFrame.new(3,4,5));m.PrimaryPart=root
        local original=root.CFrame
        assert(not pcall(function() m:MoveTo(CFrame.new()) end))
        assert(not pcall(function() m:MoveTo(Vector3.new(0/0,0,0)) end))
        assert(not pcall(function() m:MoveTo(Vector3.new(),Vector3.new()) end))
        same(root.CFrame,original)
        local outside=part(workspace,CFrame.new(99,99,99));m.PrimaryPart=outside
        assert(not pcall(function() m:MoveTo(Vector3.new(1,2,3)) end))
        same(root.CFrame,original);same(outside.CFrame,CFrame.new(99,99,99))
    "#);
}

#[test]
fn rotated_obstacle_bounds_do_not_block_empty_corners() {
    run(r#"
        local root=part(nil,CFrame.new(-20,1,0),Vector3.new(1,1,1));m.PrimaryPart=root
        part(workspace,CFrame.new(0,1,0)*CFrame.Angles(0,math.pi/4,0),Vector3.new(10,2,1))
        m:MoveTo(Vector3.new(3,1,3));same(root.CFrame,CFrame.new(3,1,3))
        m:MoveTo(Vector3.new(2,1,-2));same(root.CFrame,CFrame.new(2,2.5,-2))
    "#);
}

#[test]
fn property_callbacks_observe_the_complete_model_move() {
    run(r#"
        local root=part(nil,CFrame.new());m.PrimaryPart=root
        local head=part(nil,CFrame.new(0,3,0))
        local count,ready=0,true
        root:GetPropertyChangedSignal('Position'):Connect(function()
            count+=1;ready=ready and head.Position==Vector3.new(20,33,40)
        end)
        m:MoveTo(Vector3.new(20,30,40));task.wait()
        assert(count==1 and ready)
    "#);
}
