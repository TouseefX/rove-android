//! The bridge's unit tests: rove's replication shapes in, rustudio's
//! DataModel out. They run in CI (`.github/workflows/ci.yml`, `check`).
//!
//! The captured half is rove's own `tests/fixtures/world_snapshot.json`, read
//! from the data tree this repository mirrors (`rove/`, not a workspace
//! member), so the fixture is never copied into the build and cannot drift
//! from the shape rove's `WorldStore` parses. The rest of the file builds
//! small snapshots in place for the cases the seven-node fixture cannot show:
//! ordering, dangling refs, refusals, a 90° rotation.

use rbx_dom_weak::types::{Faces, Variant, Vector3};
use rbx_dom_weak::ustr;
use serde_json::{json, Value as J};

use rustudio_live::{snapshot, world};

/// rove's captured snapshot, in the data tree ('rove/' is not a workspace
/// member; nothing builds it).
const ROVE_SNAPSHOT: &str = include_str!("../../../rove/tests/fixtures/world_snapshot.json");

fn fixture() -> world::World {
    build_text(ROVE_SNAPSHOT)
}

fn build_text(text: &str) -> world::World {
    let parsed = snapshot::parse(text).expect("parses");
    world::build(parsed, "Fixture World".into()).expect("builds").world
}

/// A snapshot of `nodes` with the top-level fields rove writes.
fn snapshot_json(nodes: Vec<J>, local_player: Option<&str>) -> String {
    let mut root = json!({
        "version": 1,
        "sequence": 7,
        "job_id": "synthetic-snapshot-job",
        "services": {},
        "class_parents": {},
        "defaults": {},
        "default_origins": {},
        "nodes": nodes,
    });
    if let Some(player) = local_player {
        root["local_player"] = json!(player);
    }
    root.to_string()
}

fn node(id: &str, class: &str, parent: Option<&str>, properties: J) -> J {
    json!({
        "id": id,
        "class_name": class,
        "name": id,
        "parent": parent,
        "properties": properties,
    })
}

// ---------------------------------------------------------------- the values

#[test]
fn rove_value_tags_become_rbx_types_values() {
    let mut conv = snapshot::ValueConverter::default();
    // rove's tags
    assert_eq!(
        conv.value("Position", "Part", &json!({"$vector3": [1.0, 2.0, 3.0]})).unwrap(),
        Variant::Vector3(Vector3::new(1.0, 2.0, 3.0))
    );
    let Variant::CFrame(cf) = conv
        .value("CFrame", "Part", &json!({"$cframe": {"position": [1, 2, 3], "rotation": [1,0,0,0,1,0,0,0,1]}}))
        .unwrap()
    else {
        panic!("not a CFrame")
    };
    assert_eq!(cf.position, Vector3::new(1.0, 2.0, 3.0));
    // The identity, as its columns: compared component-wise because whether
    // Matrix3 itself derives PartialEq is not something this crate can see
    // (rbx_types is not vendored).
    assert_eq!((cf.orientation.x, cf.orientation.y, cf.orientation.z),
               (Vector3::new(1.0, 0.0, 0.0), Vector3::new(0.0, 1.0, 0.0), Vector3::new(0.0, 0.0, 1.0)));
    assert_eq!(
        conv.value("Size", "Part", &json!({"$vector3": [4, 1, 2]})).unwrap(),
        Variant::Vector3(Vector3::new(4.0, 1.0, 2.0))
    );
    // Untagged scalars take the property's own type from api-dump.json: a
    // Float32 property is Float32, not a JSON number — the engine, the
    // renderer and Luau read the type, not only the value.
    assert_eq!(conv.value("Gravity", "Workspace", &json!(196.2)).unwrap(), Variant::Float32(196.2));
    assert_eq!(conv.value("WalkSpeed", "Humanoid", &json!(16)).unwrap(), Variant::Float32(16.0));
    assert_eq!(conv.value("Anchored", "Part", &json!(true)).unwrap(), Variant::Bool(true));
    assert_eq!(conv.value("Name", "Instance", &json!("FixturePlayer")).unwrap(), Variant::String("FixturePlayer".into()));
    // A place-file shape under a tag that carries one: one reader, not two.
    assert_eq!(
        conv.value("Size", "Part", &json!({"$vector3": [2, 2, 2]})).unwrap(),
        Variant::Vector3(Vector3::new(2.0, 2.0, 2.0))
    );
}

