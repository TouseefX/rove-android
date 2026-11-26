use super::*;
use rove_client_core::world::WorldStore;

fn world() -> WorldStore {
    let mut store = WorldStore::from_snapshot(&json!({"version":1,"nodes":[
        {"id":"workspace","class_name":"Workspace","received_creation_complete":true,"properties":{}},
        {"id":"player","class_name":"Player","properties":{"Character":{"$ref":"character"}}},
        {"id":"character","class_name":"Model","parent":"workspace","properties":{}}
    ]})).unwrap();
    store.apply_delta(&json!({"version":1,"delta":true,"sequence":10,"actions":[
        {"kind":"create","sequence":10,"node":{"id":"1:7","class_name":"Part","parent":"workspace",
            "received_creation_complete":true,"properties":{"NetworkOwnerV3":9,"Anchored":false}}}
    ]})).unwrap();
    store
}
fn parents() -> HashMap<String, String> {
    [
        ("Part", "BasePart"),
        ("Weld", "JointInstance"),
        ("HingeConstraint", "Constraint"),
    ]
    .into_iter()
    .map(|(a, b)| (a.into(), b.into()))
    .collect()
}
fn intent() -> Value {
    json!({"kind":"OwnedPartCFrame","instance_id":"1:7","received_sequence":10,
        "created_sequence":10,"received_owner":9,"cframe":{"$cframe":{
            "position":[20,420,67],"rotation":[1,0,0,0,1,0,0,0,1]}}})
}
fn reject(world: &WorldStore, command: Value, reason: &str) {
    let mut sender = OwnedPartPoses::default();
    sender.observe(&[command]);
    assert!(sender
        .prepare(&world.view(), &parents(), 9, true)
        .is_empty());
    assert_eq!(
        sender.report()["rejected"][reason],
        1,
        "{}",
        sender.report()
    );
}

#[test]
fn owned_part_frames_preserve_local_pose_and_received_world() {
    let store = world();
    let before = store.to_snapshot();
    let mut sender = OwnedPartPoses::default();
    sender.observe(&[intent()]);
    let frames = sender.prepare(&store.view(), &parents(), 9, true);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].id, 1u64 << 32 | 7);
    assert_eq!(frames[0].position, [20., 420., 67.]);
    assert_eq!(frames[0].linear, [0.; 3]);
    assert_eq!(
        sender.report()["velocity_sources"]["local_constructor_initial_state"],
        1
    );
    assert_eq!(before, store.to_snapshot());
    assert!(sender
        .prepare(&store.view(), &parents(), 9, true)
        .is_empty());
}

fn contact_world() -> WorldStore {
    let mut store = world();
    store.apply_delta(&json!({"version":1,"delta":true,"sequence":14,"actions":[
        {"kind":"set","sequence":11,"id":"workspace","fields":{"Gravity":10.,"TouchesUseCollisionGroups":false}},
        {"kind":"set","sequence":12,"id":"1:7","fields":{"Size":{"$vector3":[2.,2.,2.]}}},
        {"kind":"create","sequence":13,"node":{"id":"1:8","class_name":"Part","parent":"workspace","received_creation_complete":true,
            "properties":{"Anchored":true,"Size":{"$vector3":[2.,2.,2.]},"CFrame":{"$cframe":{"position":[0.,0.,0.],"rotation":[1.,0.,0.,0.,1.,0.,0.,0.,1.]}}}}},
        {"kind":"create","sequence":14,"node":{"id":"1:9","class_name":"TouchTransmitter","parent":"1:8","properties":{}}}
    ]})).unwrap();
    store
}
fn contact_intent() -> Value {
    let mut command = intent();
    command["received_sequence"] = json!(14);
    command["cframe"]["$cframe"]["position"] = json!([0., 0.1, 0.]);
    command
}
#[test]
fn python_submitted_model_preserves_authored_pose_and_exact_begin_end_records() {
    let mut store=contact_world();
    let before=store.to_snapshot();
    let mut sender=OwnedPartPoses {python_submitted_model:true,..Default::default()};
    let near=contact_intent();
    sender.observe(&[near.clone()]);
    let first=sender.advance(&store.view(),&parents(),9,true,1.);
    assert_eq!(first.parts[0].position,[0.,0.1,0.]);
    // Exact Python encode_contacts bytes: scoped pairs, state 0/3, null GUID.
    assert_eq!(first.contacts,vec![vec![134,1,7,0,0,0,1,8,0,0,0,0,0]]);
    assert!(sender.advance(&store.view(),&parents(),9,true,2.).parts.is_empty());
    sender.observe(&[near.clone()]);
    assert!(sender.advance(&store.view(),&parents(),9,true,3.).contacts.is_empty());
    let mut far=near.clone();
    far["cframe"]["$cframe"]["position"]=json!([8.,4.,0.]);
    sender.observe(&[far]);
    let last=sender.advance(&store.view(),&parents(),9,true,4.);
    assert_eq!(last.parts[0].position,[8.,4.,0.]);
    assert_eq!(last.contacts,vec![vec![134,1,7,0,0,0,1,8,0,0,0,3,0]]);
    assert_eq!(sender.report()["local_dynamics"]["model"],"python_submitted_owned_box_overlap");
    assert_eq!(sender.report()["local_dynamics"]["physical_steps"],0);
    assert_eq!(store.to_snapshot(),before);
    sender.observe(&[near.clone()]);
    sender.advance(&store.view(),&parents(),9,true,5.);
    store.apply_delta(&json!({"version":1,"delta":true,"sequence":15,"actions":[
        {"kind":"set","sequence":15,"id":"1:7","fields":{"NetworkOwnerV3":8}}
    ]})).unwrap();
    sender.observe(&[near]);
    let revoked=sender.advance(&store.view(),&parents(),9,true,6.);
    assert!(revoked.parts.is_empty() && revoked.contacts.is_empty());
    assert_eq!(sender.report()["local_dynamics"]["active_contacts"],0);
}

