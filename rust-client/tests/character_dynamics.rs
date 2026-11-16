#![cfg(feature = "rust-host")]
use headless_luau::{
    engine_world_projection::ProjectionContext,
    engine_world_view::SharedWorldView,
    live_physics::LocalPrediction,
    live_runtime::compact_snapshot,
    runtime_owner::{successful, Runtime},
};
use rove_client_core::{schema::Schema, session::SessionRecord, world::WorldStore};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

const IDENTITY: [f64; 9] = [1., 0., 0., 0., 1., 0., 0., 0., 1.];
fn cf(p: [f64; 3]) -> Value {
    json!({"$cframe":{"position":p,"rotation":IDENTITY}})
}
fn fixture() -> Value {
    json!({"version":1,"sequence":1,"local_player":"1:2","services":{"Players":"1:1","Workspace":"1:4"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"Game","properties":{}},
        {"id":"1:1","class_name":"Players","name":"Players","parent":"game","properties":{}},
        {"id":"1:2","class_name":"Player","name":"Runner","parent":"1:1","properties":{"UserId":42,"Character":{"$ref":"1:3"}}},
        {"id":"1:3","class_name":"Model","name":"Runner","parent":"1:4","properties":{"PrimaryPart":{"$ref":"1:6"}}},
        {"id":"1:4","class_name":"Workspace","name":"Workspace","parent":"game","properties":{"Gravity":196.2}},
        {"id":"1:5","class_name":"Humanoid","name":"Humanoid","parent":"1:3","properties":{"WalkSpeed":16,"RootPart":{"$ref":"1:6"}}},
        {"id":"1:6","class_name":"Part","name":"HumanoidRootPart","parent":"1:3","received_creation_complete":true,"properties":{
            "CFrame":cf([0.,2.,0.]),"Size":{"$vector3":[2,4,2]},"Anchored":false,"NetworkOwnerV3":7,"AssemblyRootPart":{"$ref":"1:6"}}},
        {"id":"1:7","class_name":"Player","name":"MiMiAwEe","parent":"1:1","properties":{"UserId":43,"Character":{"$ref":"1:8"}}},
        {"id":"1:8","class_name":"Model","name":"Target","parent":"1:4","properties":{"PrimaryPart":{"$ref":"1:9"}}},
        {"id":"1:9","class_name":"Part","name":"HumanoidRootPart","parent":"1:8","received_creation_complete":true,"properties":{
            "CFrame":cf([10.,2.,20.]),"Size":{"$vector3":[2,4,2]},"Anchored":false,"NetworkOwnerV3":8}},
        {"id":"1:10","class_name":"Part","name":"Head","parent":"1:3","received_creation_complete":true,"properties":{
            "CFrame":cf([0.,5.,0.]),"Size":{"$vector3":[2,2,2]}}}
    ]})
}
fn context() -> Arc<ProjectionContext> {
    ProjectionContext::new(
        &Schema {
            version: 3,
            flags: 0,
            enums: vec![],
            classes: vec![],
            properties: vec![],
            events: vec![],
            strings: vec![],
            names: vec![],
            lr_types: vec![],
            parent_property_id: 0,
            decoded_bytes: 0,
        },
        HashMap::new(),
    )
    .unwrap()
}
fn parents() -> HashMap<String, String> {
    HashMap::from([("Part".into(), "BasePart".into())])
}
fn receipt(delta: Value) -> SessionRecord {
    SessionRecord {
        packet_kind: 161,
        world_sequence: delta["sequence"].as_u64().unwrap(),
        world_delta: delta.into(),
        events: vec![],
        pings: vec![],
        tags: vec![],
        regions: vec![],
        metadata: vec![],
        property_changes: vec![],
    }
}

