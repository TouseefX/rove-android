//! Received creation and attribute blobs reach the actual VM in both adapters.
#![cfg(feature = "rust-host")]
use headless_luau::{engine_world_projection::ProjectionContext, engine_world_view::SharedWorldView,
    live_runtime::{compact_delta_with_candidates, compact_snapshot}, runtime_owner::{successful, Runtime}};
use rove_client_core::{runtime_delta::RuntimeDeltaNormalizer, schema::{ClassDesc,Schema},
    session::SessionRecord, world::WorldStore};
use serde_json::{json,Value};

#[test]
fn received_attribute_knowledge_reaches_luau_and_recovers_after_malformed_update() {
    for shared in [false,true] {
        let schema = Schema {version:3,flags:0,enums:vec![],classes:vec![ClassDesc {
            id:0,name:"Part".into(),properties:vec![],events:vec![],isr_properties:vec![],unknown_u16:0}],
            properties:vec![],events:vec![],strings:vec![],names:vec![],lr_types:vec![],decoded_bytes:0,parent_property_id:0};
        let mut snapshot = json!({"version":1,"sequence":0,"nodes":[
            {"id":"game","class_name":"DataModel","name":"game","properties":{}},
            {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}}],
            "services":{"Workspace":"ws"}});
        let mut store = WorldStore::from_snapshot(&snapshot).unwrap();
        let mut normalizer = RuntimeDeltaNormalizer::new(&schema).unwrap();
        let context = ProjectionContext::from_normalizer(&normalizer);
        let view = SharedWorldView::projected(store.view(),context.clone());
        let mut vm = if shared {
            compact_snapshot(&mut snapshot,&view).unwrap();
            Runtime::new_with_shared_world(snapshot,32,&view).unwrap()
        } else {Runtime::new(snapshot,32).unwrap()};
        let cases = [
            (json!({"sequence":1,"kind":"create","id":"p","node":{"id":"p","class_name":"Part","name":"Probe","parent":"ws","received_creation_complete":true,"properties":{}}}),
             "assert(workspace.Probe:GetAttribute('x')==nil); assert(next(workspace.Probe:GetAttributes())==nil)"),
            (json!({"sequence":2,"kind":"set","id":"p","fields":{"AttributesReplicate":{"$bytes_hex":"010000000100000078042a000000"}}}),
             "assert(workspace.Probe:GetAttribute('x')==42)"),
            (json!({"sequence":3,"kind":"set","id":"p","fields":{"AttributesReplicate":{"$bytes_hex":"ff"}}}),
             "assert(not pcall(function() return workspace.Probe:GetAttribute('x') end))"),
            (json!({"sequence":4,"kind":"set","id":"p","fields":{"AttributesReplicate":{"$bytes_hex":""}}}),
             "assert(workspace.Probe:GetAttribute('x')==nil); assert(next(workspace.Probe:GetAttributes())==nil)"),
        ];
        for (action,source) in cases {
            let before = store.view();
            let sequence = action["sequence"].as_u64().unwrap();
            let delta = json!({"version":1,"delta":true,"sequence":sequence,"actions":[action]});
            store.apply_delta(&delta).unwrap();
            let record = SessionRecord {packet_kind:131,world_sequence:sequence,world_delta:delta.into(),
                events:vec![],pings:vec![],tags:vec![],regions:vec![],metadata:vec![],property_changes:vec![]};
            let view = SharedWorldView::projected(store.view(),context.clone());
            let (mut normalized,candidates) = if shared {
                let out = normalizer.normalize_for_shared_view_worker(&before,&store.view(),&record,true).unwrap();
                (out.worker_delta,out.compact_create_ids)
            } else {(normalizer.normalize(&before,&record,true).unwrap().worker_delta,vec![])};
            let reply = if shared {
                compact_delta_with_candidates(&mut normalized,&view,&candidates).unwrap();
                vm.call_with_shared_world(json!({"op":"update","delta":normalized}),&view).unwrap()
            } else {vm.call(json!({"op":"update","delta":normalized})).unwrap()};
            successful(&reply).unwrap();
            let reply:Value = vm.execute_control(format!("{source}; return true"),2000).unwrap();
            successful(&reply).unwrap();
            assert_eq!(reply["values"],json!([true]),"shared={shared}: {reply}");
        }
    }
}
