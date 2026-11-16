//! Ownership and assembly guards must use the same values for both world stores.
use super::*;
use crate::{engine_world_projection::ProjectionContext, engine_world_view::SharedWorldView};
use rove_client_core::{schema::Schema, world::WorldStore};
use serde_json::Value as Json;
use std::collections::HashMap;

fn raw() -> Json {
    json!({"version":1,"sequence":10,"services":{"Workspace":"ws"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game"},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game"},
        {"id":"char","class_name":"Model","name":"Character","parent":"ws"},
        {"id":"player","class_name":"Player","name":"Player","parent":"game","properties":{"Character":{"$ref":"char"}}},
        {"id":"p","class_name":"Part","name":"Loose","parent":"ws","server_owned":true,"property_revisions":{"CFrame":7,"Anchored":7,"NetworkOwnerV3":7},"properties":{"Anchored":false,"NetworkOwnerV3":7,"CFrame":{"$cframe":{"position":[0,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}}}}
    ]})
}
fn from_raw(raw: Json, shared: bool) -> WorldRef {
    if !shared {
        return Rc::new(RefCell::new(World::from_snapshot(&raw, None).unwrap()));
    }
    let store = WorldStore::from_snapshot(&raw).unwrap();
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
    Rc::new(RefCell::new(
        World::from_snapshot(&json!({"version":1,"sequence":10,"nodes":[]}), Some(&view)).unwrap(),
    ))
}
fn batch() -> Json {
    json!({"scope":7,"source_revision":10,"frames":[{"id":"p","cframe":{"$cframe":{"position":[5,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}},"linear_velocity":[3,0,0],"angular_velocity":[0,0,0]}]})
}
fn node<'a>(raw: &'a mut Json, id: &str) -> &'a mut Json {
    raw["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|n| n["id"] == id)
        .unwrap()
}
fn status(ctx: &WorldRef) -> String {
    apply_engine_owned_parts(ctx, &batch(), 7).unwrap()["status"]
        .as_str()
        .unwrap()
        .to_owned()
}
#[test]
fn shared_owned_body_accepts_valid_received_authority() {
    let ctx = from_raw(raw(), true);
    assert_eq!(status(&ctx), "applied");
    assert_eq!(
        world::get_property(&ctx, "p", "Position").unwrap(),
        json!({"$vector3":[5.,0.,0.]})
    );
    assert!(ctx.borrow().effects.get("mutations").is_none());
}
#[test]
fn shared_anchoring_and_assembly_root_guards_survive_a_local_owner_overlay() {
    for (key, value) in [
        ("Anchored", json!(true)),
        ("PhysicsRepRootPart", json!({"$ref":"another"})),
        ("PhysicsRepRootRef", json!({"$ref":"another"})),
    ] {
        let mut raw = raw();
        node(&mut raw, "p")["properties"][key] = value;
        let ctx = from_raw(raw, true);
        world::set_engine_property(&ctx, "p", "NetworkOwnerV3", json!(7), None).unwrap();
        assert_eq!(status(&ctx), "stale_authority", "{key}");
    }
}
#[test]
fn shared_player_character_reference_blocks_isolated_body_feedback() {
    let mut raw = raw();
    node(&mut raw, "p")["parent"] = json!("char");
    let ctx = from_raw(raw, true);
    world::set_engine_property(&ctx, "p", "NetworkOwnerV3", json!(7), None).unwrap();
    assert_eq!(status(&ctx), "stale_authority");
}
#[test]
fn shared_joint_and_attachment_references_block_isolated_body_feedback() {
    for (class, fields) in [
        ("WeldConstraint", json!({"Part0":{"$ref":"p"}})),
        ("Motor6D", json!({"Part1":{"$ref":"p"}})),
        ("RopeConstraint", json!({"Attachment0":{"$ref":"a"}})),
    ] {
        let mut raw = raw();
        raw["nodes"].as_array_mut().unwrap().extend([
            json!({"id":"a","class_name":"Attachment","name":"Attachment","parent":"p"}),
            json!({"id":"joint","class_name":class,"name":"Joint","parent":"ws","properties":fields})
        ]);
        let ctx = from_raw(raw, true);
        world::set_engine_property(&ctx, "p", "NetworkOwnerV3", json!(7), None).unwrap();
        assert_eq!(status(&ctx), "stale_authority", "{class}");
    }
}
#[test]
fn local_reference_overrides_win_over_received_references_in_both_stores() {
    for shared in [false, true] {
        let mut raw = raw();
        node(&mut raw, "p")["parent"] = json!("char");
        raw["nodes"].as_array_mut().unwrap().push(json!({"id":"joint","class_name":"Weld","name":"Joint","parent":"ws","properties":{"Part0":{"$ref":"p"}}}));
        let ctx = from_raw(raw, shared);
        world::set_engine_property(&ctx, "p", "NetworkOwnerV3", json!(7), None).unwrap();
        world::set_engine_property(&ctx, "player", "Character", Json::Null, None).unwrap();
        world::set_engine_property(&ctx, "joint", "Part0", Json::Null, None).unwrap();
        assert_eq!(status(&ctx), "applied", "shared={shared}");
    }
}
#[test]
fn ownership_check_does_not_expand_unrelated_shared_parts() {
    let mut raw = raw();
    for i in 0..1000 {
        raw["nodes"].as_array_mut().unwrap().push(
            json!({"id":format!("untouched{i}"),"class_name":"Part","name":"Part","parent":"ws"}),
        );
    }
    let ctx = from_raw(raw, true);
    assert_eq!(status(&ctx), "applied");
    let world = ctx.borrow();
    for i in 0..1000 {
        assert!(world.nodes.peek(&format!("untouched{i}")).is_none());
    }
}