fn runtime() -> (Runtime, WorldStore) {
    runtime_with_actors(0)
}
fn runtime_with_actors(count: usize) -> (Runtime, WorldStore) {
    let mut snapshot = fixture();
    snapshot["nodes"][4]["properties"]["Gravity"] = json!(10);
    snapshot["nodes"][5]["properties"]["HumanoidStateType"] = json!(8);
    snapshot["nodes"][6]["properties"]["AssemblyLinearVelocity"] = json!({"$vector3":[0,0,0]});
    snapshot["script_sources"] = json!({});
    for i in 0..count {
        let actor = format!("worker{i}");
        let script = format!("script{i}");
        snapshot["nodes"].as_array_mut().unwrap().extend([
            json!({"id":actor,"class_name":"Actor","name":format!("Worker{i}"),"parent":"1:4"}),
            json!({"id":script,"class_name":"LocalScript","name":"Driver","parent":actor,"properties":{"Enabled":true,"Disabled":false},"attributes":{}})
        ]);
        snapshot["script_sources"][&script] = json!({"source":r#"
            local c=game:GetService('Players').LocalPlayer.Character
            script.Parent:BindToMessage('drive',function(x)
                c.Humanoid:ChangeState(Enum.HumanoidStateType.Physics)
                c.HumanoidRootPart.AssemblyLinearVelocity=Vector3.new(x,10,0)
                c.Humanoid:Move(Vector3.new(-1,0,0))
            end)
            script.Parent:BindToMessage('sample',function() script:SetAttribute('State',c.Humanoid:GetState().Name);script:SetAttribute('X',c.HumanoidRootPart.Position.X) end)
            script.Parent:BindToMessage('jump',function() c.Humanoid.Jump=true end)
            script.Parent:BindToMessage('analog',function(x,z) c.Humanoid:Move(Vector3.new(x,0,z)) end)
            script.Parent:BindToMessage('input-read',function()
                script:SetAttribute('MoveX',c.Humanoid.MoveDirection.X)
                script:SetAttribute('IntendedX',c.Humanoid:GetMoveVelocity().X)
            end)
            script.Parent:BindToMessage('follow',function()
                c.Humanoid:MoveTo(Vector3.new(4,0,0),workspace.Target.HumanoidRootPart)
            end)
            script.Parent:BindToMessage('target-pose',function(x)
                workspace.Target.HumanoidRootPart.CFrame=CFrame.new(x,2,0)
            end)
            script.Parent:BindToMessage('cancel-follow',function()
                c.Humanoid.WalkToPart=nil
            end)
            script.Parent:BindToMessage('root-frame',function(x)
                c.HumanoidRootPart.CFrame=CFrame.new(x,12,5)*CFrame.Angles(0,math.pi/2,0)
            end)
            script.Parent:BindToMessage('root-pivot',function(x)
                c.HumanoidRootPart:PivotTo(CFrame.new(x,12,5))
            end)
            script.Parent:BindToMessage('root-alias',function(key,x,y,z)
                c.HumanoidRootPart[key]=Vector3.new(x,y,z)
            end)
            script.Parent:BindToMessage('bulk-root',function(x)
                workspace:BulkMoveTo({c.HumanoidRootPart,workspace.Target.HumanoidRootPart},
                    {CFrame.new(x,12,5),CFrame.new(100,2,20)},Enum.BulkMoveMode.FireCFrameChanged)
            end)
            script.Parent:BindToMessage('gravity-jump',function()
                workspace.Gravity=25
                c.Humanoid.UseJumpPower=false;c.Humanoid.JumpHeight=2
                c.Humanoid.Jump=true
                workspace.Gravity=0
            end)
            script.Parent:BindToMessage('speed',function(value)
                c.Humanoid.WalkSpeed=value
                c.Humanoid:Move(Vector3.new(1,0,0))
            end)
            script.Parent:BindToMessage('pivot',function(x) c:PivotTo(CFrame.new(x,2,0)) end)
            script.Parent:BindToMessage('moves',function(n)
                local accepted=0
                for i=1,n do local ok=pcall(function() c.Humanoid:Move(Vector3.new(i,0,0)) end);if ok then accepted+=1 end end
                script:SetAttribute('Accepted',accepted)
            end)
        "#});
    }
    let world = WorldStore::from_snapshot(&snapshot).unwrap();
    let view = SharedWorldView::projected(world.view(), context());
    compact_snapshot(&mut snapshot, &view).unwrap();
    (
        Runtime::new_with_shared_world(snapshot, 32, &view).unwrap(),
        world,
    )
}
fn run(vm: &mut Runtime, source: &str) -> Value {
    let result = vm.execute_control(source.into(), 100).unwrap();
    successful(&result).unwrap();
    result["values"].clone()
}
fn advance(
    prediction: &mut LocalPrediction,
    world: &WorldStore,
    intents: &[Value],
    now: f64,
) -> headless_luau::live_physics::PredictionFrame {
    prediction
        .advance(&world.view(), &parents(), "1:2", 42, 7, true, intents, now)
        .unwrap()
        .unwrap()
}
#[test]
fn script_gravity_controls_fall_and_feedback_without_rewriting_received_state() {
    let (mut vm, mut world) = runtime();
    let delta = json!({"version":1,"delta":true,"sequence":2,"actions":[
        {"kind":"set","sequence":2,"id":"1:6","fields":{"CFrame":cf([0.,100.,0.])}}
    ]});
    world.apply_delta(&delta).unwrap();
    successful(&vm.call(json!({"op":"update","delta":delta,"timeout_ms":100})).unwrap()).unwrap();
    run(&mut vm, "workspace.Gravity=0");
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    assert_eq!(advance(&mut prediction, &world, &[], 1.1).position[1], 100.);
    run(&mut vm, "workspace.Gravity=20");
    let frame = advance(&mut prediction, &world, &vm.take_movement_intents(), 1.2);
    assert!((frame.velocity[1] + 2.).abs() < 1e-6, "{frame:?}");
    assert!((frame.position[1] - 99.9).abs() < 1e-6, "{frame:?}");
    let feedback = prediction.feedback().unwrap();
    successful(&vm.call(json!({"op":"physics_feedback","engine_poses":feedback["engine_poses"],"feedback":feedback})).unwrap()).unwrap();
    let observed = run(&mut vm, "return workspace.Gravity,game:GetService('Players').LocalPlayer.Character.HumanoidRootPart.Position.Y");
    assert_eq!(observed[0], 20.);
    assert!((observed[1].as_f64().unwrap() - 99.9).abs() < 1e-4);
    assert_eq!(world.view().get_property("1:4", "Gravity"), Some(json!(10)));
    assert_eq!(prediction.report()["accepted_gravity_writes"], 2);
}

#[test]
fn move_to_part_tracks_received_translation_rotation_and_newer_physics_pose() {
    let (mut vm, mut world) = runtime();
    run(&mut vm, "workspace.Gravity=0;h=game:GetService('Players').LocalPlayer.Character.Humanoid;h:MoveTo(Vector3.new(4,0,0),workspace.Target.HumanoidRootPart);assert(h.WalkToPart==workspace.Target.HumanoidRootPart and h.WalkToPoint==Vector3.new(4,0,0))");
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    let frame = advance(&mut prediction, &world, &[], 1.1);
    assert!((frame.velocity[0]/frame.velocity[2] - 14./20.).abs() < 1e-6);
    let start = frame.position;
    world.apply_delta(&json!({"version":1,"delta":true,"sequence":2,"actions":[
        {"kind":"set","sequence":2,"id":"1:9","fields":{"CFrame":{"$cframe":{
            "position":[20,2,0],"rotation":[0,0,1,0,1,0,-1,0,0]}}}}
    ]})).unwrap();
    let frame = advance(&mut prediction, &world, &[], 1.2);
    let dx = 20. - start[0];
    let dz = -4. - start[2];
    assert!((frame.velocity[0] - 16.*dx/dx.hypot(dz)).abs() < 1e-6);
    assert!((frame.velocity[2] - 16.*dz/dx.hypot(dz)).abs() < 1e-6);
    world.apply_delta(&json!({"version":1,"delta":true,"sequence":3,"actions":[
        {"kind":"set","sequence":3,"id":"1:9","fields":{"ReplicationPV":{"position":[-20,2,0],"rotation_id":2}}}
    ]})).unwrap();
    assert!(advance(&mut prediction, &world, &[], 1.3).velocity[0] < 0.);
    assert_eq!(prediction.report()["move_to"]["part_id"], "1:9");
    assert_eq!(world.view().get_property("1:9", "CFrame").unwrap()["$cframe"]["position"], json!([20,2,0]));
}

#[test]
fn unavailable_move_to_part_stops_steering_resumes_and_keeps_original_timeout() {
    let (mut vm, mut world) = runtime();
    run(&mut vm, "workspace.Gravity=0;h=game:GetService('Players').LocalPlayer.Character.Humanoid;h:MoveTo(Vector3.zero,workspace.Target.HumanoidRootPart)");
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    assert!(advance(&mut prediction, &world, &[], 1.1).velocity[0] > 0.);
    // A part outside Workspace is no longer a usable world target.
    world.apply_delta(&json!({"version":1,"delta":true,"sequence":2,"actions":[
        {"kind":"set","sequence":2,"id":"1:9","fields":{"Parent":null}}
    ]})).unwrap();
    let frame = advance(&mut prediction, &world, &[], 1.2);
    assert_eq!(frame.velocity[0], 0.);
    assert_eq!(frame.velocity[2], 0.);
    assert_eq!(prediction.report()["completed_move_to"], 0);
    world.apply_delta(&json!({"version":1,"delta":true,"sequence":3,"actions":[
        {"kind":"set","sequence":3,"id":"1:9","fields":{"Parent":{"$ref":"1:8"}}}
    ]})).unwrap();
    assert!(advance(&mut prediction, &world, &[], 1.3).velocity[0] > 0.);
    world.apply_delta(&json!({"version":1,"delta":true,"sequence":4,"actions":[
        {"kind":"remove","sequence":4,"id":"1:9"}
    ]})).unwrap();
    assert_eq!(advance(&mut prediction, &world, &[], 1.4).velocity[0], 0.);
    advance(&mut prediction, &world, &[], 9.);
    assert_eq!(prediction.report()["expired_move_to"], 1);
    assert_eq!(prediction.feedback().unwrap()["move_to_result"]["reached"], false);
}

#[test]
fn relative_move_to_arrival_delivers_completion_back_to_luau() {
    let (mut vm, mut world) = runtime();
    run(&mut vm, "local h=game:GetService('Players').LocalPlayer.Character.Humanoid;finished=0;h.MoveToFinished:Connect(function(ok) assert(ok);finished+=1 end);h:MoveTo(Vector3.new(4,0,0),workspace.Target.HumanoidRootPart)");
    // The relative destination moves near our current root before the next tick.
    world.apply_delta(&json!({"version":1,"delta":true,"sequence":2,"actions":[
        {"kind":"set","sequence":2,"id":"1:9","fields":{"CFrame":cf([-3.5,2.,0.])}}
    ]})).unwrap();
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    assert_eq!(prediction.report()["completed_move_to"], 1);
    let mut feedback = prediction.feedback().unwrap();
    let result = feedback["move_to_result"].clone();
    feedback["move_to_finished"] = json!([result]);
    for _ in 0..2 {
        successful(&vm.call(json!({"op":"physics_feedback","engine_poses":feedback["engine_poses"],"feedback":feedback})).unwrap()).unwrap();
    }
    // Feedback queues the callback; a control read doesn't advance other tasks.
    assert_eq!(run(&mut vm, "return finished")[0].as_f64(), Some(0.));
    successful(&vm.poll().unwrap()).unwrap();
    assert_eq!(run(&mut vm, "return finished")[0].as_f64(), Some(1.));
    successful(&vm.poll().unwrap()).unwrap();
    assert_eq!(run(&mut vm, "return finished")[0].as_f64(), Some(1.));
}

#[test]
fn actor_relative_move_to_forwards_once_and_nil_restores_world_target() {
    let (mut vm, world) = runtime_with_actors(1);
    poll_actors(&mut vm);
    run(&mut vm, "workspace.Worker0:SendMessage('follow')");
    poll_actors(&mut vm);
    let commands = vm.take_movement_intents();
    assert_eq!(commands.len(), 1, "{commands:?}");
    assert_eq!(commands[0]["part_id"], "1:9");
    assert_eq!(commands[0]["actor_origin"], "worker0");
    run(&mut vm, "assert(game:GetService('Players').LocalPlayer.Character.Humanoid.WalkToPart==workspace.Target.HumanoidRootPart)");
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &commands, 1.);
    assert!(advance(&mut prediction, &world, &[], 1.1).velocity[0] > 0.);
    run(&mut vm, "local h=game:GetService('Players').LocalPlayer.Character.Humanoid;h:MoveTo(Vector3.new(-20,2,0),nil);assert(h.WalkToPart==nil)");
    let commands = vm.take_movement_intents();
    assert_eq!(commands.len(), 1);
    assert!(commands[0].get("part_id").is_none());
    assert!(advance(&mut prediction, &world, &commands, 1.2).velocity[0] < 0.);
    poll_actors(&mut vm);
    assert!(vm.take_movement_intents().is_empty());
}

#[test]
fn walk_target_edits_cancel_old_requests_but_same_values_and_later_requests_survive() {
    let (mut vm, world) = runtime();
    run(&mut vm, "workspace.Gravity=0;h=game:GetService('Players').LocalPlayer.Character.Humanoid;h:MoveTo(Vector3.new(100,2,0));h.WalkToPoint=Vector3.new(100,2,0);h.WalkToPart=nil");
    let mut prediction = LocalPrediction::default();
    let commands = vm.take_movement_intents();
    assert_eq!(commands.len(), 2); // Gravity and MoveTo; no same-value cancellation.
    advance(&mut prediction, &world, &commands, 1.);
    assert!(advance(&mut prediction, &world, &[], 1.1).velocity[0] > 0.);
    run(&mut vm, "h.WalkToPoint=Vector3.new(-100,2,0)");
    let commands = vm.take_movement_intents();
    assert_eq!(commands.len(), 1);
    let frame = advance(&mut prediction, &world, &commands, 1.2);
    assert_eq!(frame.velocity[0], 0.);
    assert!(prediction.report()["move_to"].is_null());
    advance(&mut prediction, &world, &[], 10.);
    assert_eq!(prediction.report()["expired_move_to"], 0);
    assert!(prediction.feedback().unwrap().get("move_to_result").is_none());
    run(&mut vm, "h:MoveTo(Vector3.zero,workspace.Target.HumanoidRootPart);h.WalkToPart=nil;h:MoveTo(Vector3.new(-100,2,0))");
    let commands = vm.take_movement_intents();
    assert_eq!(commands.iter().map(|row| row["kind"].as_str().unwrap()).collect::<Vec<_>>(), vec!["MoveTo","CancelMoveTo","MoveTo"]);
    assert!(advance(&mut prediction, &world, &commands, 10.1).velocity[0] < 0.);
    assert!(prediction.report()["move_to"].is_object());
}

#[test]
fn destination_edits_do_not_start_walking_or_interrupt_manual_move_input() {
    let (mut vm, world) = runtime();
    run(&mut vm, "workspace.Gravity=0;h=game:GetService('Players').LocalPlayer.Character.Humanoid;h.WalkToPoint=Vector3.new(100,2,0)");
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    assert_eq!(advance(&mut prediction, &world, &[], 1.1).velocity[0], 0.);
    run(&mut vm, "h:Move(Vector3.new(0.5,0,0));h.WalkToPoint=Vector3.new(-100,2,0);h.WalkToPart=workspace.Target.HumanoidRootPart");
    let frame = advance(&mut prediction, &world, &vm.take_movement_intents(), 1.2);
    assert_eq!(frame.velocity[0], 8.);
    assert!(prediction.report()["move_to"].is_null());
}

#[test]
fn actor_destination_edit_cancels_once_and_root_can_restart_after_it() {
    let (mut vm, world) = runtime_with_actors(1);
    poll_actors(&mut vm);
    run(&mut vm, "h=game:GetService('Players').LocalPlayer.Character.Humanoid;h:MoveTo(Vector3.zero,workspace.Target.HumanoidRootPart)");
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    assert!(advance(&mut prediction, &world, &[], 1.1).velocity[0] > 0.);
    poll_actors(&mut vm);
    run(&mut vm, "workspace.Worker0:SendMessage('cancel-follow')");
    poll_actors(&mut vm);
    let commands = vm.take_movement_intents();
    assert_eq!(commands.len(), 1, "{commands:?}");
    assert_eq!(commands[0]["kind"], "CancelMoveTo");
    assert_eq!(commands[0]["actor_origin"], "worker0");
    assert_eq!(advance(&mut prediction, &world, &commands, 1.2).velocity[0], 0.);
    assert!(prediction.report()["move_to"].is_null());
    run(&mut vm, "assert(h.WalkToPart==nil);h:MoveTo(Vector3.new(-100,2,0))");
    assert!(advance(&mut prediction, &world, &vm.take_movement_intents(), 1.3).velocity[0] < 0.);
    poll_actors(&mut vm);
    assert!(vm.take_movement_intents().is_empty());
}

#[test]
fn character_pivots_cancel_only_earlier_move_to_requests() {
    let (mut vm, world) = runtime();
    run(&mut vm, "workspace.Gravity=0;c=game:GetService('Players').LocalPlayer.Character;h=c.Humanoid;h:MoveTo(Vector3.new(100,2,0));c:PivotTo(CFrame.new(20,2,0))");
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    let frame = advance(&mut prediction, &world, &[], 1.1);
    assert_eq!(frame.position[0], 20.);
    assert_eq!(frame.velocity[0], 0.);
    assert!(prediction.report()["move_to"].is_null());
    run(&mut vm, "c:PivotTo(CFrame.new(30,2,0));h:MoveTo(Vector3.new(-100,2,0))");
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.2);
    assert!(advance(&mut prediction, &world, &[], 1.3).velocity[0] < 0.);
    assert!(prediction.report()["move_to"].is_object());
    // A pivot that fails the existing received-root check must not cancel motion.
    let bad = json!({"sequence":100,"kind":"PivotTo","instance_id":"1:3","player_id":"1:2","root_id":"1:9","cframe":cf([50.,2.,0.])});
    assert!(advance(&mut prediction, &world, &[bad], 1.4).velocity[0] < 0.);
    assert!(prediction.report()["move_to"].is_object());
}

