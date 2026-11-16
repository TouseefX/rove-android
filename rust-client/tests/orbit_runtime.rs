#![cfg(feature = "rust-host")]
use headless_luau::{
    engine_world_projection::ProjectionContext,
    engine_world_view::SharedWorldView,
    live_physics::LocalPrediction,
    live_runtime::compact_snapshot,
    physics_sender::encode_root_frame_with_rotation,
    runtime_owner::{successful, Runtime},
};
use rove_client_core::{schema::Schema, session::SessionRecord, world::WorldStore};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

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
fn vector(v: &Value) -> [f64; 3] {
    std::array::from_fn(|i| v[i].as_f64().unwrap())
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

#[test]
fn model_move_to_player_script_moves_the_character_and_queues_the_owned_root() {
    let mut snapshot = fixture();
    // MoveTo addresses the root's origin even with an authored pivot offset.
    snapshot["nodes"][6]["properties"]["PivotOffset"] = cf([7., 8., 9.]);
    let store = WorldStore::from_snapshot(&snapshot).unwrap();
    let view = SharedWorldView::projected(store.view(), context());
    compact_snapshot(&mut snapshot, &view).unwrap();
    let mut vm = Runtime::new_with_shared_world(snapshot, 32, &view).unwrap();
    let result = vm.execute_control(r#"
        local P=game:GetService("Players"); local target;
        for _,p in ipairs(P:GetPlayers()) do if p.Name:lower()=="mimiawee" then target=p; break end end;
        local c=P.LocalPlayer.Character; local t=target and target.Character;
        local r=t and t:FindFirstChild("HumanoidRootPart");
        if c and r then
            local ok,err=pcall(function() c:MoveTo(r.Position+Vector3.new(3,0,0)) end);
            print(ok and "Teleport requested" or tostring(err))
        else print("Player or character root unavailable") end
        assert(c.HumanoidRootPart.Position==Vector3.new(13,2,20))
        assert(c.Head.Position==Vector3.new(13,5,20))
        assert(r.Position==Vector3.new(10,2,20))
        return true
    "#.into(), 2000).unwrap();
    successful(&result).unwrap();
    assert_eq!(result["values"][0], true, "{result}");
    let intents = vm.take_movement_intents();
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0]["kind"], "PivotTo");
    assert_eq!(intents[0]["root_id"], "1:6");
    assert_eq!(intents[0]["cframe"]["$cframe"]["position"], json!([13.,2.,20.]));
    assert_eq!(intents[0]["cframe"]["$cframe"]["rotation"], json!(IDENTITY));
    let mut prediction = LocalPrediction::default();
    let frame = prediction.advance(&store.view(), &parents(), "1:2", 42, 7, true, &intents, 1.)
        .unwrap().expect("MoveTo must reach owned-root prediction");
    assert_eq!(frame.position, [13.,2.,20.]);
    assert_eq!(frame.rotation, IDENTITY);
    assert!(!frame.server_acceptance_confirmed);
    let wire = encode_root_frame_with_rotation(frame.binding.root_part_id, 1, 1,
        frame.position, frame.rotation, frame.velocity, frame.grounded).unwrap();
    assert_eq!(wire[0], 0x1b);
}