#[test]
fn unknown_or_different_shared_owner_is_not_inferred_as_local() {
    for owner in [Json::Null, json!(8)] {
        let mut raw = raw();
        node(&mut raw, "p")["properties"]["NetworkOwnerV3"] = owner;
        assert_eq!(status(&from_raw(raw, true)), "stale_authority");
    }
    let mut raw = raw();
    node(&mut raw, "p")["properties"]
        .as_object_mut()
        .unwrap()
        .remove("NetworkOwnerV3");
    assert_eq!(status(&from_raw(raw, true)), "stale_authority");
}

#[test]
fn local_joint_reference_additions_are_honored_in_both_stores() {
    for shared in [false, true] {
        let mut raw = raw();
        raw["nodes"].as_array_mut().unwrap().push(json!({"id":"joint","class_name":"Weld","name":"Joint","parent":"ws","properties":{"Part0":null}}));
        let ctx = from_raw(raw, shared);
        world::set_engine_property(&ctx, "joint", "Part0", json!({"$ref":"p"}), None).unwrap();
        assert_eq!(status(&ctx), "stale_authority", "shared={shared}");
    }
}

#[test]
fn mixed_shared_authority_batch_rejects_before_any_body_moves() {
    let mut raw = raw();
    let mut second = node(&mut raw, "p").clone();
    second["id"] = json!("q");
    second["properties"]["NetworkOwnerV3"] = json!(8);
    raw["nodes"].as_array_mut().unwrap().push(second);
    let ctx = from_raw(raw, true);
    let mut batch = batch();
    let mut frame = batch["frames"][0].clone();
    frame["id"] = json!("q");
    batch["frames"].as_array_mut().unwrap().push(frame);
    assert_eq!(
        apply_engine_owned_parts(&ctx, &batch, 7).unwrap(),
        json!({"status":"stale_authority","frames_applied":0})
    );
    assert_eq!(
        world::get_property(&ctx, "p", "Position").unwrap(),
        json!({"$vector3":[0,0,0]})
    );
    assert!(!ctx.borrow().nodes["p"]
        .local_overrides
        .contains_key("CFrame"));
}