#[test]
fn root_cframe_write_reaches_prediction_and_feedback_and_cancels_move_to() {
    let (mut vm, world) = runtime();
    run(&mut vm, "workspace.Gravity=0;c=game:GetService('Players').LocalPlayer.Character;c.Humanoid:MoveTo(Vector3.new(100,2,0));expected=CFrame.new(20,12,5)*CFrame.Angles(0,math.pi/2,0);c.HumanoidRootPart.CFrame=expected");
    let commands = vm.take_movement_intents();
    assert_eq!(commands.iter().map(|r| r["kind"].as_str().unwrap()).collect::<Vec<_>>(), vec!["Gravity","MoveTo","PivotTo"]);
    let mut prediction = LocalPrediction::default();
    let frame = advance(&mut prediction, &world, &commands, 1.);
    assert_eq!(frame.position, [20.,12.,5.]);
    let expected_rotation = [0.,0.,1.,0.,1.,0.,-1.,0.,0.];
    assert!(frame.rotation.into_iter().zip(expected_rotation).all(|(a,b)| (a-b).abs()<1e-6));
    assert!(prediction.report()["move_to"].is_null());
    assert_eq!(advance(&mut prediction, &world, &[], 1.1).position, [20.,12.,5.]);
    let feedback = prediction.feedback().unwrap();
    successful(&vm.call(json!({"op":"physics_feedback","engine_poses":feedback["engine_poses"],"feedback":feedback})).unwrap()).unwrap();
    run(&mut vm, "assert((c.HumanoidRootPart.Position-Vector3.new(20,12,5)).Magnitude<1e-5);assert((c.HumanoidRootPart.CFrame.LookVector-expected.LookVector).Magnitude<1e-5)");
    assert_eq!(world.view().get_property("1:6", "CFrame").unwrap(), cf([0.,2.,0.]));
    assert_eq!(prediction.report()["accepted_pivots"], 1);
}

#[test]
fn root_part_pivot_uses_actual_frame_after_pivot_offset() {
    let (mut vm, world) = runtime();
    run(&mut vm, "workspace.Gravity=0;r=game:GetService('Players').LocalPlayer.Character.HumanoidRootPart;r.PivotOffset=CFrame.new(0,3,0);r:PivotTo(CFrame.new(20,12,5)*CFrame.Angles(0,math.pi/2,0));assert((r.Position-Vector3.new(20,9,5)).Magnitude<1e-5)");
    let commands = vm.take_movement_intents();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[1]["cframe"]["$cframe"]["position"], json!([20.,9.,5.]));
    let mut prediction = LocalPrediction::default();
    assert_eq!(advance(&mut prediction, &world, &commands, 1.).position, [20.,9.,5.]);
    let feedback = prediction.feedback().unwrap();
    successful(&vm.call(json!({"op":"physics_feedback","engine_poses":feedback["engine_poses"],"feedback":feedback})).unwrap()).unwrap();
    run(&mut vm, "assert((r:GetPivot().Position-Vector3.new(20,12,5)).Magnitude<1e-5)");
}

#[test]
fn actor_direct_root_transforms_forward_once_after_merging() {
    let (mut vm, world) = runtime_with_actors(1);
    poll_actors(&mut vm);
    let mut prediction = LocalPrediction::default();
    for (index, name) in ["root-frame", "root-pivot"].into_iter().enumerate() {
        let x = 20 + index*10;
        run(&mut vm, &format!("workspace.Worker0:SendMessage('{name}',{x})"));
        poll_actors(&mut vm);
        let commands = vm.take_movement_intents();
        assert_eq!(commands.len(), 1, "{commands:?}");
        assert_eq!(commands[0]["kind"], "PivotTo");
        assert_eq!(commands[0]["actor_origin"], "worker0");
        let frame = advance(&mut prediction, &world, &commands, 1. + index as f64*0.1);
        assert_eq!(frame.position, [x as f64,12.,5.]);
        poll_actors(&mut vm);
        assert!(vm.take_movement_intents().is_empty());
    }
    assert_eq!(prediction.report()["accepted_pivots"], 2);
}

#[test]
fn root_transform_commands_do_not_bypass_received_ownership_or_anchoring() {
    for (key, denied, allowed) in [("NetworkOwnerV3",json!(8),json!(7)), ("Anchored",json!(true),json!(false))] {
        let (mut vm, mut world) = runtime();
        run(&mut vm, "game:GetService('Players').LocalPlayer.Character.HumanoidRootPart.CFrame=CFrame.new(20,12,5)");
        let commands = vm.take_movement_intents();
        assert_eq!(commands.len(), 1);
        let mut fields = json!({}); fields[key] = denied;
        world.apply_delta(&json!({"version":1,"delta":true,"sequence":2,"actions":[{"kind":"set","sequence":2,"id":"1:6","fields":fields}]})).unwrap();
        let mut prediction = LocalPrediction::default();
        assert!(prediction.advance(&world.view(), &parents(), "1:2",42,7,true,&commands,1.).unwrap().is_none());
        let mut fields = json!({}); fields[key] = allowed;
        world.apply_delta(&json!({"version":1,"delta":true,"sequence":3,"actions":[{"kind":"set","sequence":3,"id":"1:6","fields":fields}]})).unwrap();
        assert_eq!(advance(&mut prediction, &world, &[], 1.1).position, [0.,2.,0.]);
        assert_eq!(prediction.report()["accepted_pivots"], 0);
    }
}

#[test]
fn bulk_root_move_forwards_one_final_pose_and_noop_does_not_repeat_it() {
    let (mut vm, world) = runtime();
    run(&mut vm, "workspace.Gravity=0;c=game:GetService('Players').LocalPlayer.Character;r=c.HumanoidRootPart;other=workspace.Target.HumanoidRootPart;c.Humanoid:MoveTo(Vector3.new(100,2,0));workspace:BulkMoveTo({r,other},{CFrame.new(20,12,5),CFrame.new(100,2,20)})");
    let commands = vm.take_movement_intents();
    assert_eq!(commands.iter().map(|r| r["kind"].as_str().unwrap()).collect::<Vec<_>>(), vec!["Gravity","MoveTo","PivotTo"]);
    let mut prediction = LocalPrediction::default();
    assert_eq!(advance(&mut prediction, &world, &commands, 1.).position, [20.,12.,5.]);
    assert!(prediction.report()["move_to"].is_null());
    run(&mut vm, "workspace:BulkMoveTo({r,other},{r.CFrame,other.CFrame})");
    assert!(vm.take_movement_intents().is_empty());
    let feedback = prediction.feedback().unwrap();
    successful(&vm.call(json!({"op":"physics_feedback","engine_poses":feedback["engine_poses"],"feedback":feedback})).unwrap()).unwrap();
    run(&mut vm, "assert(r.Position==Vector3.new(20,12,5));assert(other.Position==Vector3.new(100,2,20))");
    assert_eq!(world.view().get_property("1:6", "CFrame").unwrap(), cf([0.,2.,0.]));
    assert_eq!(prediction.report()["accepted_pivots"], 1);
}

#[test]
fn moving_a_welded_member_forwards_the_resolved_root_not_the_requested_endpoint() {
    let (mut vm, mut world) = runtime();
    let delta = json!({"version":1,"delta":true,"sequence":2,"actions":[
        {"kind":"set","sequence":2,"id":"1:10","fields":{"Anchored":false}}
    ]});
    world.apply_delta(&delta).unwrap();
    let view = SharedWorldView::projected(world.view(), context());
    successful(&vm.call_with_shared_world(json!({"op":"update","delta":delta,"timeout_ms":100}), &view).unwrap()).unwrap();
    run(&mut vm, "c=game:GetService('Players').LocalPlayer.Character;r=c.HumanoidRootPart;local w=Instance.new('WeldConstraint',c);w.Part0=r;w.Part1=c.Head;c.Head.CFrame=CFrame.new(10,5,0);assert(r.Position==Vector3.new(10,2,0))");
    let commands = vm.take_movement_intents();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["root_id"], "1:6");
    let mut prediction = LocalPrediction::default();
    assert_eq!(advance(&mut prediction, &world, &commands, 1.).position, [10.,2.,0.]);
    // The existing batch rule is last endpoint wins for a shared rigid component.
    run(&mut vm, "workspace:BulkMoveTo({r,c.Head},{CFrame.new(80,2,0),CFrame.new(30,5,0)},Enum.BulkMoveMode.FireCFrameChanged);assert(r.Position==Vector3.new(30,2,0))");
    let commands = vm.take_movement_intents();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["cframe"]["$cframe"]["position"], json!([30.,2.,0.]));
    assert_eq!(advance(&mut prediction, &world, &commands, 1.1).position, [30.,2.,0.]);
    assert_eq!(prediction.report()["accepted_pivots"], 2);
}