#[test]
fn exact_400_step_orbit_tracks_a_moving_player_and_emits_owned_root_frames() {
    let mut snapshot = fixture();
    let mut world = WorldStore::from_snapshot(&snapshot).unwrap();
    let context = context();
    let view = SharedWorldView::projected(world.view(), context.clone());
    compact_snapshot(&mut snapshot, &view).unwrap();
    let mut vm = Runtime::new_with_shared_world(snapshot, 32, &view).unwrap();
    let mut prediction = LocalPrediction::default();
    let started = Instant::now();
    let mut response=vm.call(json!({"op":"execute","timeout_ms":100,"source":include_str!("../../examples/orbit-player.luau")})).unwrap();
    successful(&response).unwrap();
    let job = response["job_id"].as_u64().unwrap();
    let mut center = [10., 2., 20.];
    let mut count = 0;
    loop {
        successful(&response).unwrap();
        let intents = vm.take_movement_intents();
        for intent in intents {
            count += 1;
            assert_eq!(intent["kind"], "PivotTo");
            assert_eq!(intent["root_id"], "1:6");
            let p = vector(&intent["cframe"]["$cframe"]["position"]);
            let offset = [p[0] - center[0], p[1] - center[1], p[2] - center[2]];
            assert!(
                (offset[0].hypot(offset[2]) - 8.).abs() < 0.0001,
                "step={count}, center={center:?}, p={p:?}"
            );
            assert!(offset[1].abs() < 0.0001);
            let frame = prediction
                .advance(
                    &world.view(),
                    &parents(),
                    "1:2",
                    42,
                    7,
                    true,
                    &[intent],
                    started.elapsed().as_secs_f64(),
                )
                .unwrap()
                .expect("owned root must accept the pivot");
            assert_eq!(frame.position, p);
            let facing = [-frame.rotation[2], -frame.rotation[5], -frame.rotation[8]];
            let dot: f64 = (0..3).map(|i| facing[i] * (-offset[i] / 8.)).sum();
            assert!(dot > 0.99999);
            let wire = encode_root_frame_with_rotation(
                frame.binding.root_part_id,
                1,
                count,
                p,
                frame.rotation,
                frame.velocity,
                frame.grounded,
            )
            .unwrap();
            assert_eq!(wire[0], 0x1b);
            assert_eq!(wire[17], 0x85);
            let feedback = prediction.feedback().unwrap();
            assert_eq!(
                feedback["engine_poses"]["frames"][0]["cframe"]["$cframe"]["rotation"],
                json!(frame.rotation)
            );
            let feedback_reply = vm
                .call(json!({"op":"physics_feedback",
                "engine_poses":feedback["engine_poses"],"feedback":feedback}))
                .unwrap();
            successful(&feedback_reply).unwrap();
            assert_eq!(feedback_reply["engine_pose_result"]["status"], "applied");
            assert!(!frame.server_acceptance_confirmed);
        }
        let status = vm.status(job).unwrap();
        if status["completed"] == true {
            successful(&status).unwrap();
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(45),
            "orbit stalled at step {count}: {status}"
        );
        if count > 0 && count % 20 == 0 && center[0] < 10. + (count / 20) as f64 {
            center[0] = 10. + (count / 20) as f64;
            let sequence = world.sequence() + 1;
            let delta = json!({"version":1,"delta":true,"sequence":sequence,"actions":[{"kind":"set","id":"1:9","sequence":sequence,"fields":{"CFrame":cf(center)}}]});
            world.apply_delta(&delta).unwrap();
            prediction.observe_replication(&receipt(delta.clone()));
            let view = SharedWorldView::projected(world.view(), context.clone());
            response = vm
                .call_with_shared_world(json!({"op":"update","timeout_ms":50,"delta":delta}), &view)
                .unwrap();
        } else {
            std::thread::sleep(Duration::from_millis(5));
            response = vm.poll().unwrap();
        }
    }
    assert_eq!(count, 400);
    assert_eq!(prediction.report()["accepted_pivots"], 400);
    assert_eq!(
        world.view().get_property("1:6", "CFrame").unwrap(),
        cf([0., 2., 0.])
    );
    let head=vm.execute_control("local c=game:GetService('Players').LocalPlayer.Character; return (c.Head.Position-c.HumanoidRootPart.Position).Magnitude".into(),50).unwrap();
    assert!((head["values"][0].as_f64().unwrap() - 3.).abs() < 0.0001);
    eprintln!("400 orbit steps, moving target, preserved facing, 400 encoded root frames in {:.2}s; offline only",started.elapsed().as_secs_f64());
}

#[test]
fn pivots_cannot_drive_other_players_or_an_unowned_root() {
    let world = WorldStore::from_snapshot(&fixture()).unwrap();
    let mut prediction = LocalPrediction::default();
    let intent = json!({"kind":"PivotTo","sequence":1,"instance_id":"1:8","player_id":"1:2","root_id":"1:9","cframe":cf([80.,2.,80.])});
    let frame = prediction
        .advance(&world.view(), &parents(), "1:2", 42, 7, true, &[intent], 1.)
        .unwrap()
        .unwrap();
    assert_eq!(frame.position, [0., 2., 0.]);
    assert_eq!(prediction.report()["accepted_pivots"], 0);
    assert_eq!(prediction.report()["rejected_intents"], 1);
    let mut raw = fixture();
    raw["nodes"][6]["properties"]["NetworkOwnerV3"] = json!(8);
    let world = WorldStore::from_snapshot(&raw).unwrap();
    let intent = json!({"kind":"PivotTo","sequence":2,"instance_id":"1:3","player_id":"1:2","root_id":"1:6","cframe":cf([80.,2.,80.])});
    assert!(prediction
        .advance(&world.view(), &parents(), "1:2", 42, 7, true, &[intent], 2.)
        .unwrap()
        .is_none());
    assert_eq!(prediction.report()["accepted_pivots"], 0);
}