#[test]
fn a_rotation_is_read_row_major_as_rove_writes_it() {
    // 90° about Y, row-major, as rove's replication carries it (physics.rs
    // indexes rotation[row * 3 + column]).
    let cframe = json!({"$cframe": {"position": [0, 5, 0], "rotation": [0, 0, 1, 0, 1, 0, -1, 0, 0]}});
    let mut conv = snapshot::ValueConverter::default();
    let Variant::CFrame(cf) = conv.value("CFrame", "Part", &cframe).unwrap() else { panic!() };
    // Matrix3's x, y, z are its columns: for a Y-90° rotation column 0 is
    // (0, 0, -1) and column 2 is (1, 0, 0). (rove writes row-major into a
    // column-major CFrame, which is what makes the columns the axes.)
    assert_eq!(cf.orientation.x, Vector3::new(0.0, 0.0, -1.0));
    assert_eq!(cf.orientation.y, Vector3::new(0.0, 1.0, 0.0));
    assert_eq!(cf.orientation.z, Vector3::new(1.0, 0.0, 0.0));
    // And the same CFrame read back through rustudio's own writer.
    assert_eq!(rustudio_core::project_values::to_json("Part", "CFrame", &Variant::CFrame(cf)).is_some(), true);
}

#[test]
fn a_bitmask_faces_value_is_six_bits_in_one_byte() {
    // rove's replication writes {"$faces": bits} as one byte
    // (replication.rs), not a list of names: 63 is all six faces.
    let mut conv = snapshot::ValueConverter::default();
    let Variant::Faces(all) = conv.value("Faces", "Part", &json!({"$faces": 63})).unwrap() else {
        panic!("not Faces")
    };
    assert!(all.contains(Faces::FRONT) && all.contains(Faces::BOTTOM) && all.contains(Faces::RIGHT));
    let Variant::Faces(some) = conv.value("Faces", "Part", &json!({"$faces": 2})).unwrap() else {
        panic!("not Faces")
    };
    assert!(some == Faces::TOP, "a single bit must be a single face");
    // A bit outside the six faces is a wrong world, not a silent clipping.
    assert!(conv.value("Faces", "Part", &json!({"$faces": 128})).is_err());
    // The named form a place file uses still reads.
    let Variant::Faces(named) = conv
        .value("Faces", "Part", &json!({"$faces": ["Top", "Front"]}))
        .unwrap()
    else {
        panic!("not Faces")
    };
    assert!(named.contains(Faces::TOP) && named.contains(Faces::FRONT) && !named.contains(Faces::LEFT));
}

#[test]
fn a_brickcolor_is_its_number_or_its_name() {
    // rove sends the number ({"$brickcolor": 194}); a hand-written snapshot may
    // spell the name. Both are the same reader, so both must work.
    let mut conv = snapshot::ValueConverter::default();
    let by_number = conv.value("BrickColor", "Part", &json!({"$brickcolor": 194})).unwrap();
    let by_name = conv
        .value("BrickColor", "Part", &json!({"$brickcolor": "Medium stone grey"}))
        .unwrap();
    assert!(matches!(by_number, Variant::BrickColor(_)), "not a BrickColor");
    assert_eq!(by_number, by_name);
    // A number that is not a BrickColor is refused, not defaulted.
    assert!(conv.value("BrickColor", "Part", &json!({"$brickcolor": 65535})).is_err());
}