#[test]
fn actor_bulk_root_move_merges_and_forwards_once_without_replaying_the_command() {
    let (mut vm, world) = runtime_with_actors(1);
    poll_actors(&mut vm);
    run(&mut vm, "workspace.Worker0:SendMessage('bulk-root',25)");
    poll_actors(&mut vm);
    let commands = vm.take_movement_intents();
    assert_eq!(commands.len(), 1, "{commands:?}");
    assert_eq!(commands[0]["actor_origin"], "worker0");
    assert_eq!(commands[0]["kind"], "PivotTo");
    run(&mut vm, "assert(game:GetService('Players').LocalPlayer.Character.HumanoidRootPart.Position==Vector3.new(25,12,5));assert(workspace.Target.HumanoidRootPart.Position==Vector3.new(100,2,20))");
    let mut prediction = LocalPrediction::default();
    assert_eq!(advance(&mut prediction, &world, &commands, 1.).position, [25.,12.,5.]);
    poll_actors(&mut vm);
    assert!(vm.take_movement_intents().is_empty());
}

#[test]
fn move_observations_are_immediate_normalized_and_preserve_analog_speed() {
    let (mut vm, _) = runtime();
    run(&mut vm, r#"
        p=game:GetService('Players').LocalPlayer;h=p.Character.Humanoid
        h.WalkSpeed=32
        h:Move(Vector3.new(0.25,100,0))
        assert(h.MoveDirection==Vector3.new(1,0,0))
        assert(h:GetMoveVelocity()==Vector3.new(8,0,0))
        h.WalkSpeed=16;assert(h:GetMoveVelocity()==Vector3.new(4,0,0))
        assert(not pcall(function() h.MoveDirection=Vector3.zero end))
        local cam=Instance.new('Camera',workspace);workspace.CurrentCamera=cam
        cam.CFrame=CFrame.Angles(0,math.pi/2,0)
        p:Move(Vector3.new(0.5,0,0),true)
        assert((h.MoveDirection-Vector3.new(0,0,-1)).Magnitude<1e-5)
        assert((h:GetMoveVelocity()-Vector3.new(0,0,-8)).Magnitude<1e-5)
        cam.CFrame=CFrame.new()
        assert((h:GetMoveVelocity()-Vector3.new(0,0,-8)).Magnitude<1e-5)
        h:Move(Vector3.zero);assert(h.MoveDirection==Vector3.zero and h:GetMoveVelocity()==Vector3.zero)
    "#);
}

#[test]
fn physics_feedback_distinguishes_intended_movement_from_physical_velocity() {
    let (mut vm, world) = runtime();
    run(&mut vm, "workspace.Gravity=0;c=game:GetService('Players').LocalPlayer.Character;h=c.Humanoid;h:ChangeState(Enum.HumanoidStateType.Physics);c.HumanoidRootPart.AssemblyLinearVelocity=Vector3.new(0,0,9);h:Move(Vector3.new(0.25,0,0))");
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    let frame = advance(&mut prediction, &world, &[], 1.1);
    assert_eq!(frame.velocity, [0.,0.,9.]);
    let feedback = prediction.feedback().unwrap();
    successful(&vm.call(json!({"op":"physics_feedback","feedback":feedback})).unwrap()).unwrap();
    run(&mut vm, "assert(h.MoveDirection==Vector3.new(1,0,0));assert(h:GetMoveVelocity()==Vector3.new(4,0,0));assert(c.HumanoidRootPart.AssemblyLinearVelocity==Vector3.new(0,0,9))");
    // An older physics tick must not erase a newer script input.
    run(&mut vm, "h:Move(Vector3.new(-0.5,0,0))");
    successful(&vm.call(json!({"op":"physics_feedback","feedback":feedback})).unwrap()).unwrap();
    run(&mut vm, "assert(h.MoveDirection==Vector3.new(-1,0,0));assert(h:GetMoveVelocity()==Vector3.new(-8,0,0))");
}

#[test]
fn move_to_feedback_exposes_intent_and_cancellation_clears_it() {
    let (mut vm, world) = runtime();
    run(&mut vm, "workspace.Gravity=0;h=game:GetService('Players').LocalPlayer.Character.Humanoid;h:MoveTo(Vector3.new(100,2,0))");
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    let feedback = prediction.feedback().unwrap();
    successful(&vm.call(json!({"op":"physics_feedback","feedback":feedback})).unwrap()).unwrap();
    run(&mut vm, "assert(h.MoveDirection==Vector3.new(1,0,0));assert(h:GetMoveVelocity()==Vector3.new(16,0,0));h.WalkToPoint=Vector3.new(-100,2,0)");
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.1);
    let feedback = prediction.feedback().unwrap();
    successful(&vm.call(json!({"op":"physics_feedback","feedback":feedback})).unwrap()).unwrap();
    run(&mut vm, "assert(h.MoveDirection==Vector3.zero and h:GetMoveVelocity()==Vector3.zero)");
}

#[test]
fn actor_move_observations_follow_accepted_global_order_and_feedback() {
    let (mut vm, world) = runtime_with_actors(1);
    poll_actors(&mut vm);
    run(&mut vm, "workspace.Worker0:SendMessage('analog',0.5,0)");
    poll_actors(&mut vm);
    run(&mut vm, "h=game:GetService('Players').LocalPlayer.Character.Humanoid;assert(h.MoveDirection==Vector3.new(1,0,0));assert(h:GetMoveVelocity()==Vector3.new(8,0,0));h:Move(Vector3.new(-0.25,0,0))");
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    let feedback = prediction.feedback().unwrap();
    successful(&vm.call(json!({"op":"physics_feedback","feedback":feedback})).unwrap()).unwrap();
    poll_actors(&mut vm);
    run(&mut vm, "workspace.Worker0:SendMessage('input-read')");
    poll_actors(&mut vm);
    run(&mut vm, "assert(workspace.Worker0.Driver:GetAttribute('MoveX')==-1);assert(workspace.Worker0.Driver:GetAttribute('IntendedX')==-4);assert(h.MoveDirection==Vector3.new(-1,0,0))");
}

