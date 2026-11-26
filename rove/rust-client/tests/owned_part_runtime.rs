#![cfg(feature = "rust-host")]
//! Actual VM writes cross the shared-world bridge without claiming a receipt.
use headless_luau::{
    engine_world_projection::ProjectionContext,
    engine_world_view::SharedWorldView,
    live_runtime::compact_snapshot,
    runtime_owner::{successful, Runtime},
};
use rove_client_core::{schema::Schema, world::WorldStore};
use serde_json::json;
use std::collections::HashMap;

fn runtime() -> (Runtime, WorldStore) {
    let mut store=WorldStore::from_snapshot(&json!({"version":1,"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","properties":{}},
        {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}}
    ]})).unwrap();
    store.apply_delta(&json!({"version":1,"delta":true,"sequence":10,"actions":[
        {"kind":"create","sequence":10,"node":{"id":"1:7","class_name":"Part","name":"Loose","parent":"workspace",
            "received_creation_complete":true,"properties":{"NetworkOwnerV3":9,"Anchored":false,
                "CFrame":{"$cframe":{"position":[0,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}}}}}
    ]})).unwrap();
    let schema = Schema {
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
    };
    let view = SharedWorldView::projected(
        store.view(),
        ProjectionContext::new(&schema, HashMap::new()).unwrap(),
    );
    let mut snapshot = store.to_snapshot();
    snapshot["services"] = json!({"Workspace":"workspace"});
    compact_snapshot(&mut snapshot, &view).unwrap();
    (
        Runtime::new_with_shared_world(snapshot, 32, &view).unwrap(),
        store,
    )
}

#[test]
fn owned_part_cframe_and_position_writes_preserve_receipt_evidence() {
    let (mut vm, store) = runtime();
    let result=vm.execute_control("workspace.Loose.CFrame=CFrame.new(20,420,67); workspace.Loose.Position=Vector3.new(67,350,67); assert(workspace.Loose.Position==Vector3.new(67,350,67)); return true".into(),2000).unwrap();
    successful(&result).unwrap();
    let intents = vm.take_movement_intents();
    assert_eq!(intents.len(), 2, "{result}");
    for intent in &intents {
        assert_eq!(intent["kind"], "OwnedPartCFrame");
        assert_eq!(intent["received_owner"], 9);
        assert_eq!(intent["created_sequence"], 10);
        assert_eq!(intent["received_sequence"], 10);
    }
    assert_eq!(
        intents[1]["cframe"]["$cframe"]["position"],
        json!([67., 350., 67.])
    );
    assert_eq!(
        store.get_property("1:7", "CFrame").unwrap()["$cframe"]["position"],
        json!([0, 0, 0])
    );
}

#[test]
fn owned_part_later_local_edits_invalidate_pending_network_poses() {
    for later in ["workspace.Loose.Anchored=true", "workspace.Loose.Parent=nil", "workspace.Loose:Destroy()",
        "local p=Instance.new('Part');p.Parent=workspace;local w=Instance.new('WeldConstraint');w.Part0=workspace.Loose;w.Part1=p;w.Parent=workspace"] {
        let (mut vm,_)=runtime();
        let result=vm.execute_control(format!("workspace.Loose.CFrame=CFrame.new(20,420,67); {later}; return true"),2000).unwrap();
        successful(&result).unwrap();
        assert!(vm.take_movement_intents().is_empty(),"{later}: {result}");
    }
}

#[test]
fn owned_part_locally_created_bodies_never_acquire_received_authority() {
    let (mut vm, _) = runtime();
    let result=vm.execute_control("local p=Instance.new('Part');p.Parent=workspace;p.CFrame=CFrame.new(1,2,3);return p.Position".into(),2000).unwrap();
    successful(&result).unwrap();
    assert!(vm.take_movement_intents().is_empty());
}

#[test]
fn owned_part_later_job_stops_an_already_published_body() {
    for later in [
        "workspace.Loose.Anchored=true",
        "workspace.Loose.Parent=nil",
        "workspace.Loose:Destroy()",
        "workspace.Loose.Size=Vector3.new(10,10,10)",
    ] {
        let (mut vm, _) = runtime();
        successful(
            &vm.execute_control("workspace.Loose.CFrame=CFrame.new(1,2,3)".into(), 2000)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(vm.take_movement_intents().len(), 1);
        successful(&vm.execute_control(later.into(), 2000).unwrap()).unwrap();
        let intents = vm.take_movement_intents();
        assert_eq!(intents.len(), 1, "{later}: {intents:?}");
        assert_eq!(intents[0]["kind"], "OwnedPartStop");
        assert_eq!(intents[0]["instance_id"], "1:7");
    }
}