#[test]
fn every_remaining_tag_converts_to_the_type_its_property_has() {
    let mut conv = snapshot::ValueConverter::default();
    let mut v = |property: &str, value: serde_json::Value| {
        conv.value(property, "Part", &value).unwrap_or_else(|e| panic!("{property}: {e}"))
    };
    assert!(matches!(v("Size", json!({"$udim": [3, 0]})), Variant::UDim(_)));
    assert!(matches!(v("Size", json!({"$udim2": [[1, 0], [2, 0]]})), Variant::UDim2(_)));
    assert!(matches!(v("Color", json!({"$color3": [0.5, 0.25, 0.0]})), Variant::Color3(_)));
    assert!(matches!(v("Color", json!({"$color3uint8": [255, 128, 0]})), Variant::Color3uint8(_)));
    assert!(matches!(v("Size", json!({"$rect": [0, 0, 4, 2]})), Variant::Rect(_)));
    assert_eq!(v("Value", json!({"$number": 2.5})), Variant::Float64(2.5));
    assert_eq!(v("Name", json!({"$string": "Part"})), Variant::String("Part".into()));
    assert_eq!(v("Value", json!({"$shared_string": "shared"})), Variant::String("shared".into()));
    assert_eq!(v("TextureID", json!({"$content": "rbxassetid://1"})), Variant::String("rbxassetid://1".into()));
    assert!(matches!(v("Value", json!({"$enum": 3})), Variant::Enum(_)));
    // $bytes_hex: the odd-length and non-hex forms are refused.
    let Variant::BinaryString(blob) = v("Value", json!({"$bytes_hex": "00ff10"})) else {
        panic!("not a BinaryString")
    };
    assert_eq!(AsRef::<[u8]>::as_ref(&blob), &[0x00, 0xff, 0x10]);
    assert!(conv.value("Value", "Part", &json!({"$bytes_hex": "abc"})).is_err());
    assert!(conv.value("Value", "Part", &json!({"$bytes_hex": "zz"})).is_err());
}

#[test]
fn an_unknown_tag_is_an_error_naming_the_property() {
    let mut conv = snapshot::ValueConverter::default();
    let err = conv.value("CFrame", "Part", &json!({"$quaternion": [1, 0, 0, 0]})).unwrap_err();
    assert!(err.contains("$quaternion"), "{err}");
    assert!(err.contains("Part.CFrame"), "{err}");
    assert_eq!(conv.refused.len(), 1);
}

#[test]
fn values_that_cannot_be_converted_are_refused_not_defaulted() {
    let mut conv = snapshot::ValueConverter::default();
    // A DateTime is not converted yet: refused by name, never defaulted.
    let err = conv.value("Created", "Instance", &json!({"$datetime": 1700000000})).unwrap_err();
    assert!(err.contains("$datetime"), "{err}");
    // A $vector3 of two numbers.
    let err = conv.value("Position", "Part", &json!({"$vector3": [1, 2]})).unwrap_err();
    assert!(err.contains("wants 3 numbers"), "{err}");
    // A property api-dump.json does not know, with no tag.
    let err = conv.value("NotAProperty", "Part", &json!(1)).unwrap_err();
    assert!(!err.is_empty());
}

// ----------------------------------------------------------------- the world

#[test]
fn the_fixtures_ids_become_refs_and_its_tree_is_parented() {
    let world = fixture();
    assert_eq!(world.stats.nodes, 7, "rove's snapshot has seven nodes");
    assert_eq!(world.stats.instances, 8, "seven nodes and the DataModel root");
    assert_eq!(
        world.stats.root_classes,
        vec!["Workspace".to_string(), "Players".to_string(), "RunService".to_string()]
    );

    // Every node's rove id answers with a Ref, and back.
    for (id, r) in &world.ids {
        assert_eq!(world.refs.get(r), Some(id), "{id} does not map back");
        assert!(world.dom.get_by_ref(*r).is_some(), "{id} has no instance");
    }
    // Parenting is the snapshot's, not an accident of insertion order.
    let workspace = world.ids["scope:workspace"];
    assert_eq!(world.parent_of("scope:workspace"), Some(world.dom.root_ref()));
    assert_eq!(world.parent_of("scope:player-1"), Some(world.ids["scope:players"]));
    assert_eq!(world.parent_of("scope:character-1"), Some(workspace));
    assert_eq!(world.parent_of("scope:root-1"), Some(world.ids["scope:character-1"]));
    assert_eq!(world.dom.get_by_ref(workspace).unwrap().children().len(), 1);
}