#[test]
fn root_transform_aliases_preserve_joint_offsets_through_prediction_feedback() {
    let (mut vm, mut world) = runtime();
    let delta = json!({"version":1,"delta":true,"sequence":3,"actions":[
        {"kind":"set","sequence":2,"id":"1:6","fields":{"Massless":false,"RootPriority":10}},
        {"kind":"set","sequence":3,"id":"1:10","fields":{"Anchored":false,"Massless":false,"RootPriority":0}}
    ]});
    world.apply_delta(&delta).unwrap();
    let view = SharedWorldView::projected(world.view(), context());
    successful(&vm.call_with_shared_world(json!({"op":"update","delta":delta,"timeout_ms":100}), &view).unwrap()).unwrap();
    run(&mut vm, r#"
        workspace.Gravity=0;c=game:GetService('Players').LocalPlayer.Character;r=c.HumanoidRootPart
        joint=Instance.new('Motor6D',c);joint.C0=CFrame.new(0,3,0);joint.Part0=r;joint.Part1=c.Head
        headBefore=c.Head.CFrame
    "#);
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    for (index, (key, value, expected)) in [
        ("Position","Vector3.new(10,2,0)","CFrame.new(10,2,0)"),
        ("Orientation","Vector3.new(10,30,20)","CFrame.new(10,2,0)*CFrame.fromEulerAnglesYXZ(math.rad(10),math.rad(30),math.rad(20))"),
        ("Rotation","Vector3.new(-20,10,40)","CFrame.new(10,2,0)*CFrame.Angles(math.rad(-20),math.rad(10),math.rad(40))")
    ].into_iter().enumerate() {
        run(&mut vm, &format!(r#"
            r.{key}={value};expected={expected}
            assert((r.Position-expected.Position).Magnitude<1e-4)
            assert((r.CFrame.LookVector-expected.LookVector).Magnitude<1e-4)
            assert(c.Head.CFrame==headBefore)
            assert(((r.CFrame*joint.C0).Position-(c.Head.CFrame*joint.C1).Position).Magnitude<1e-4)
        "#));
        let commands = vm.take_movement_intents();
        assert_eq!(commands.len(), 1, "{key}: {commands:?}");
        assert_eq!(commands[0]["kind"], "PivotTo");
        let frame = advance(&mut prediction, &world, &commands, 1.1+index as f64*0.1);
        assert_eq!(frame.position, [10.,2.,0.]);
        for (actual, expected) in frame.rotation.into_iter().zip(commands[0]["cframe"]["$cframe"]["rotation"].as_array().unwrap()) {
            assert!((actual-expected.as_f64().unwrap()).abs()<1e-6);
        }
        let feedback = prediction.feedback().unwrap();
        successful(&vm.call(json!({"op":"physics_feedback","engine_poses":feedback["engine_poses"],"feedback":feedback})).unwrap()).unwrap();
        run(&mut vm, "assert((r.CFrame.LookVector-expected.LookVector).Magnitude<1e-4);assert(c.Head.CFrame==headBefore);assert(((r.CFrame*joint.C0).Position-(c.Head.CFrame*joint.C1).Position).Magnitude<1e-4)");
    }
    assert_eq!(prediction.report()["accepted_pivots"], 3);
    assert_eq!(world.view().get_property("1:6", "CFrame").unwrap(), cf([0.,2.,0.]));
}

#[test]
fn actor_root_transform_aliases_forward_once_in_order() {
    let (mut vm, world) = runtime_with_actors(1);
    poll_actors(&mut vm);
    let mut prediction = LocalPrediction::default();
    for (index, (key, vector)) in [
        ("Position",[20,12,5]),("Orientation",[0,90,0]),("Rotation",[0,0,45])
    ].into_iter().enumerate() {
        run(&mut vm, &format!("workspace.Worker0:SendMessage('root-alias','{key}',{},{},{})",vector[0],vector[1],vector[2]));
        poll_actors(&mut vm);
        let commands = vm.take_movement_intents();
        assert_eq!(commands.len(), 1, "{key}: {commands:?}");
        assert_eq!(commands[0]["actor_origin"], "worker0");
        assert_eq!(advance(&mut prediction, &world, &commands, 1.+index as f64*0.1).position, [20.,12.,5.]);
        poll_actors(&mut vm);
        assert!(vm.take_movement_intents().is_empty());
    }
    assert_eq!(prediction.report()["accepted_pivots"], 3);
}

#[test]
fn move_to_tracks_local_target_edits_without_restarting_its_deadline() {
    let (mut vm, mut world) = runtime();
    run(&mut vm, "workspace.Gravity=0;h=game:GetService('Players').LocalPlayer.Character.Humanoid;t=workspace.Target.HumanoidRootPart;h:MoveTo(Vector3.new(4,0,0),t)");
    let mut prediction=LocalPrediction::default();
    advance(&mut prediction,&world,&vm.take_movement_intents(),1.);
    run(&mut vm,"t.CFrame=CFrame.new(-100,2,0)*CFrame.Angles(0,math.pi/2,0)");
    let commands=vm.take_movement_intents();
    assert_eq!(commands.len(),1,"{commands:?}");
    assert_eq!(commands[0]["kind"],"MoveToTarget");
    assert!(advance(&mut prediction,&world,&commands,1.1).velocity[0]<0.);
    assert_eq!(prediction.report()["move_to"]["started_at"],1.);
    run(&mut vm,"assert(t.Position.X==-100)");
    assert!(vm.take_movement_intents().is_empty());
    run(&mut vm,"t.Parent=nil");
    assert_eq!(advance(&mut prediction,&world,&vm.take_movement_intents(),1.2).velocity,[0.;3]);
    let delta=json!({"version":1,"delta":true,"sequence":2,"actions":[
        {"kind":"set","sequence":2,"id":"1:9","fields":{"CFrame":cf([10.,2.,20.])}}
    ]});
    world.apply_delta(&delta).unwrap();
    assert_eq!(advance(&mut prediction,&world,&[],1.3).velocity,[0.;3]);
    let view=SharedWorldView::projected(world.view(),context());
    successful(&vm.call_with_shared_world(json!({"op":"update","delta":delta,"timeout_ms":100}),&view).unwrap()).unwrap();
    assert_eq!(advance(&mut prediction,&world,&vm.take_movement_intents(),1.4).velocity,[0.;3]);
    run(&mut vm,"t.Parent=workspace.Target;t.Position=Vector3.new(100,2,0)");
    assert!(advance(&mut prediction,&world,&vm.take_movement_intents(),8.9).velocity[0]>0.);
    advance(&mut prediction,&world,&[],9.);
    assert_eq!(prediction.report()["expired_move_to"],1);
    assert_eq!(prediction.feedback().unwrap()["move_to_result"]["reached"],false);
    assert_eq!(world.view().get_property("1:9","CFrame").unwrap(),cf([10.,2.,20.]));
}

#[test]
fn local_only_move_to_targets_rotate_disappear_and_complete() {
    let (mut vm, world) = runtime();
    run(&mut vm,"workspace.Gravity=0;h=game:GetService('Players').LocalPlayer.Character.Humanoid;t=Instance.new('Part',workspace);t.Anchored=true;t.CFrame=CFrame.new(100,2,0);h:MoveTo(Vector3.new(10,0,0),t)");
    let commands=vm.take_movement_intents();
    assert_eq!(commands.iter().map(|c|c["kind"].as_str().unwrap()).collect::<Vec<_>>(),vec!["Gravity","MoveTo","MoveToTarget"]);
    assert_eq!(commands[2]["received"],false);
    let mut prediction=LocalPrediction::default();
    advance(&mut prediction,&world,&commands,1.);
    run(&mut vm,"t.CFrame=CFrame.new(0,2,0)*CFrame.Angles(0,math.pi/2,0)");
    let frame=advance(&mut prediction,&world,&vm.take_movement_intents(),1.1);
    assert!(frame.velocity[2]<0. && frame.velocity[0].abs()<1e-4);
    run(&mut vm,"t.Parent=nil");
    assert_eq!(advance(&mut prediction,&world,&vm.take_movement_intents(),1.2).velocity,[0.;3]);
    run(&mut vm,"t.Parent=workspace;t.CFrame=CFrame.new(0,2,-1.6);h:MoveTo(Vector3.zero,t)");
    advance(&mut prediction,&world,&vm.take_movement_intents(),1.3);
    assert_eq!(prediction.report()["completed_move_to"],1);
    run(&mut vm,"h:MoveTo(Vector3.new(100,0,0),t);t:Destroy()");
    assert_eq!(advance(&mut prediction,&world,&vm.take_movement_intents(),1.4).velocity,[0.;3]);
}

#[test]
fn newer_received_target_pose_supersedes_queued_and_active_local_observations() {
    let (mut vm, mut world) = runtime();
    run(&mut vm,"workspace.Gravity=0;h=game:GetService('Players').LocalPlayer.Character.Humanoid;t=workspace.Target.HumanoidRootPart;t.Position=Vector3.new(-100,2,0);h:MoveTo(Vector3.zero,t)");
    let mut prediction=LocalPrediction::default();
    advance(&mut prediction,&world,&vm.take_movement_intents(),1.);
    assert!(advance(&mut prediction,&world,&[],1.1).velocity[0]<0.);
    run(&mut vm,"t.Position=Vector3.new(-200,2,0)");
    let stale=vm.take_movement_intents();
    let delta=json!({"version":1,"delta":true,"sequence":2,"actions":[
        {"kind":"set","sequence":2,"id":"1:9","fields":{"CFrame":cf([10.,2.,20.])}}
    ]});
    world.apply_delta(&delta).unwrap();
    assert!(advance(&mut prediction,&world,&[],1.2).velocity[0]>0.);
    assert!(advance(&mut prediction,&world,&stale,1.3).velocity[0]>0.);
    let view=SharedWorldView::projected(world.view(),context());
    successful(&vm.call_with_shared_world(json!({"op":"update","delta":delta,"timeout_ms":100}),&view).unwrap()).unwrap();
    let commands=vm.take_movement_intents();
    assert_eq!(commands.len(),1,"{commands:?}");
    assert_eq!(commands[0]["use_received"],true);
    assert!(advance(&mut prediction,&world,&commands,1.4).velocity[0]>0.);
    run(&mut vm,"t.Position=Vector3.new(-200,2,0)");
    assert!(advance(&mut prediction,&world,&vm.take_movement_intents(),1.5).velocity[0]<0.);
    let delta=json!({"version":1,"delta":true,"sequence":3,"actions":[
        {"kind":"set","sequence":3,"id":"1:9","fields":{"CFrame":cf([10.,2.,20.])}}
    ]});
    world.apply_delta(&delta).unwrap();
    let view=SharedWorldView::projected(world.view(),context());
    // Scripts can intentionally override the new receipt with the old local pose.
    successful(&vm.call_with_shared_world(json!({"op":"update","delta":delta,"timeout_ms":100}),&view).unwrap()).unwrap();
    run(&mut vm,"t.Position=Vector3.new(-200,2,0)");
    assert!(advance(&mut prediction,&world,&vm.take_movement_intents(),1.6).velocity[0]<0.);
}

#[test]
fn actor_target_edits_are_sampled_after_merge_and_cannot_revive_canceled_move_to() {
    let (mut vm, world)=runtime_with_actors(1);
    poll_actors(&mut vm);
    run(&mut vm,"workspace.Worker0:SendMessage('follow')");
    poll_actors(&mut vm);
    let mut prediction=LocalPrediction::default();
    advance(&mut prediction,&world,&vm.take_movement_intents(),1.);
    run(&mut vm,"workspace.Worker0:SendMessage('target-pose',-100)");
    poll_actors(&mut vm);
    let commands=vm.take_movement_intents();
    assert_eq!(commands.len(),1,"{commands:?}");
    assert_eq!(commands[0]["kind"],"MoveToTarget");
    assert!(advance(&mut prediction,&world,&commands,1.1).velocity[0]<0.);
    poll_actors(&mut vm);
    assert!(vm.take_movement_intents().is_empty());
    run(&mut vm,"game:GetService('Players').LocalPlayer.Character.Humanoid:Move(Vector3.new(0,0,1))");
    advance(&mut prediction,&world,&vm.take_movement_intents(),1.2);
    let mut late=commands[0].clone();
    late["sequence"]=json!(prediction.report()["movement_cursor"].as_u64().unwrap()+1);
    assert_eq!(advance(&mut prediction,&world,&[late],1.3).velocity[2],16.);
    assert!(prediction.report()["move_to"].is_null());
    run(&mut vm,"workspace.Worker0:SendMessage('target-pose',200)");
    poll_actors(&mut vm);
    assert!(vm.take_movement_intents().is_empty());
}

#[test]
fn gravity_set_before_character_arrival_survives_respawn_and_ownership_loss() {
    let (mut vm, mut world) = runtime();
    run(&mut vm, "workspace.Gravity=0");
    let commands = vm.take_movement_intents();
    world.apply_delta(&json!({"version":1,"delta":true,"sequence":2,"actions":[
        {"kind":"set","sequence":2,"id":"1:2","fields":{"Character":null}}
    ]})).unwrap();
    let mut prediction = LocalPrediction::default();
    assert!(prediction.advance(&world.view(), &parents(), "1:2",42,7,false,&commands,1.).unwrap().is_none());
    assert_eq!(prediction.report()["accepted_gravity_writes"], 1);
    world.apply_delta(&json!({"version":1,"delta":true,"sequence":3,"actions":[
        {"kind":"set","sequence":3,"id":"1:2","fields":{"Character":{"$ref":"1:3"}}}
    ]})).unwrap();
    advance(&mut prediction, &world, &[], 1.1);
    assert_eq!(prediction.report()["gravity"], 0.);
    world.apply_delta(&json!({"version":1,"delta":true,"sequence":6,"actions":[
        {"kind":"set","sequence":4,"id":"1:2","fields":{"Character":{"$ref":"1:8"}}},
        {"kind":"set","sequence":5,"id":"1:5","fields":{"Parent":{"$ref":"1:8"},"RootPart":{"$ref":"1:9"}}},
        {"kind":"set","sequence":6,"id":"1:9","fields":{"NetworkOwnerV3":7,"AssemblyRootPart":{"$ref":"1:9"}}}
    ]})).unwrap();
    advance(&mut prediction, &world, &[], 1.2);
    assert_eq!(prediction.report()["gravity"], 0.);
    world.apply_delta(&json!({"version":1,"delta":true,"sequence":7,"actions":[
        {"kind":"set","sequence":7,"id":"1:9","fields":{"NetworkOwnerV3":8}}
    ]})).unwrap();
    assert!(prediction.advance(&world.view(), &parents(), "1:2",42,7,true,&[],1.3).unwrap().is_none());
    world.apply_delta(&json!({"version":1,"delta":true,"sequence":8,"actions":[
        {"kind":"set","sequence":8,"id":"1:9","fields":{"NetworkOwnerV3":7}}
    ]})).unwrap();
    advance(&mut prediction, &world, &[], 1.4);
    assert_eq!(prediction.report()["gravity"], 0.);
    assert!(!advance(&mut prediction, &world, &[], 1.5).server_acceptance_confirmed);
}

#[test]
fn fresh_gravity_receipt_overrides_local_and_older_queued_settings() {
    let (mut vm, mut world) = runtime();
    let mut prediction = LocalPrediction::default();
    run(&mut vm, "workspace.Gravity=0");
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    run(&mut vm, "workspace.Gravity=4");
    let older = vm.take_movement_intents();
    let delta = json!({"version":1,"delta":true,"sequence":2,"actions":[
        {"kind":"set","sequence":2,"id":"1:4","fields":{"Gravity":10}}
    ]});
    world.apply_delta(&delta).unwrap();
    successful(&vm.call(json!({"op":"update","delta":delta,"timeout_ms":100})).unwrap()).unwrap();
    advance(&mut prediction, &world, &older, 1.1);
    assert_eq!(prediction.report()["gravity"], 10.);
    assert_eq!(prediction.report()["rejected_intents"], 1);
    assert_eq!(run(&mut vm, "return workspace.Gravity"), json!([10.]));
    run(&mut vm, "workspace.Gravity=3");
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.2);
    assert_eq!(prediction.report()["gravity"], 3.);
    world.apply_delta(&json!({"version":1,"delta":true,"sequence":3,"actions":[
        {"kind":"set","sequence":3,"id":"1:9","fields":{"Transparency":0.5}}
    ]})).unwrap();
    advance(&mut prediction, &world, &[], 1.3);
    assert_eq!(prediction.report()["gravity"], 3.);
}

#[test]
fn actor_gravity_and_jump_commands_preserve_order_without_replay_duplicates() {
    let (mut vm, mut world) = runtime_with_actors(1);
    poll_actors(&mut vm);
    let delta = json!({"version":1,"delta":true,"sequence":2,"actions":[
        {"kind":"create","sequence":2,"node":{"id":"1:20","class_name":"Part","name":"Floor","parent":"1:4",
            "received_creation_complete":true,"properties":{"CFrame":cf([0.,-0.5,0.]),"Size":{"$vector3":[100,1,100]},"Anchored":true,"CanCollide":true}}}
    ]});
    world.apply_delta(&delta).unwrap();
    successful(&vm.call(json!({"op":"update","delta":delta,"timeout_ms":100})).unwrap()).unwrap();
    poll_actors(&mut vm);
    run(&mut vm, "workspace.Worker0:SendMessage('gravity-jump')");
    poll_actors(&mut vm);
    let commands = vm.take_movement_intents();
    assert_eq!(commands.iter().map(|c| c["kind"].as_str().unwrap()).collect::<Vec<_>>(),
        vec!["Gravity","Jump","Gravity"]);
    let mut prediction = LocalPrediction::default();
    let frame = advance(&mut prediction, &world, &commands, 1.);
    assert_eq!(frame.velocity[1], 10.);
    let frame = advance(&mut prediction, &world, &[], 1.1);
    assert!((frame.position[1] - 3.).abs() < 1e-6);
    assert_eq!(frame.velocity[1], 10.);
    assert_eq!(prediction.report()["gravity"], 0.);
    assert_eq!(prediction.report()["accepted_gravity_writes"], 2);
    assert_eq!(prediction.report()["accepted_jumps"], 1);
    assert!(vm.take_movement_intents().is_empty());
}

#[test]
fn partial_luau_move_inputs_scale_speed_and_diagonals_stay_capped() {
    let (mut vm, world) = runtime();
    let mut prediction = LocalPrediction::default();
    run(&mut vm, "p=game:GetService('Players').LocalPlayer;h=p.Character.Humanoid;h.WalkSpeed=32;h:Move(Vector3.new(0.25,700,0))");
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    let frame = advance(&mut prediction, &world, &[], 1.1);
    assert_eq!(frame.velocity[0], 8.);
    assert!((frame.position[0] - 0.8).abs() < 1e-6);
    assert_eq!(prediction.feedback().unwrap()["running_speed"], 8.);
    run(&mut vm, "p:Move(Vector3.new(-0.25,0,0))");
    assert_eq!(advance(&mut prediction, &world, &vm.take_movement_intents(), 1.2).velocity[0], -8.);
    run(&mut vm, "h:Move(Vector3.new(3,0,4))");
    let frame = advance(&mut prediction, &world, &vm.take_movement_intents(), 1.3);
    assert!((frame.velocity[0].hypot(frame.velocity[2]) - 32.).abs() < 1e-9);
    run(&mut vm, "h:Move(Vector3.new(0,900,0))");
    let frame = advance(&mut prediction, &world, &vm.take_movement_intents(), 1.4);
    assert_eq!(frame.velocity[0], 0.);
    assert_eq!(frame.velocity[2], 0.);
}

#[test]
fn partial_camera_relative_move_keeps_captured_heading_without_pitch_slowdown() {
    let (mut vm, world) = runtime();
    run(&mut vm, r#"
        local camera=Instance.new('Camera')
        camera.Parent=workspace;workspace.CurrentCamera=camera
        camera.CFrame=CFrame.Angles(0,math.pi/2,0)*CFrame.Angles(math.pi/3,0,0)
        game:GetService('Players').LocalPlayer.Character.Humanoid:Move(Vector3.new(0,0,-0.25),true)
        camera.CFrame=CFrame.new()
    "#);
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    let frame = advance(&mut prediction, &world, &[], 1.1);
    assert!((frame.velocity[0] + 4.).abs() < 1e-5, "{frame:?}");
    assert!(frame.velocity[2].abs() < 1e-5, "{frame:?}");
    assert!((frame.position[0] + 0.4).abs() < 1e-5);
}

#[test]
fn actor_partial_move_replaces_root_input_without_becoming_full_speed() {
    let (mut vm, world) = runtime_with_actors(1);
    poll_actors(&mut vm);
    run(&mut vm, "game:GetService('Players').LocalPlayer.Character.Humanoid:Move(Vector3.new(-0.5,0,0));workspace.Worker0:SendMessage('analog',0.25,0)");
    poll_actors(&mut vm);
    let commands = vm.take_movement_intents();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[1]["actor_origin"], "worker0");
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &commands, 1.);
    assert_eq!(advance(&mut prediction, &world, &[], 1.1).velocity[0], 4.);
    assert!(vm.take_movement_intents().is_empty());
}

#[test]
fn script_walk_speed_drives_movement_and_zero_stops_the_controller() {
    let (mut vm, world) = runtime();
    run(&mut vm, "h=game:GetService('Players').LocalPlayer.Character.Humanoid;h.WalkSpeed=32;h:Move(Vector3.new(1,0,0))");
    let mut prediction = LocalPrediction::default();
    let commands = vm.take_movement_intents();
    assert_eq!(commands[0]["kind"], "WalkSpeed");
    assert_eq!(commands[0]["received_sequence"], 1);
    advance(&mut prediction, &world, &commands, 1.);
    let frame = advance(&mut prediction, &world, &[], 1.1);
    assert_eq!(frame.velocity[0], 32.);
    assert!((frame.position[0] - 3.2).abs() < 1e-6);
    assert_eq!(world.view().get_property("1:5", "WalkSpeed"), Some(json!(16)));
    assert_eq!(prediction.feedback().unwrap()["running_speed"], 32.);
    run(&mut vm, "h.WalkSpeed=0");
    assert_eq!(advance(&mut prediction, &world, &vm.take_movement_intents(), 1.2).velocity[0], 0.);
    assert_eq!(prediction.report()["accepted_walk_speed_writes"], 2);
}

#[test]
fn actor_walk_speed_writes_reach_prediction_once_and_survive_empty_ticks() {
    let (mut vm, world) = runtime_with_actors(1);
    poll_actors(&mut vm);
    run(&mut vm, "workspace.Worker0:SendMessage('speed',24)");
    poll_actors(&mut vm);
    let commands = vm.take_movement_intents();
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0]["kind"], "WalkSpeed");
    assert_eq!(commands[0]["actor_origin"], "worker0");
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &commands, 1.);
    assert_eq!(advance(&mut prediction, &world, &[], 1.1).velocity[0], 24.);
    assert_eq!(advance(&mut prediction, &world, &[], 1.2).velocity[0], 24.);
    assert_eq!(prediction.report()["accepted_walk_speed_writes"], 1);
    assert!(vm.take_movement_intents().is_empty());
}