#[test]
fn owned_part_contacts_require_a_physical_step_and_preserve_the_received_world() {
    let store = contact_world();
    let before = store.to_snapshot();
    let mut sender = OwnedPartPoses::default();
    sender.observe(&[contact_intent()]);
    let first = sender.advance(&store.view(), &parents(), 9, true, 1.);
    assert_eq!(first.parts.len(), 1);
    assert!(first.contacts.is_empty(), "teleport alone cannot touch");
    let next = sender.advance(&store.view(), &parents(), 9, true, 1.02);
    assert_eq!(next.parts.len(), 1);
    assert_eq!(next.parts[0].position[1], 2.);
    assert_eq!(
        next.contacts,
        [rove_client_core::replication_out::contact_batch(&[((1, 7), (1, 8), true)]).unwrap()]
    );
    assert!(sender
        .advance(&store.view(), &parents(), 9, true, 1.04)
        .contacts
        .is_empty());
    let mut away = contact_intent();
    away["cframe"]["$cframe"]["position"] = json!([8., 4., 0.]);
    sender.observe(&[away]);
    let next = sender.advance(&store.view(), &parents(), 9, true, 1.06);
    assert_eq!(
        next.contacts,
        [rove_client_core::replication_out::contact_batch(&[((1, 7), (1, 8), false)]).unwrap()]
    );
    assert_eq!(before, store.to_snapshot());
}
#[test]
fn owned_part_simulation_stops_on_local_cancel_and_received_revocation() {
    for local in [true, false] {
        let mut store = contact_world();
        let mut sender = OwnedPartPoses::default();
        sender.observe(&[contact_intent()]);
        sender.advance(&store.view(), &parents(), 9, true, 1.);
        assert!(!sender
            .advance(&store.view(), &parents(), 9, true, 1.02)
            .contacts
            .is_empty());
        if local {
            sender.observe(&[json!({"kind":"OwnedPartStop","instance_id":"1:7"})]);
        } else {
            store
                .apply_delta(&json!({"version":1,"delta":true,"sequence":15,"actions":[
                    {"kind":"set","sequence":15,"id":"1:7","fields":{"NetworkOwnerV3":8}}
                ]}))
                .unwrap();
        }
        let step = sender.advance(&store.view(), &parents(), 9, true, 1.04);
        assert!(step.parts.is_empty());
        assert!(step.contacts.is_empty(), "revocation cannot invent an end");
        assert_eq!(sender.report()["local_dynamics"]["active_bodies"], 0);
    }
}
#[test]
fn owned_part_velocity_is_not_derived_from_teleport_distance() {
    let store = contact_world();
    let mut sender = OwnedPartPoses::default();
    let mut command = contact_intent();
    command["cframe"]["$cframe"]["position"] = json!([100., 100., 0.]);
    sender.observe(&[command.clone()]);
    sender.advance(&store.view(), &parents(), 9, true, 1.);
    let first = sender.advance(&store.view(), &parents(), 9, true, 1.1);
    assert!((first.parts[0].linear[1] + 1.).abs() < 1e-8);
    command["cframe"]["$cframe"]["position"] = json!([500., 100., 0.]);
    sender.observe(&[command]);
    let second = sender.advance(&store.view(), &parents(), 9, true, 1.2);
    assert!((second.parts[0].linear[1] + 2.).abs() < 1e-8);
    assert_eq!(second.parts[0].linear[0], 0.);
}