#[test]
fn pivot_offset_uses_received_values_and_keeps_incomplete_creations_unknown() {
    for received_offset in [false, true] {
        let mut snapshot = fixture();
        snapshot["nodes"][6]["received_creation_complete"] = json!(false);
        if received_offset {
            snapshot["nodes"][6]["properties"]["PivotOffset"] = cf([2., 0., 0.]);
        }
        let store = WorldStore::from_snapshot(&snapshot).unwrap();
        let view = SharedWorldView::projected(store.view(), context());
        compact_snapshot(&mut snapshot, &view).unwrap();
        let mut vm = Runtime::new_with_shared_world(snapshot, 32, &view).unwrap();
        let result = vm
            .execute_control(
                r#"
            local c=game:GetService('Players').LocalPlayer.Character
            local ok,why=pcall(function() c:PivotTo(CFrame.new(20,2,0)) end)
            return ok,c.HumanoidRootPart.Position.X,tostring(why)
        "#
                .into(),
                50,
            )
            .unwrap();
        successful(&result).unwrap();
        assert_eq!(result["values"][0], received_offset, "{result}");
        if received_offset {
            assert_eq!(result["values"][1], 18.);
            let intents = vm.take_movement_intents();
            assert_eq!(intents.len(), 1);
            assert_eq!(
                intents[0]["cframe"]["$cframe"]["position"],
                json!([18., 2., 0.])
            );
        } else {
            assert_eq!(result["values"][1], 0.);
            assert!(result["values"][2]
                .as_str()
                .unwrap()
                .contains("PivotOffset"));
            assert!(vm.take_movement_intents().is_empty());
        }
    }
}

#[test]
fn a_new_received_root_pose_corrects_the_local_pivot_even_when_values_repeat() {
    let mut world = WorldStore::from_snapshot(&fixture()).unwrap();
    let mut prediction = LocalPrediction::default();
    let rotation = [0., 0., 1., 0., 1., 0., -1., 0., 0.];
    let intent = json!({"kind":"PivotTo","sequence":1,"instance_id":"1:3","player_id":"1:2","root_id":"1:6",
        "cframe":{"$cframe":{"position":[80,2,80],"rotation":rotation}}});
    let frame = prediction
        .advance(&world.view(), &parents(), "1:2", 42, 7, true, &[intent], 1.)
        .unwrap()
        .unwrap();
    assert_eq!(frame.position, [80., 2., 80.]);
    assert_eq!(frame.rotation, rotation);
    let still = prediction
        .advance(&world.view(), &parents(), "1:2", 42, 7, true, &[], 1.001)
        .unwrap()
        .unwrap();
    assert_eq!(still.rotation, rotation);
    // Neither another player's movement nor a non-pose root property should
    // roll our predicted character back to its old received position.
    let unrelated = json!({"version":1,"delta":true,"sequence":3,"actions":[
        {"kind":"set","sequence":2,"id":"1:9","fields":{"CFrame":cf([30.,2.,30.])}},
        {"kind":"set","sequence":3,"id":"1:6","fields":{"CanCollide":true}}]});
    world.apply_delta(&unrelated).unwrap();
    prediction.observe_replication(&receipt(unrelated));
    let still = prediction
        .advance(&world.view(), &parents(), "1:2", 42, 7, true, &[], 1.002)
        .unwrap()
        .unwrap();
    assert_eq!(still.position, [80., 2., 80.]);
    assert_eq!(still.rotation, rotation);
    // This is a new server pose receipt with the same values as the initial
    // snapshot. The committed receipt must still replace the authored pivot.
    let delta = json!({"version":1,"delta":true,"sequence":4,"actions":[{"kind":"set","sequence":4,"id":"1:6",
        "fields":{"CFrame":{"position":[0,2,0],"rotation_id":2}}}]});
    world.apply_delta(&delta).unwrap();
    prediction.observe_replication(&receipt(delta));
    let corrected = prediction
        .advance(&world.view(), &parents(), "1:2", 42, 7, true, &[], 1.02)
        .unwrap()
        .unwrap();
    assert_eq!(corrected.position, [0., 2., 0.]);
    assert_eq!(corrected.rotation, IDENTITY);
    assert!(!corrected.server_acceptance_confirmed);
}