#[test]
fn received_walk_speed_replaces_local_speed_and_rejects_an_older_queued_write() {
    let (mut vm, mut world) = runtime();
    let mut prediction = LocalPrediction::default();
    run(&mut vm, "h=game:GetService('Players').LocalPlayer.Character.Humanoid;h.WalkSpeed=32;h:Move(Vector3.new(1,0,0))");
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    run(&mut vm, "h.WalkSpeed=48");
    let delayed = vm.take_movement_intents();
    let delta = json!({"version":1,"delta":true,"sequence":2,"actions":[
        {"kind":"set","sequence":2,"id":"1:5","fields":{"WalkSpeed":16}}
    ]});
    world.apply_delta(&delta).unwrap();
    prediction.observe_replication(&receipt(delta.clone()));
    successful(&vm.call(json!({"op":"update","delta":delta,"timeout_ms":100})).unwrap()).unwrap();
    assert_eq!(advance(&mut prediction, &world, &delayed, 1.1).velocity[0], 16.);
    assert_eq!(prediction.report()["rejected_intents"], 1);
    assert_eq!(run(&mut vm, "return h.WalkSpeed"), json!([16.]));
    run(&mut vm, "h.WalkSpeed=20");
    assert_eq!(advance(&mut prediction, &world, &vm.take_movement_intents(), 1.2).velocity[0], 20.);
    let unrelated = json!({"version":1,"delta":true,"sequence":3,"actions":[
        {"kind":"set","sequence":3,"id":"1:9","fields":{"Transparency":0.5}}
    ]});
    world.apply_delta(&unrelated).unwrap();
    assert_eq!(advance(&mut prediction, &world, &[], 1.3).velocity[0], 20.);
    assert_eq!(prediction.report()["accepted_walk_speed_writes"], 2);
}

