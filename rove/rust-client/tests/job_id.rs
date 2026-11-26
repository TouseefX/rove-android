//! Session identity exposed through the real Luau VM, including compact worlds.
#![cfg(feature = "rust-host")]

use headless_luau::{
    engine_world_projection::ProjectionContext,
    engine_world_view::SharedWorldView,
    live_runtime::compact_snapshot,
    runtime_owner::{successful, Runtime},
};
use rove_client_core::{schema::Schema, world::WorldStore};
use serde_json::{json, Value};
use std::collections::HashMap;

const FIRST: &str = "12345678-1234-4234-9234-123456789abc";
const SECOND: &str = "87654321-4321-4321-8321-cba987654321";

fn snapshot(job_id: &str) -> Value {
    json!({"version":1,"sequence":0,"job_id":job_id,"services":{"Workspace":"workspace"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
        {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}}
    ]})
}

fn run(runtime: &mut Runtime, source: &str) -> Value {
    let response = runtime
        .call(json!({"op":"execute","source":source,"timeout_ms":100}))
        .unwrap();
    successful(&response).unwrap_or_else(|error| panic!("{error}: {response}"));
    response["values"].clone()
}

#[test]
fn job_id_comes_from_each_session_and_is_read_only() {
    let mut a = Runtime::new(snapshot(FIRST), 32).unwrap();
    let mut b = Runtime::new(snapshot(SECOND), 32).unwrap();
    for (runtime, expected) in [(&mut a, FIRST), (&mut b, SECOND)] {
        assert_eq!(
            run(runtime, "return game.JobId, typeof(game.JobId)"),
            json!([expected, "string"])
        );
        assert_eq!(
            run(
                runtime,
                r#"
            local ok, message = pcall(function() game.JobId = "changed" end)
            return ok, string.find(message, "read-only", 1, true) ~= nil, game.JobId
        "#
            ),
            json!([false, true, expected])
        );
        assert_eq!(
            run(
                runtime,
                "return pcall(function() return workspace.JobId end)"
            )[0],
            false
        );
    }
    assert_eq!(run(&mut a, "return game.JobId"), json!([FIRST]));
}

#[test]
fn shared_world_job_id_survives_updates_without_a_replicated_property() {
    let mut snapshot = snapshot(FIRST);
    let mut store = WorldStore::from_snapshot(&snapshot).unwrap();
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
    let projection = ProjectionContext::new(&schema, HashMap::new()).unwrap();
    let view = SharedWorldView::projected(store.view(), projection.clone());
    compact_snapshot(&mut snapshot, &view).unwrap();
    let mut runtime = Runtime::new_with_shared_world(snapshot, 32, &view).unwrap();
    assert_eq!(run(&mut runtime, "return game.JobId"), json!([FIRST]));

    let delta = json!({"version":1,"delta":true,"sequence":1,"actions":[
        {"sequence":1,"kind":"set","id":"workspace","fields":{"Gravity":150.0}}
    ]});
    store.apply_delta(&delta).unwrap();
    let view = SharedWorldView::projected(store.view(), projection);
    let response = runtime
        .call_with_shared_world(json!({"op":"update","delta":delta,"timeout_ms":100}), &view)
        .unwrap();
    successful(&response).unwrap();
    assert_eq!(
        run(&mut runtime, "return game.JobId, workspace.Gravity"),
        json!([FIRST, 150.0])
    );
}

#[test]
fn session_metadata_wins_and_explicit_offline_properties_remain_supported() {
    let mut value = snapshot(FIRST);
    value["nodes"][0]["properties"]["JobId"] = json!(SECOND);
    let mut runtime = Runtime::new(value.clone(), 32).unwrap();
    assert_eq!(run(&mut runtime, "return game.JobId"), json!([FIRST]));

    value.as_object_mut().unwrap().remove("job_id");
    let mut runtime = Runtime::new(value.clone(), 32).unwrap();
    assert_eq!(run(&mut runtime, "return game.JobId"), json!([SECOND]));
    assert_eq!(
        run(
            &mut runtime,
            "return pcall(function() game.JobId='changed' end)"
        )[0],
        false
    );

    value["nodes"][0]["properties"] = json!({});
    let mut runtime = Runtime::new(value, 32).unwrap();
    assert_eq!(
        run(
            &mut runtime,
            "return pcall(function() return game.JobId end)"
        )[0],
        false
    );
}