#[test]
fn local_player_and_character_are_bound_as_refs() {
    let world = fixture();
    let players = world.ids["scope:players"];
    let player = world.ids["scope:player-1"];
    let character = world.ids["scope:character-1"];

    // Players.LocalPlayer is a Ref to the player, which is where the camera,
    // the GUI and M4's character binding read it.
    assert_eq!(world.stored("scope:players", "LocalPlayer"), Some(Variant::Ref(player)));
    assert_eq!(world.local_player, Some(player));
    // Player.Character is the `$ref` the snapshot carried.
    assert_eq!(world.stored("scope:player-1", "Character"), Some(Variant::Ref(character)));
    assert_eq!(world.doc.prop(player, "UserId"), Some(Variant::Int64(123)));
    assert_eq!(world.dom.get_by_ref(player).unwrap().name, "FixturePlayer");
}

#[test]
fn the_part_is_where_the_snapshot_puts_it() {
    let world = fixture();
    let part = world.ids["scope:root-1"];
    let Some(Variant::CFrame(cframe)) = world.stored("scope:root-1", "CFrame") else {
        panic!("the part has no stored CFrame")
    };
    assert_eq!(cframe.position, Vector3::new(1.0, 2.0, 3.0));
    assert_eq!(world.stored("scope:root-1", "Anchored"), Some(Variant::Bool(true)));
    assert_eq!(world.dom.get_by_ref(part).unwrap().class.as_str(), "Part");
    // The Humanoid's own numbers, at their property types.
    assert_eq!(world.stored("scope:humanoid-1", "WalkSpeed"), Some(Variant::Float32(16.0)));
    assert_eq!(world.doc.prop(world.ids["scope:humanoid-1"], "Health"), Some(Variant::Float32(100.0)));
}

#[test]
fn the_build_reports_what_it_did() {
    let parsed = snapshot::parse(ROVE_SNAPSHOT).unwrap();
    let built = world::build(parsed, "Fixture World".into()).unwrap();
    assert_eq!(built.world.stats.local_player_nodes, 1);
    assert_eq!(built.world.stats.refs_bound, 2, "LocalPlayer and Player.Character");
    assert_eq!(built.world.stats.dangling_refs, 0);
    assert_eq!(built.world.stats.refused_values, 0);
    assert!(built.world.stats.duration_ms >= 0.0);
    // Printed, not just asserted: a failing assertion here says which number
    // was wrong, and this line says all of them.
    println!("report: {}", built.report);
    println!("stats:  {}", built.world.stats_json());
    assert!(built.report.contains("7 nodes"), "{}", built.report);
    assert!(built.report.contains("8 instances"), "report: {}", built.report);
    assert!(built.report.contains("LocalPlayer: FixturePlayer"), "report: {}", built.report);
    // --result carries the same numbers.
    let json = built.world.stats_json();
    assert_eq!(json["nodes"], 7);
    assert_eq!(json["refs_bound"], 2);
}

// --------------------------------------------------------------- the orders