#[test]
fn walk_speed_override_is_discarded_on_character_replacement_and_ownership_loss() {
    let (mut vm, mut world) = runtime();
    let mut prediction = LocalPrediction::default();
    run(&mut vm, "h=game:GetService('Players').LocalPlayer.Character.Humanoid;h.WalkSpeed=32;h:Move(Vector3.new(1,0,0))");
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    let delta = json!({"version":1,"delta":true,"sequence":4,"actions":[
        {"kind":"set","sequence":2,"id":"1:2","fields":{"Character":{"$ref":"1:8"}}},
        {"kind":"set","sequence":3,"id":"1:5","fields":{"Parent":{"$ref":"1:8"},"RootPart":{"$ref":"1:9"}}},
        {"kind":"set","sequence":4,"id":"1:9","fields":{"NetworkOwnerV3":7,"AssemblyRootPart":{"$ref":"1:9"}}}
    ]});
    world.apply_delta(&delta).unwrap();
    advance(&mut prediction, &world, &[], 1.1);
    assert_eq!(prediction.report()["walk_speed"], 16.);
    let command = json!({"sequence":3,"kind":"WalkSpeed","instance_id":"1:5","player_id":"1:2","character_id":"1:8","value":28,"received_sequence":4});
    advance(&mut prediction, &world, &[command], 1.2);
    assert_eq!(prediction.report()["walk_speed"], 28.);
    world.apply_delta(&json!({"version":1,"delta":true,"sequence":5,"actions":[
        {"kind":"set","sequence":5,"id":"1:9","fields":{"NetworkOwnerV3":8}}
    ]})).unwrap();
    assert!(prediction.advance(&world.view(), &parents(), "1:2",42,7,true,&[],1.3).unwrap().is_none());
    world.apply_delta(&json!({"version":1,"delta":true,"sequence":6,"actions":[
        {"kind":"set","sequence":6,"id":"1:9","fields":{"NetworkOwnerV3":7}}
    ]})).unwrap();
    advance(&mut prediction, &world, &[], 1.4);
    assert_eq!(prediction.report()["walk_speed"], 16.);
}

#[test]
fn queued_script_bursts_reach_prediction_in_bounded_ordered_batches() {
    let (mut vm, world) = runtime();
    for (start, end) in [(1, 200), (201, 400)] {
        run(&mut vm, &format!(
            "local h=game:GetService('Players').LocalPlayer.Character.Humanoid; \
             for i={start},{end} do h:Move(Vector3.new(if i<=256 then 1 else -1,0,0)) end"
        ));
    }
    let first = vm.take_movement_intents();
    let second = vm.take_movement_intents();
    assert_eq!(first.len(), 256);
    assert_eq!(second.len(), 144);
    for (index, intent) in first.iter().chain(&second).enumerate() {
        assert_eq!(intent["sequence"], index + 1);
    }
    assert!(vm.take_movement_intents().is_empty());
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &first, 1.);
    assert_eq!(advance(&mut prediction, &world, &[], 1.1).velocity[0], 16.);
    assert_eq!(advance(&mut prediction, &world, &second, 1.2).velocity[0], -16.);
    assert!(!advance(&mut prediction, &world, &[], 1.3).server_acceptance_confirmed);
}

#[test]
fn script_physics_state_and_velocity_drive_prediction_and_luau_feedback() {
    let (mut vm, world) = runtime();
    run(&mut vm,"local c=game:GetService('Players').LocalPlayer.Character;c.Humanoid:ChangeState(Enum.HumanoidStateType.Physics);c.HumanoidRootPart.AssemblyLinearVelocity=Vector3.new(6,10,0)");
    let intents = vm.take_movement_intents();
    assert_eq!(intents.len(), 2);
    assert_eq!(intents[1]["kind"], "AssemblyLinearVelocity");
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &intents, 1.);
    let mut frame = advance(&mut prediction, &world, &[], 1.1);
    assert!((frame.position[0] - 0.6).abs() < 1e-6);
    assert!(frame.position[1] > 2.8);
    for i in 2..=10 {
        frame = advance(&mut prediction, &world, &[], 1. + i as f64 / 10.);
    }
    assert!((frame.position[0] - 6.).abs() < 1e-6);
    assert!(frame.velocity[1].abs() < 1e-6);
    assert_eq!(prediction.feedback().unwrap()["state"]["name"], "Physics");
    let feedback = prediction.feedback().unwrap();
    let reply=vm.call(json!({"op":"physics_feedback","engine_poses":feedback["engine_poses"],"feedback":feedback})).unwrap();
    successful(&reply).unwrap();
    let values=run(&mut vm,"local c=game:GetService('Players').LocalPlayer.Character;return c.Humanoid:GetState().Name,c.HumanoidRootPart.Position.X,c.HumanoidRootPart.AssemblyLinearVelocity.X");
    assert_eq!(values[0], "Physics");
    assert!((values[1].as_f64().unwrap() - 6.).abs() < 1e-5);
    assert_eq!(values[2], 6.);
    run(&mut vm,"local h=game:GetService('Players').LocalPlayer.Character.Humanoid;h:Move(Vector3.new(1,0,0));h:ChangeState(Enum.HumanoidStateType.Running)");
    frame = advance(&mut prediction, &world, &vm.take_movement_intents(), 2.1);
    assert_eq!(frame.velocity[0], 16.);
    assert_eq!(prediction.report()["physics_state"], false);
    assert!(!frame.server_acceptance_confirmed);
}
#[test]
fn velocity_write_survives_empty_scheduler_ticks_before_the_first_physics_step() {
    let (mut vm, world) = runtime();
    let mut prediction = LocalPrediction::default();
    run(&mut vm,"game:GetService('Players').LocalPlayer.Character.HumanoidRootPart.AssemblyLinearVelocity=Vector3.new(6,0,0)");
    let initial = advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    assert_eq!(initial.velocity[0], 6.);
    for i in 1..=3 {
        let frame = advance(&mut prediction, &world, &[], 1. + i as f64 * 0.0001);
        assert_eq!(frame.velocity[0], 6.);
    }
    let first = advance(&mut prediction, &world, &[], 1.1);
    assert!((first.position[0] - 0.6).abs() < 0.001);
    let second = advance(&mut prediction, &world, &[], 1.2);
    assert_eq!(second.velocity[0], 0.);
}
#[test]
fn velocity_intents_require_current_root_player_and_received_ownership() {
    let (mut vm, mut world) = runtime();
    run(
        &mut vm,
        "workspace.Target.HumanoidRootPart.AssemblyLinearVelocity=Vector3.new(99,0,0)",
    );
    assert!(vm.take_movement_intents().is_empty());
    let mut prediction = LocalPrediction::default();
    let invalid = json!({"sequence":1,"kind":"AssemblyLinearVelocity","instance_id":"1:9","root_id":"1:9","character_id":"1:3","player_id":"1:2","vector":[99,0,0]});
    assert_eq!(
        advance(&mut prediction, &world, &[invalid], 1.).velocity[0],
        0.
    );
    assert_eq!(prediction.report()["rejected_intents"], 1);
    world.apply_delta(&json!({"version":1,"delta":true,"sequence":2,"actions":[{"kind":"set","sequence":2,"id":"1:6","fields":{"NetworkOwnerV3":8}}]})).unwrap();
    let command = json!({"sequence":2,"kind":"AssemblyLinearVelocity","instance_id":"1:6","root_id":"1:6","character_id":"1:3","player_id":"1:2","vector":[99,0,0]});
    assert!(prediction
        .advance(
            &world.view(),
            &parents(),
            "1:2",
            42,
            7,
            true,
            &[command],
            1.1
        )
        .unwrap()
        .is_none());
    world.apply_delta(&json!({"version":1,"delta":true,"sequence":3,"actions":[{"kind":"set","sequence":3,"id":"1:6","fields":{"NetworkOwnerV3":7}}]})).unwrap();
    assert_eq!(advance(&mut prediction, &world, &[], 1.2).velocity[0], 0.);
    assert_eq!(prediction.report()["accepted_velocity_writes"], 0);
}
#[test]
fn received_state_receipt_overrides_manual_physics_even_when_values_repeat() {
    let (mut vm, mut world) = runtime();
    let mut prediction = LocalPrediction::default();
    run(&mut vm,"local c=game:GetService('Players').LocalPlayer.Character;c.Humanoid:ChangeState(Enum.HumanoidStateType.Physics);c.HumanoidRootPart.AssemblyLinearVelocity=Vector3.new(6,0,0)");
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    advance(&mut prediction, &world, &[], 1.1);
    let delta = json!({"version":1,"delta":true,"sequence":2,"actions":[{"kind":"set","sequence":2,"id":"1:5","fields":{"HumanoidStateType":8}}]});
    world.apply_delta(&delta).unwrap();
    prediction.observe_replication(&receipt(delta));
    assert_eq!(advance(&mut prediction, &world, &[], 1.2).velocity[0], 0.);
    assert_eq!(prediction.report()["physics_state"], false);
}
#[test]
fn disabled_physics_state_and_invalid_vectors_do_not_create_motion() {
    let (mut vm, world) = runtime();
    let mut prediction = LocalPrediction::default();
    let values = run(
        &mut vm,
        r#"local c=game:GetService('Players').LocalPlayer.Character
        c.Humanoid:SetStateEnabled(Enum.HumanoidStateType.Physics,false)
        c.Humanoid:ChangeState(Enum.HumanoidStateType.Physics)
        local a=pcall(function() c.HumanoidRootPart.AssemblyLinearVelocity=Vector3.new(math.huge,0,0) end)
        local b=pcall(function() c.HumanoidRootPart.AssemblyLinearVelocity={} end)
        return a,b,c.HumanoidRootPart.AssemblyLinearVelocity.X"#,
    );
    assert_eq!(values, json!([false, false, 0.]));
    let intents = vm.take_movement_intents();
    assert_eq!(intents.len(), 1);
    advance(&mut prediction, &world, &intents, 1.);
    let blocked = json!({"sequence":2,"kind":"ChangeState","instance_id":"1:5","state":"Physics","state_value":16});
    advance(&mut prediction, &world, &[blocked], 1.1);
    assert_eq!(prediction.report()["physics_state"], false);
    assert_eq!(prediction.report()["accepted_state_changes"], 0);
}