#[test]
fn owned_part_frames_reject_revoked_authority_new_receipts_and_generations() {
    let mut command = intent();
    command["received_owner"] = json!(8);
    reject(&world(), command, "ownership_not_current");
    let mut command = intent();
    command["created_sequence"] = json!(9);
    reject(&world(), command, "part_generation_changed");
    for (field, value, reason) in [
        ("NetworkOwnerV3", json!(8), "ownership_not_current"),
        ("Anchored", json!(true), "part_not_known_unanchored"),
        (
            "CFrame",
            json!({"$cframe":{"position":[1,2,3]}}),
            "newer_received_state",
        ),
        (
            "ReplicationPV",
            json!({"position":[1,2,3]}),
            "newer_received_state",
        ),
        (
            "Parent",
            json!({"$ref":"character"}),
            "newer_received_state",
        ),
    ] {
        let mut store = world();
        store
            .apply_delta(&json!({"version":1,"delta":true,"sequence":11,"actions":[
                {"sequence":11,"kind":"set","id":"1:7","fields":{field:value}}
            ]}))
            .unwrap();
        reject(&store, intent(), reason);
    }
}

#[test]
fn owned_part_frames_exclude_characters_and_connected_bodies() {
    for (class, fields) in [
        (
            "Weld",
            json!({"Part0":{"$ref":"1:7"},"Part1":{"$ref":"1:8"}}),
        ),
        ("WeldConstraint", json!({"Part1":{"$ref":"1:7"}})),
    ] {
        let mut store = world();
        store.apply_delta(&json!({"version":1,"delta":true,"sequence":11,"actions":[
            {"sequence":11,"kind":"create","node":{"id":"joint","class_name":class,"properties":fields}}
        ]})).unwrap();
        reject(&store, intent(), "connected_assembly_unsupported");
    }
    let mut store = world();
    store
        .apply_delta(&json!({"version":1,"delta":true,"sequence":11,"actions":[
            {"sequence":11,"kind":"set","id":"1:7","fields":{"Parent":{"$ref":"character"}}}
        ]}))
        .unwrap();
    let mut command = intent();
    command["received_sequence"] = json!(11);
    reject(&store, command, "character_pose_excluded");
}

#[test]
fn owned_part_same_value_receipts_still_invalidate_older_writes() {
    for (field, value) in [
        ("NetworkOwnerV3", json!(9)),
        ("Parent", json!({"$ref":"workspace"})),
        ("PhysicsRepRootPart", json!({"$ref":"1:7"})),
        ("PhysicsRepRootRef", json!(null)),
        ("AssemblyRootPart", json!({"$ref":"1:7"})),
    ] {
        let mut store = world();
        store
            .apply_delta(&json!({"version":1,"delta":true,"sequence":11,"actions":[
                {"sequence":11,"kind":"set","id":"1:7","fields":{field:value}}
            ]}))
            .unwrap();
        assert_eq!(
            store
                .view()
                .read_only_node("1:7")
                .unwrap()
                .metadata_field("property_revisions", field),
            Some(json!(11))
        );
        reject(&store, intent(), "newer_received_state");
    }
}

#[test]
fn owned_part_frames_keep_received_velocity_and_do_not_guess_malformed_pv() {
    let mut store = world();
    store
        .apply_delta(&json!({"version":1,"delta":true,"sequence":11,"actions":[
            {"sequence":11,"kind":"set","id":"1:7","fields":{"ReplicationPV":{
                "position":[0,0,0],"decoded_velocities":[[1,2,3],[-1,-2,-3]]}}}
        ]}))
        .unwrap();
    let mut command = intent();
    command["received_sequence"] = json!(11);
    let mut sender = OwnedPartPoses::default();
    sender.observe(&[command]);
    let frames = sender.prepare(&store.view(), &parents(), 9, true);
    assert_eq!(frames[0].linear, [1., 2., 3.]);
    assert_eq!(frames[0].angular, [-1., -2., -3.]);
    assert_eq!(compact_velocity(&json!(null)), Some([0.; 3]));
    assert_eq!(compact_velocity(&json!([0, 0])), Some([0.; 3]));
    assert!(compact_velocity(&json!([0, 12])).is_none());
    assert!(compact_velocity(&json!([1, 12])).is_none());
    assert!(compact_velocity(&json!([16, 0x80000000u32])).is_none());
    assert!(compact_velocity(&json!([300, 0])).is_none());
    for velocity in [[1., -2., 3.], [-0.5, 0.25, 0.], [0.; 3]] {
        let raw = crate::physics_codec::encode_velocity(velocity).unwrap();
        let word = if raw.len() == 1 {
            0
        } else {
            u32::from_le_bytes(raw[1..].try_into().unwrap())
        };
        let got = compact_velocity(&json!([raw[0], word])).unwrap();
        for (a, b) in got.into_iter().zip(velocity) {
            assert!((a - b).abs() < 0.005);
        }
    }
}