#[test]
fn a_child_that_arrives_before_its_parent_waits_for_it() {
    let mut root: J = serde_json::from_str(ROVE_SNAPSHOT).unwrap();
    root["nodes"].as_array_mut().unwrap().reverse();
    let built = world::build(snapshot::parse(&root.to_string()).unwrap(), "Reversed".into()).unwrap();
    assert_eq!(built.world.stats.nodes, 7);
    assert_eq!(built.world.stats.instances, 8);
    assert_eq!(built.world.stats.orphans, 0);
    // Three nodes are services at the root; the other four waited.
    assert_eq!(built.world.stats.out_of_order, 4);
    assert_eq!(built.world.parent_of("scope:root-1"), Some(built.world.ids["scope:character-1"]));
}

#[test]
fn a_child_of_a_node_the_snapshot_lacks_is_kept_at_the_root_and_counted() {
    let mut root: J = serde_json::from_str(ROVE_SNAPSHOT).unwrap();
    root["nodes"].as_array_mut().unwrap().retain(|n| n["id"] != "scope:character-1");
    let built = world::build(snapshot::parse(&root.to_string()).unwrap(), "Orphaned".into()).unwrap();
    assert_eq!(built.world.stats.orphans, 2, "Humanoid and HumanoidRootPart");
    assert!(built.report.contains("2 instances had no parent"), "report: {}", built.report);
    // They exist and are reachable, not dropped.
    assert!(built.world.dom.get_by_ref(built.world.ids["scope:root-1"]).is_some());
}

#[test]
fn a_ref_to_a_node_the_snapshot_lacks_is_dropped_and_counted() {
    let mut root: J = serde_json::from_str(ROVE_SNAPSHOT).unwrap();
    root["nodes"].as_array_mut().unwrap().retain(|n| n["id"] != "scope:character-1");
    let built = world::build(snapshot::parse(&root.to_string()).unwrap(), "Dangling".into()).unwrap();
    // Player.Character pointed at the dropped node. It must not become a Ref
    // into nowhere; the property keeps its default and the count says so.
    assert_eq!(built.world.stored("scope:player-1", "Character"), None);
    assert_eq!(built.world.stats.dangling_refs, 1);
    assert!(built.report.contains("1 property pointed at a node"), "{}", built.report);
}

// -------------------------------------------------------------- the versions

#[test]
fn a_snapshot_of_another_version_is_refused_by_name() {
    let mut root: J = serde_json::from_str(ROVE_SNAPSHOT).unwrap();
    root["version"] = json!(2);
    let err = snapshot::parse(&root.to_string()).unwrap_err();
    assert!(err.contains("version 2"), "{err}");
    assert!(err.contains("version 1"), "{err}");
}

#[test]
fn a_node_without_its_identity_is_refused_before_anything_is_built() {
    let mut root: J = serde_json::from_str(ROVE_SNAPSHOT).unwrap();
    root["nodes"][0].as_object_mut().unwrap().remove("id");
    let err = snapshot::parse(&root.to_string()).unwrap_err();
    assert!(err.contains("id"), "{err}");

    let mut root: J = serde_json::from_str(ROVE_SNAPSHOT).unwrap();
    let first = root["nodes"][0].clone();
    root["nodes"].as_array_mut().unwrap().push(first);
    let err = snapshot::parse(&root.to_string()).unwrap_err();
    assert!(err.contains("duplicate"), "{err}");
}

// ------------------------------------------------------------- the one-offs

#[test]
fn a_class_name_the_engine_does_not_know_still_gets_an_instance() {
    // Not a guess at behaviour: the world must show what rove sent, and the
    // gap telemetry (docs/ENGINE-GAPS.md, `missing`) is what reports it.
    let text = snapshot_json(
        vec![
            node("scope:workspace", "Workspace", None, json!({"Gravity": 196.2})),
            node("scope:odd", "ThisClassIsNotRoblox", Some("scope:workspace"), json!({})),
        ],
        None,
    );
    let world = build_text(&text);
    assert_eq!(world.dom.get_by_ref(world.ids["scope:odd"]).unwrap().class.as_str(), "ThisClassIsNotRoblox");
    assert_eq!(world.parent_of("scope:odd"), Some(world.ids["scope:workspace"]));
}