#[test]
fn repeated_received_velocity_corrects_local_motion_but_stale_state_does_not() {
    let (mut vm, mut world) = runtime();
    let mut prediction = LocalPrediction::default();
    run(&mut vm,"local c=game:GetService('Players').LocalPlayer.Character;c.Humanoid:ChangeState(Enum.HumanoidStateType.Physics);c.HumanoidRootPart.AssemblyLinearVelocity=Vector3.new(6,0,0)");
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    advance(&mut prediction, &world, &[], 1.1);
    let delta = json!({"version":1,"delta":true,"sequence":2,"actions":[{"kind":"set","sequence":2,"id":"1:6","fields":{"AssemblyLinearVelocity":{"$vector3":[0,0,0]}}}]});
    world.apply_delta(&delta).unwrap();
    prediction.observe_replication(&receipt(delta));
    let frame = advance(&mut prediction, &world, &[], 1.2);
    assert_eq!(frame.velocity[0], 0.);
    assert!(
        (frame.position[0] - 0.6).abs() < 1e-6,
        "velocity receipt rewound position: {frame:?}"
    );
    assert_eq!(prediction.report()["physics_state"], true);
    let stale = json!({"version":1,"delta":true,"sequence":2,"actions":[{"kind":"set","sequence":1,"id":"1:5","fields":{"HumanoidStateType":8}}]});
    prediction.observe_replication(&receipt(stale));
    advance(&mut prediction, &world, &[], 1.3);
    assert_eq!(prediction.report()["physics_state"], true);
}
#[test]
fn physics_state_survives_move_and_jump_and_is_cleared_on_character_replacement() {
    let (mut vm, mut world) = runtime();
    let mut prediction = LocalPrediction::default();
    run(&mut vm,"local c=game:GetService('Players').LocalPlayer.Character;c.Humanoid:ChangeState(Enum.HumanoidStateType.Physics);c.HumanoidRootPart.AssemblyLinearVelocity=Vector3.new(6,0,0)");
    advance(&mut prediction, &world, &vm.take_movement_intents(), 1.);
    run(&mut vm,"local h=game:GetService('Players').LocalPlayer.Character.Humanoid;h:Move(Vector3.new(-1,0,0));h.Jump=true");
    assert_eq!(
        advance(&mut prediction, &world, &vm.take_movement_intents(), 1.1).velocity[0],
        6.
    );
    assert_eq!(prediction.report()["rejected_jumps"], 1);
    let delta = json!({"version":1,"delta":true,"sequence":4,"actions":[
        {"kind":"set","sequence":2,"id":"1:2","fields":{"Character":{"$ref":"1:8"}}},
        {"kind":"set","sequence":3,"id":"1:5","fields":{"Parent":{"$ref":"1:8"},"RootPart":{"$ref":"1:9"}}},
        {"kind":"set","sequence":4,"id":"1:9","fields":{"NetworkOwnerV3":7,"AssemblyRootPart":{"$ref":"1:9"}}}
    ]});
    world.apply_delta(&delta).unwrap();
    prediction.observe_replication(&receipt(delta));
    let frame = advance(&mut prediction, &world, &[], 1.2);
    assert_eq!(prediction.report()["physics_state"], false);
    assert_eq!(frame.binding.root_part_id, (1u64 << 32) | 9);
    assert_eq!(frame.velocity[0], 0.);
}

fn poll_actors(vm: &mut Runtime) {
    for _ in 0..4 {
        let r = vm.poll().unwrap();
        successful(&r).unwrap();
        for actor in r["actor_runtime"].as_array().into_iter().flatten() {
            assert!(actor.get("shadow_merge_error").is_none(), "{actor}");
            assert!(
                actor["task_errors"]
                    .as_array()
                    .is_none_or(|rows| rows.is_empty()),
                "{actor}"
            );
            assert!(
                actor["movement_forwarding"]["rejected"]
                    .as_u64()
                    .unwrap_or(0)
                    == 0,
                "{actor}"
            );
        }
    }
}
#[test]
fn actor_velocity_state_and_move_commands_reach_physics_once_in_order() {
    let (mut vm, world) = runtime_with_actors(1);
    poll_actors(&mut vm);
    run(&mut vm, "workspace.Worker0:SendMessage('drive',6)");
    poll_actors(&mut vm);
    let commands = vm.take_movement_intents();
    assert_eq!(commands.len(), 3, "{commands:?}");
    assert_eq!(
        commands
            .iter()
            .map(|v| v["kind"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["ChangeState", "AssemblyLinearVelocity", "Move"]
    );
    assert!(commands.iter().all(|v| v["actor_origin"] == "worker0"));
    assert!(commands
        .windows(2)
        .all(|w| w[0]["sequence"].as_u64() < w[1]["sequence"].as_u64()));
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &commands, 1.);
    let frame = advance(&mut prediction, &world, &[], 1.1);
    assert!((frame.position[0] - 0.6).abs() < 1e-6);
    assert!(frame.position[1] > 2.8);
    assert_eq!(prediction.report()["accepted_velocity_writes"], 1);
    assert_eq!(prediction.report()["accepted_state_changes"], 1);
    let feedback = prediction.feedback().unwrap();
    let reply=vm.call(json!({"op":"physics_feedback","engine_poses":feedback["engine_poses"],"feedback":feedback})).unwrap();
    successful(&reply).unwrap();
    run(&mut vm, "workspace.Worker0:SendMessage('sample')");
    poll_actors(&mut vm);
    let values=run(&mut vm,"return workspace.Worker0.Driver:GetAttribute('State'),workspace.Worker0.Driver:GetAttribute('X')");
    assert_eq!(values[0], "Physics");
    assert!((values[1].as_f64().unwrap() - 0.6).abs() < 1e-5);
    poll_actors(&mut vm);
    assert!(vm.take_movement_intents().is_empty());
}
#[test]
fn multiple_actor_sequences_are_rebased_after_root_commands() {
    let (mut vm, world) = runtime_with_actors(2);
    poll_actors(&mut vm);
    run(&mut vm,"local h=game:GetService('Players').LocalPlayer.Character.Humanoid;h:Move(Vector3.zero);workspace.Worker0:SendMessage('drive',6);workspace.Worker1:SendMessage('drive',9)");
    poll_actors(&mut vm);
    let commands = vm.take_movement_intents();
    assert_eq!(commands.len(), 7, "{commands:?}");
    assert!(commands[0].get("actor_origin").is_none());
    assert!(commands
        .windows(2)
        .all(|w| w[0]["sequence"].as_u64() < w[1]["sequence"].as_u64()));
    for actor in ["worker0", "worker1"] {
        let rows = commands
            .iter()
            .filter(|v| v["actor_origin"] == actor)
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["actor_sequence"], 1);
    }
    let last = commands
        .iter()
        .rev()
        .find(|v| v["kind"] == "AssemblyLinearVelocity")
        .unwrap()["vector"][0]
        .as_f64()
        .unwrap();
    let mut prediction = LocalPrediction::default();
    advance(&mut prediction, &world, &commands, 1.);
    assert_eq!(advance(&mut prediction, &world, &[], 1.1).velocity[0], last);
}
#[test]
fn actor_jump_and_pivot_replay_do_not_duplicate_commands() {
    let (mut vm, _) = runtime_with_actors(1);
    poll_actors(&mut vm);
    run(
        &mut vm,
        "workspace.Worker0:SendMessage('jump');workspace.Worker0:SendMessage('pivot',12)",
    );
    poll_actors(&mut vm);
    let commands = vm.take_movement_intents();
    assert_eq!(commands.len(), 2, "{commands:?}");
    assert_eq!(commands[0]["kind"], "Jump");
    assert_eq!(commands[1]["kind"], "PivotTo");
    assert_eq!(
        commands[1]["cframe"]["$cframe"]["position"],
        json!([12., 2., 0.])
    );
    assert_eq!(
        run(
            &mut vm,
            "return game:GetService('Players').LocalPlayer.Character.HumanoidRootPart.Position.X"
        ),
        json!([12.])
    );
}
#[test]
fn actor_command_acceptance_matches_forwarded_batches_across_slices() {
    let (mut vm, _) = runtime_with_actors(2);
    poll_actors(&mut vm);
    run(
        &mut vm,
        "workspace.Worker0:SendMessage('moves',200);workspace.Worker1:SendMessage('moves',200)",
    );
    let mut accepted = json!([-1., -1.]);
    for _ in 0..100 {
        poll_actors(&mut vm);
        accepted = run(&mut vm, "return workspace.Worker0.Driver:GetAttribute('Accepted') or -1, workspace.Worker1.Driver:GetAttribute('Accepted') or -1");
        if accepted.as_array().unwrap().iter().all(|n| n.as_f64().unwrap() >= 0.) {
            break;
        }
    }
    let counts: Vec<_> = accepted.as_array().unwrap().iter().map(|n| n.as_f64().unwrap()).collect();
    assert!(counts.iter().all(|n| (0.0..=200.0).contains(n)), "actors did not finish: {accepted}");
    let total = counts.iter().sum::<f64>() as usize;
    assert!((256..=400).contains(&total), "{accepted}");

    // A slower machine can split the callbacks across frames. Drain every batch
    // and check that each successful call reached the host exactly once.
    let mut commands = Vec::new();
    loop {
        let batch = vm.take_movement_intents();
        assert!(batch.len() <= 256);
        if batch.is_empty() { break; }
        commands.extend(batch);
    }
    assert_eq!(commands.len(), total, "{accepted}");
    for (index, count) in counts.iter().enumerate() {
        let actor = format!("worker{index}");
        assert_eq!(commands.iter().filter(|row| row["actor_origin"] == actor).count(), *count as usize);
    }
    for pair in commands.windows(2) {
        assert!(pair[0]["sequence"].as_u64().unwrap() < pair[1]["sequence"].as_u64().unwrap());
    }
    run(&mut vm, "workspace.Worker0:SendMessage('moves',1)");
    poll_actors(&mut vm);
    assert_eq!(vm.take_movement_intents().len(), 1);
}