#[test]
fn owned_part_queue_is_bounded_coalesces_and_respects_negotiation() {
    let mut sender = OwnedPartPoses::default();
    for i in 0..100 {
        let mut cmd = intent();
        cmd["instance_id"] = json!(format!("1:{i}"));
        sender.observe(&[cmd]);
    }
    assert_eq!(sender.pending.len(), LIMIT);
    sender.observe(&[intent()]);
    assert_eq!(sender.coalesced, 1);
    assert!(sender
        .prepare(&world().view(), &parents(), 9, false)
        .is_empty());
    assert_eq!(
        sender.report()["rejected"]["negotiated_physics_format_unsupported"],
        LIMIT
    );
    assert!(sender.pending.is_empty());
}

#[test]
fn owned_part_wire_matches_independent_python_and_constructor_catalog() {
    let cases: Value =
        serde_json::from_str(include_str!("../tests/fixtures/physics-owned-parts.json")).unwrap();
    for c in cases.as_array().unwrap() {
        let frame = PartFrame {
            id: c["id"].as_u64().unwrap(),
            position: vector(&c["position"]).unwrap(),
            rotation: vector(&c["rotation"]).unwrap(),
            linear: vector(&c["linear"]).unwrap(),
            angular: vector(&c["angular"]).unwrap(),
        };
        let bytes = crate::physics_sender::encode_owned_parts(
            c["timestamp_ms"].as_u64().unwrap(),
            c["sequence"].as_u64().unwrap(),
            &[frame],
        )
        .unwrap();
        assert_eq!(
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            c["hex"].as_str().unwrap()
        );
    }
    let initial: Value = serde_json::from_str(include_str!(
        "../../runtime/physics-initial-velocities-745.json"
    ))
    .unwrap();
    let source: Value =
        serde_json::from_str(include_str!("../../runtime/class-constructors-745.json")).unwrap();
    for (class, pair) in initial.as_object().unwrap() {
        for (i, name) in ["AssemblyLinearVelocity", "AssemblyAngularVelocity"]
            .iter()
            .enumerate()
        {
            assert_eq!(
                pair[i],
                source["classes"][class]["properties"][*name]["value"]["$vector3"]
            );
        }
    }
}

#[test]
fn owned_contact_depenetration_and_separation_in_one_step_emits_both_transitions() {
    let mut store = contact_world();
    // A thin platform resolves the initial overlap vertically, then gravity
    // separates the body during the same physical interval.
    store.apply_delta(&json!({"version":1,"delta":true,"sequence":15,"actions":[
        {"kind":"set","sequence":15,"id":"1:8","fields":{"Size":{"$vector3":[4.,1.,4.]}}}
    ]})).unwrap();
    let mut sender = OwnedPartPoses::default();
    let mut command = contact_intent();
    command["cframe"]["$cframe"]["position"] = json!([0.,0.,0.]);
    sender.observe(&[command]);
    assert!(sender.advance(&store.view(), &parents(), 9, true, 1.).contacts.is_empty());
    let next=sender.advance(&store.view(), &parents(), 9, true, 1.2);
    assert!(next.parts[0].position[1]< -1.5, "{:?}",next.parts[0]);
    assert_eq!(next.contacts, [rove_client_core::replication_out::contact_step_batch(&[
        ((1,7),(1,8),true),((1,7),(1,8),false)]).unwrap()]);
    assert!(sender.advance(&store.view(), &parents(), 9, true, 1.22).contacts.is_empty());
}

#[test]
fn submitted_contact_forgets_unavailable_touch_state_without_inventing_end() {
    let mut store=contact_world();
    let mut sender=OwnedPartPoses {python_submitted_model:true,..Default::default()};
    sender.observe(&[contact_intent()]);
    assert_eq!(sender.advance(&store.view(),&parents(),9,true,1.).contacts.len(),1);
    store.apply_delta(&json!({"version":1,"delta":true,"sequence":15,"actions":[
        {"kind":"set","sequence":15,"id":"1:7","fields":{"CanTouch":false}}
    ]})).unwrap();
    sender.observe(&[contact_intent()]);
    assert!(sender.advance(&store.view(),&parents(),9,true,2.).contacts.is_empty());
    assert_eq!(sender.report()["local_dynamics"]["active_contacts"],0);
}