#[test]
fn an_untagged_property_of_a_type_the_dump_knows_is_typed_by_the_dump() {
    let text = snapshot_json(
        vec![
            node("scope:workspace", "Workspace", None, json!({})),
            node("scope:p", "Part", Some("scope:workspace"), json!({"Size": [4, 1, 2], "Transparency": 0.25, "Name": "Ledge"})),
        ],
        None,
    );
    let world = build_text(&text);
    assert_eq!(world.stored("scope:p", "Size"), Some(Variant::Vector3(Vector3::new(4.0, 1.0, 2.0))));
    assert_eq!(world.stored("scope:p", "Transparency"), Some(Variant::Float32(0.25)));
    // A Name in the properties is the instance's name, or the builder keeps
    // the node's: either way the instance is not nameless.
    assert!(!world.dom.get_by_ref(world.ids["scope:p"]).unwrap().name.is_empty());
    let _ = ustr("Size");
}

#[test]
fn a_parent_written_as_a_ref_object_is_the_same_edge_as_a_string() {
    // rove's WorldStore accepts both forms for a node's parent.
    let text = snapshot_json(
        vec![
            node("scope:workspace", "Workspace", None, json!({})),
            json!({
                "id": "scope:p",
                "class_name": "Part",
                "parent": {"$ref": "scope:workspace"},
                "properties": {"Anchored": true},
            }),
        ],
        None,
    );
    let world = build_text(&text);
    assert_eq!(world.parent_of("scope:p"), Some(world.ids["scope:workspace"]));
}

#[test]
fn an_absent_name_gets_the_class_default_and_a_empty_one_is_kept() {
    // rove omits `name` when it is the class's default; a name it does send
    // is the instance's, even an empty one (a place can have those).
    let text = snapshot_json(
        vec![
            node("scope:workspace", "Workspace", None, json!({})),
            json!({"id": "scope:part", "class_name": "Part", "parent": "scope:workspace", "properties": {}}),
            json!({"id": "scope:named", "class_name": "Part", "name": "", "parent": "scope:workspace", "properties": {}}),
        ],
        None,
    );
    let world = build_text(&text);
    assert_eq!(world.dom.get_by_ref(world.ids["scope:part"]).unwrap().name, "Part");
    assert_eq!(world.dom.get_by_ref(world.ids["scope:named"]).unwrap().name, "");
}

#[test]
fn a_world_with_no_local_player_binds_none() {
    let text = snapshot_json(
        vec![node("scope:workspace", "Workspace", None, json!({}))],
        None,
    );
    let world = build_text(&text);
    assert_eq!(world.local_player, None);
    assert_eq!(world.stats.refs_bound, 0);
    // The report is built by `build()`; with no local player it must not claim
    // one (a report nobody reads is how a bound player silently goes missing).
    let built = world::build(snapshot::parse(&text).expect("parses"), "No Player".into()).expect("builds");
    assert!(!built.report.contains("LocalPlayer:"), "report named a player that is not there");
}

#[test]
fn a_ninety_degree_cframe_survives_a_round_trip_through_the_document() {
    // The convention test, through the whole build rather than the converter.
    let text = snapshot_json(
        vec![
            node("scope:workspace", "Workspace", None, json!({})),
            node(
                "scope:p",
                "Part",
                Some("scope:workspace"),
                json!({"CFrame": {"$cframe": {"position": [0, 5, 0], "rotation": [0,0,1,0,1,0,-1,0,0]}}}),
            ),
        ],
        None,
    );
    let world = build_text(&text);
    let Some(Variant::CFrame(cf)) = world.stored("scope:p", "CFrame") else { panic!() };
    assert_eq!(cf.orientation.x, Vector3::new(0.0, 0.0, -1.0));
    assert_eq!(cf.orientation.z, Vector3::new(1.0, 0.0, 0.0));
    assert_eq!(cf.position, Vector3::new(0.0, 5.0, 0.0));
}
