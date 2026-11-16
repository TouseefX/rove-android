//! Real Luau VM integration for compact Rust-owned received-world nodes.
use headless_luau::{
    engine_world_projection::ProjectionContext,
    engine_world_view::{SharedProjectedWorldView, SharedWorldView},
    runtime_owner::{successful, Runtime},
};
use rove_client_core::{
    runtime_delta::RuntimeDeltaNormalizer,
    runtime_snapshot::{build_runtime_snapshot, RuntimeSnapshotMetadata},
    schema::{ClassDesc, PropertyDesc, Schema},
    session::SessionRecord,
    world::WorldStore,
};
use serde_json::{json, Value};
use std::sync::Arc;

fn schema() -> Schema {
    let classes = [
        "Part",
        "Workspace",
        "Players",
        "RunService",
        "Player",
        "Model",
        "Humanoid",
        "DataModel",
        "SurfaceAppearance",
        "Attachment",
        "Actor",
        "LocalScript",
    ]
    .into_iter()
    .enumerate()
    .map(|(id, name)| ClassDesc {
        id,
        name: name.into(),
        properties: if name == "Part" {
            vec![0, 1, 2]
        } else if name == "Attachment" {
            vec![3,4,5,6]
        } else {
            vec![]
        },
        events: vec![],
        isr_properties: vec![],
        unknown_u16: 0,
    })
    .collect();
    Schema {
        version: 3,
        flags: 0,
        enums: vec![],
        classes,
        properties: vec![
            PropertyDesc {
                id: 0,
                class: 0,
                name: "CFrame".into(),
                type_id: 27,
                enum_id: None,
                lr_type: None,
            },
            PropertyDesc {
                id: 1,
                class: 0,
                name: "size".into(),
                type_id: 22,
                enum_id: None,
                lr_type: None,
            },
            PropertyDesc {
                id: 2,
                class: 0,
                name: "Color3uint8".into(),
                type_id: 20,
                enum_id: None,
                lr_type: None,
            },
            PropertyDesc { id:3, class:9, name:"CFrame".into(),type_id:27,enum_id:None,lr_type:None },
            PropertyDesc { id:4, class:9, name:"Position".into(),type_id:22,enum_id:None,lr_type:None },
            PropertyDesc { id:5, class:9, name:"Axis".into(),type_id:22,enum_id:None,lr_type:None },
            PropertyDesc { id:6, class:9, name:"SecondaryAxis".into(),type_id:22,enum_id:None,lr_type:None },
        ],
        events: vec![],
        strings: vec![],
        names: vec![],
        lr_types: vec![],
        parent_property_id: 3,
        decoded_bytes: 0,
    }
}

fn raw_snapshot() -> Value {
    let mut raw: Value =
        serde_json::from_str(include_str!("../../tests/fixtures/world_snapshot.json")).unwrap();
    for (class, id) in [
        ("Workspace", "scope:workspace"),
        ("Players", "scope:players"),
        ("RunService", "scope:run-service"),
    ] {
        let node = raw["nodes"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|node| node["id"] == id)
            .unwrap();
        node["top_container_flags"] = json!([false, false]);
        assert_eq!(node["class_name"], class);
    }
    let part = raw["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|node| node["id"] == "scope:root-1")
        .unwrap();
    part["name"] = json!("SharedProbe");
    part["parent"] = json!("scope:workspace");
    part["properties"] = json!({
        "Archivable":true,
        "CFrame":{"position":[1.0,2.0,3.0],"rotation_id":2},
        "size":{"$vector3":[4.0,5.0,6.0]},
        "Color3uint8":{"$color3":[0.25,0.5,0.75]}
    });
    part["origins"] = json!({"CFrame":"received","size":"received","Color3uint8":"received"});
    part["property_revisions"] = json!({"CFrame":7,"size":7,"Color3uint8":7});
    // Like the ordinary JSON path, an explicit attribute object is complete
    // unless attributes_known explicitly says otherwise.
    part["attributes"] = json!({"Received":4});
    raw
}

fn compact_shared_part(mut snapshot: Value) -> Value {
    let part = snapshot["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|node| node["id"] == "scope:root-1")
        .unwrap();
    part["properties"] = json!({});
    part["origins"] = json!({});
    part["property_revisions"] = json!({});
    part["_rust_shared_node"] = json!(true);
    for node in snapshot["nodes"].as_array_mut().unwrap() {
        if node["class_name"] == "Attachment" {
            node["properties"]=json!({});node["origins"]=json!({});
            node["property_revisions"]=json!({});node["_rust_shared_node"]=json!(true);
        }
    }
    snapshot
}

#[test]
fn shared_attachment_receipts_preserve_axis_history_and_clone_current_pose() {
    let mut raw=raw_snapshot();
    raw["nodes"].as_array_mut().unwrap().push(json!({
        "id":"scope:attachment","class_name":"Attachment","name":"Point","parent":"scope:root-1",
        "properties":{"Archivable":true,"CFrame":{"position":[1,2,3],"rotation_id":2}},
        "origins":{"CFrame":"received"},"property_revisions":{"CFrame":7}
    }));
    let mut h=Harness::from_raw(raw);
    h.run(r#"
        a=workspace.SharedProbe.Point;expected=Instance.new("Attachment");expected.CFrame=a.CFrame
        a.CFrame=CFrame.new(90,80,70)
        function same() local x,y={a.CFrame:GetComponents()},{expected.CFrame:GetComponents()}
            for i=1,12 do assert(math.abs(x[i]-y[i])<.0001) end
            local c=a:Clone();assert(c.CFrame==a.CFrame)
        end
    "#);
    let steps=[("Axis",[1,2,3]),("SecondaryAxis",[3,-1,1]),("Axis",[1,-2,1]),("Position",[8,9,10])];
    for (i,(key,v)) in steps.into_iter().enumerate() {
        let seq=i as u64+8;
        let (delta,view)=h.apply(json!({"version":1,"delta":true,"sequence":seq,"actions":[{
            "sequence":seq,"kind":"set","id":"scope:attachment","fields":{key:{"$vector3":v}}
        }]}));
        h.update_vm(delta,&view);
        h.run(&format!("expected.{key}=Vector3.new({},{},{});same()",v[0],v[1],v[2]));
    }
    let (delta,view)=h.apply(json!({"version":1,"delta":true,"sequence":12,"actions":[{
        "sequence":12,"kind":"set","id":"scope:attachment","fields":{"CFrame":{"position":[4,5,6],"rotation_id":2}}
    }]}));
    h.update_vm(delta,&view);
    h.run("expected.CFrame=CFrame.new(4,5,6);same()");
}

struct Harness {
    runtime: Runtime,
    store: WorldStore,
    normalizer: RuntimeDeltaNormalizer,
    projection: Arc<ProjectionContext>,
}

impl Harness {
    fn new() -> Self {
        Self::from_raw(raw_snapshot())
    }

    fn from_raw(raw: Value) -> Self {
        let schema = schema();
        let mut normalizer = RuntimeDeltaNormalizer::new(&schema).unwrap();
        let metadata = RuntimeSnapshotMetadata {
            job_id: raw["job_id"].as_str(),
            local_player: raw["local_player"].as_str(),
            initial_replication_complete: Some(true),
            class_parents: Some(&raw["class_parents"]),
            defaults: Some(&raw["defaults"]),
            default_origins: Some(&raw["default_origins"]),
            ..RuntimeSnapshotMetadata::default()
        };
        let mut initial = build_runtime_snapshot(&schema, &raw, &metadata).unwrap();
        if let Some(sources)=raw.get("script_sources") {
            initial["script_sources"]=sources.clone();
        }
        if raw.get("lightweight_assets")==Some(&json!(true)) {
            initial["lightweight_assets"]=json!(true);
        }
        normalizer.bind_snapshot_services(&initial).unwrap();
        let projection = ProjectionContext::from_normalizer(&normalizer);
        let store = WorldStore::from_snapshot(&raw).unwrap();
        let projected = SharedWorldView::projected(store.view(), projection.clone());

        // Compare the projected initial values against the full snapshot
        // normalizer before the VM receives its compact header.
        let expected = initial["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["id"] == "scope:root-1")
            .unwrap();
        let projected_part = projected.lookup_node("scope:root-1").unwrap();
        for key in ["CFrame", "Size", "Position"] {
            assert_eq!(
                projected_part.property(key).unwrap(),
                expected["properties"].get(key).cloned()
            );
        }

        let compact = compact_shared_part(initial);
        let runtime = Runtime::new_with_shared_world(compact, 32, &projected).unwrap();
        // Runtime must own per-node handles after the full projected view drops.
        drop(projected);
        Self {
            runtime,
            store,
            normalizer,
            projection,
        }
    }

    fn apply(&mut self, delta: Value) -> (Value, SharedProjectedWorldView) {
        let sequence = delta["sequence"].as_u64().unwrap();
        let record = SessionRecord {
            packet_kind: 131,
            world_sequence: sequence,
            world_delta: delta.clone().into(),
            events: vec![],
            pings: vec![],
            tags: vec![],
            regions: vec![],
            metadata: vec![],
            property_changes: vec![],
        };
        let normalized = self
            .normalizer
            .normalize(&self.store.view(), &record, true)
            .unwrap();
        self.store.apply_delta(&delta).unwrap();
        let view = SharedWorldView::projected(self.store.view(), self.projection.clone());
        (normalized.worker_delta, view)
    }

    fn update_vm(&mut self, delta: Value, view: &SharedProjectedWorldView) {
        let response = self
            .runtime
            .call_with_shared_world(json!({"op":"update","timeout_ms":500,"delta":delta}), view)
            .unwrap();
        successful(&response)
            .unwrap_or_else(|error| panic!("VM update failed: {error}; {response}"));
    }

    fn run(&mut self, source: &str) -> Value {
        run(&mut self.runtime, source)
    }
}

#[test]
fn lightweight_omitted_asset_reports_error_and_supported_local_assets_remain_readable() {
    let mut raw=raw_snapshot();
    raw["lightweight_assets"]=json!(true);
    raw["nodes"].as_array_mut().unwrap().push(json!({
        "id":"scope:appearance","class_name":"SurfaceAppearance","name":"Visual",
        "parent":"scope:root-1","properties":{},"server_owned":true
    }));
    let mut harness=Harness::from_raw(raw);
    assert_eq!(harness.run(r#"
        local part=workspace:FindFirstChild("SharedProbe")
        local visual=part:FindFirstChild("Visual")
        local ok,message=pcall(function()return visual.ColorMap end)
        assert(not ok and string.find(message,"lightweight asset mode",1,true))
        assert(part.Position.X==1 and part.Size.X==4)
        local image=Instance.new("ImageLabel")
        image.Image="rbxassetid://123"
        assert(image.Image=="rbxassetid://123")
        return true
    "#),json!([true]));
}

#[test]
fn received_collision_alias_preserves_local_override_and_ordered_changes() {
    let mut raw=raw_snapshot();
    let part=raw["nodes"].as_array_mut().unwrap().iter_mut().find(|node|node["id"]=="scope:root-1").unwrap();
    part["properties"]["CollisionGroupReplicate"]=json!({"$bytes_hex":"0147686f7374"});
    part["property_revisions"]["CollisionGroupReplicate"]=json!(7);
    let mut harness=Harness::from_raw(raw);
    assert_eq!(harness.run(r#"
        p=workspace:FindFirstChild("SharedProbe")
        assert(p.CollisionGroup=="Ghost")
        p.CollisionGroup="Default"
        return p.CollisionGroup
    "#), json!(["Default"]));
    let (delta,view)=harness.apply(json!({"version":1,"delta":true,"sequence":8,"actions":[{
        "sequence":8,"kind":"set","id":"scope:root-1","fields":{"CFrame":{"position":[4,5,6],"rotation_id":2}}
    }]}));
    harness.update_vm(delta,&view);
    assert_eq!(harness.run("return p.CollisionGroup"),json!(["Default"]));
    let (delta,view)=harness.apply(json!({"version":1,"delta":true,"sequence":9,"actions":[{
        "sequence":9,"kind":"set","id":"scope:root-1","fields":{"CollisionGroupReplicate":{"$bytes_hex":"024e6577"}}
    }]}));
    harness.update_vm(delta,&view);
    assert_eq!(harness.run("return p.CollisionGroup"),json!(["New"]));
}

#[test]
fn explicit_collision_group_precedes_received_wire_alias() {
    let mut raw=raw_snapshot();
    let part=raw["nodes"].as_array_mut().unwrap().iter_mut().find(|node|node["id"]=="scope:root-1").unwrap();
    part["properties"]["CollisionGroupReplicate"]=json!({"$bytes_hex":"00"});
    part["properties"]["CollisionGroup"]=json!("Explicit");
    let mut harness=Harness::from_raw(raw);
    assert_eq!(harness.run("return workspace:FindFirstChild('SharedProbe').CollisionGroup"),json!(["Explicit"]));
}

#[test]
fn local_color_survives_unrelated_refresh_until_received_color_replaces_it() {
    let mut harness = Harness::new();
    harness.run(
        r#"
        p=workspace:FindFirstChild("SharedProbe")
        p.Color=Color3.new(0.1,0.2,0.3)
    "#,
    );
    let set = json!({"version":1,"delta":true,"sequence":8,"actions":[{
        "sequence":8,"kind":"set","id":"scope:root-1","fields":{
            "CFrame":{"position":[4.0,5.0,6.0],"rotation_id":2}
        }
    }]});
    let (delta, view) = harness.apply(set);
    harness.update_vm(delta, &view);
    assert_eq!(
        harness.run(
            r#"
            assert(math.abs(p.Color.R-0.1)<1e-6)
            assert(math.abs(p.Color.G-0.2)<1e-6)
            assert(math.abs(p.Color.B-0.3)<1e-6)
            return true
        "#
        ),
        json!([true])
    );

    // Preserve the existing received-write boundary: a later authoritative
    // Color update clears the local override, unlike an unrelated refresh.
    let set = json!({"version":1,"delta":true,"sequence":9,"actions":[{
        "sequence":9,"kind":"set","id":"scope:root-1","fields":{
            "Color3uint8":{"$color3":[0.9,0.8,0.7]}
        }
    }]});
    let (delta, view) = harness.apply(set);
    harness.update_vm(delta, &view);
    assert_eq!(
        harness.run(
            r#"
            assert(math.abs(p.Color.R-0.9)<1e-6)
            assert(math.abs(p.Color.G-0.8)<1e-6)
            assert(math.abs(p.Color.B-0.7)<1e-6)
            return true
        "#
        ),
        json!([true])
    );
}

#[test]
fn explicit_received_color_takes_precedence_over_wire_alias() {
    let mut raw = raw_snapshot();
    let part = raw["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|node| node["id"] == "scope:root-1")
        .unwrap();
    part["properties"]["Color"] = json!({"$color3":[0.1,0.2,0.3]});
    part["origins"]["Color"] = json!("received");
    part["property_revisions"]["Color"] = json!(8);
    let mut harness = Harness::from_raw(raw);
    assert_eq!(
        harness.run(
            r#"
            local p=workspace:FindFirstChild("SharedProbe")
            assert(math.abs(p.Color.R-0.1)<1e-6)
            assert(math.abs(p.Color.G-0.2)<1e-6)
            assert(math.abs(p.Color.B-0.3)<1e-6)
            return true
        "#
        ),
        json!([true])
    );
}

#[test]
fn invalid_received_color_alias_is_rejected_during_initial_world_binding() {
    let mut raw = raw_snapshot();
    let part = raw["nodes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|node| node["id"] == "scope:root-1")
        .unwrap();
    // A tagged Color3 remains structurally valid to the projection layer, but
    // this channel value is outside the native Color3uint8 range.
    part["properties"]["Color3uint8"] = json!({"$color3":[1.1,0.5,0.5]});
    let result = std::panic::catch_unwind(|| Harness::from_raw(raw));
    let panic = match result {
        Ok(_) => panic!("invalid derived alias must fail initial binding"),
        Err(error) => error,
    };
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("");
    assert!(
        message.contains("received Color3uint8 component is out of range"),
        "expected initial Color3uint8 range validation error, got: {message}"
    );
}

fn run(runtime: &mut Runtime, source: &str) -> Value {
    let response = runtime
        .call(json!({"op":"execute","source":source,"timeout_ms":500}))
        .unwrap();
    successful(&response).unwrap_or_else(|error| panic!("Luau failed: {error}; {response}"));
    response["values"].clone()
}

#[test]
fn real_vm_reads_projected_received_properties_and_native_color_alias_after_view_drop() {
    let Harness {
        mut runtime,
        store,
        normalizer,
        projection,
    } = Harness::new();
    drop(store);
    drop(normalizer);
    drop(projection);
    assert_eq!(
        run(
            &mut runtime,
            r#"
            local p=workspace:FindFirstChild("SharedProbe")
            assert(p and p.CFrame.Position.X==1)
            assert(p.Size.X==4 and p.Size.Y==5 and p.Size.Z==6)
            assert(p.Position.X==1 and p.Position.Y==2 and p.Position.Z==3)
            assert(math.abs(p.Color.R-0.25)<1e-6)
            assert(math.abs(p.Color.G-0.5)<1e-6)
            assert(math.abs(p.Color.B-0.75)<1e-6)
            assert(p:GetAttributes().Received==4 and p:GetAttribute("Missing")==nil)
            return p.CFrame.Position.X,p.Size.Y,p.Position.Z,p.Color.G
        "#
        ),
        json!([1.0, 5.0, 3.0, 0.5])
    );
}

#[test]
fn local_write_wins_and_clone_keeps_pinned_properties_after_source_update_and_removal() {
    let mut harness = Harness::new();
    harness.run(
        r#"
        p=workspace:FindFirstChild("SharedProbe")
        clone=p:Clone()
        clone.Parent=workspace
        p.Size=Vector3.new(9,8,7)
        p:SetAttribute("LocalOnly",123)
    "#,
    );

    let set = json!({"version":1,"delta":true,"sequence":8,"actions":[{
        "sequence":8,"kind":"set","id":"scope:root-1","fields":{
            "CFrame":{"position":[4.0,5.0,6.0],"rotation_id":2},
            "Color3uint8":{"$color3":[0.9,0.8,0.7]}
        }
    }]});
    let (delta, view) = harness.apply(set);
    harness.update_vm(delta, &view);
    drop(view);
    assert_eq!(
        harness.run(
            r#"
            assert(p.CFrame.Position.X==4)
            assert(p.Position.X==4)
            assert(p.Size.X==9 and p.Size.Y==8 and p.Size.Z==7)
            assert(p:GetAttribute("LocalOnly")==123)
            assert(math.abs(p.Color.R-0.9)<1e-6)
            assert(clone.CFrame.Position.X==1)
            assert(clone.Size.X==4 and clone.Size.Y==5)
            assert(clone:GetAttribute("LocalOnly")==nil)
            assert(math.abs(clone.Color.R-0.25)<1e-6)
            return p.CFrame.Position.X,p.Size.X,clone.CFrame.Position.X,clone.Size.X
        "#
        ),
        json!([4.0, 9.0, 1.0, 4.0])
    );

    let remove = json!({"version":1,"delta":true,"sequence":9,"actions":[{
        "sequence":9,"kind":"remove","id":"scope:root-1"
    }]});
    let (delta, view) = harness.apply(remove);
    harness.update_vm(delta, &view);
    assert_eq!(
        harness.run(
            r#"
            assert(clone.Parent==workspace)
            assert(clone.CFrame.Position.X==1)
            assert(clone.Size.X==4 and math.abs(clone.Color.R-0.25)<1e-6)
            return clone.CFrame.Position.X,clone.Size.X
        "#
        ),
        json!([1.0, 4.0])
    );
}

#[test]
fn same_frame_create_then_set_uses_full_create_and_final_shared_refresh() {
    let mut harness = Harness::new();
    let delta = json!({"version":1,"delta":true,"sequence":9,"actions":[
        {"sequence":8,"kind":"create","node":{
            "id":"created:set","class_name":"Part","name":"CreatedAndSet","parent":"scope:workspace",
            "properties":{"CFrame":{"position":[2.0,0.0,0.0],"rotation_id":2},"size":{"$vector3":[2.0,2.0,2.0]}}
        }},
        {"sequence":9,"kind":"set","id":"created:set","fields":{
            "CFrame":{"position":[6.0,0.0,0.0],"rotation_id":2},"size":{"$vector3":[6.0,7.0,8.0]}
        }}
    ]});
    let (delta, view) = harness.apply(delta);
    harness.update_vm(delta, &view);
    assert_eq!(
        harness.run(
            r#"
            local p=workspace:FindFirstChild("CreatedAndSet")
            assert(p and p.CFrame.Position.X==6)
            assert(p.Position.X==6 and p.Size.X==6 and p.Size.Y==7)
            return p.CFrame.Position.X,p.Position.X,p.Size.Y
        "#
        ),
        json!([6.0, 6.0, 7.0])
    );
}

#[test]
fn shared_wire_aliases_notify_public_properties_after_local_override_removal() {
    let mut raw=raw_snapshot();
    let part=raw["nodes"].as_array_mut().unwrap().iter_mut().find(|node|node["id"]=="scope:root-1").unwrap();
    part["properties"]["CollisionGroupReplicate"]=json!({"$bytes_hex":"00"});
    part["property_revisions"]["CollisionGroupReplicate"]=json!(7);
    let mut h=Harness::from_raw(raw);
    h.run(r#"
        p=workspace.SharedProbe;p.Color=Color3.new(1,0,0);p.CollisionGroup='Local'
        colors=0;groups=0;names={}
        p:GetPropertyChangedSignal('Color'):Connect(function(...) assert(select('#',...)==0);colors+=1 end)
        p:GetPropertyChangedSignal('CollisionGroup'):Connect(function(...) assert(select('#',...)==0);groups+=1 end)
        p.Changed:Connect(function(name) table.insert(names,name) end)
    "#);
    let (delta,view)=h.apply(json!({"version":1,"delta":true,"sequence":8,"actions":[{
        "kind":"set","sequence":8,"id":"scope:root-1","fields":{
            "Color3uint8":{"$color3":[0.25,0.5,0.75]},"CollisionGroupReplicate":{"$bytes_hex":"00"}
        }
    }]}));
    h.update_vm(delta,&view);
    assert_eq!(h.run("return colors,groups,p.Color.R,p.CollisionGroup,table.concat(names,',')"),json!([1.0,1.0,0.25,"Default","BrickColor,CollisionGroup,Color"]));
    let (delta,view)=h.apply(json!({"version":1,"delta":true,"sequence":12,"actions":[
        {"kind":"set","sequence":9,"id":"scope:root-1","fields":{"Color3uint8":{"$color3":[1,0,0]},"CollisionGroupReplicate":{"$bytes_hex":"0147686f7374"}}},
        {"kind":"set","sequence":10,"id":"scope:root-1","fields":{"CollisionGroupReplicate":{"$bytes_hex":"0247686f7374"}}},
        {"kind":"set","sequence":11,"id":"scope:root-1","fields":{"Color3uint8":{"$color3":[0.25,0.5,0.75]}}},
        {"kind":"set","sequence":12,"id":"scope:root-1","fields":{"CollisionGroupReplicate":{"$bytes_hex":"00"}}}
    ]}));
    h.update_vm(delta.clone(),&view);
    assert_eq!(h.run("return colors,groups,p.Color.R,p.CollisionGroup"),json!([3.0,3.0,0.25,"Default"]));
    h.update_vm(delta,&view);
    assert_eq!(h.run("return colors,groups"),json!([3.0,3.0]));
}

#[test]
fn shared_transform_receipts_preserve_intermediate_position_notifications() {
    let mut h=Harness::new();
    h.run(r#"
        moves=0;turns=0
        workspace.SharedProbe:GetPropertyChangedSignal('Position'):Connect(function(...) assert(select('#',...)==0);moves+=1 end)
        workspace.SharedProbe:GetPropertyChangedSignal('Orientation'):Connect(function() turns+=1 end)
    "#);
    let (delta,view)=h.apply(json!({"version":1,"delta":true,"sequence":10,"actions":[
        {"kind":"set","id":"scope:root-1","sequence":8,"fields":{"CFrame":{"position":[4,5,6],"rotation_id":2}}},
        {"kind":"set","id":"scope:root-1","sequence":9,"fields":{"CFrame":{"position":[4,5,6],"rotation_id":2}}},
        {"kind":"set","id":"scope:root-1","sequence":10,"fields":{"CFrame":{"position":[1,2,3],"rotation_id":2}}}
    ]}));
    h.update_vm(delta.clone(),&view);
    assert_eq!(h.run("return moves,turns,workspace.SharedProbe.Position.X"),json!([2.,0.,1.]));
    h.update_vm(delta,&view);
    assert_eq!(h.run("return moves,turns"),json!([2.,0.]));
}

#[test]
fn shared_received_pose_and_color_replace_actors_old_script_overlays() {
    let mut raw=raw_snapshot();
    raw["nodes"].as_array_mut().unwrap().extend([
        json!({"id":"scope:actor","class_name":"Actor","name":"Worker","parent":"scope:workspace","properties":{}}),
        json!({"id":"scope:reader","class_name":"LocalScript","name":"Reader","parent":"scope:actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}})
    ]);
    raw["script_sources"]=json!({"scope:reader":{"source":r#"
        local p=workspace.SharedProbe
        script.Parent:BindToMessage('sample',function()
            script:SetAttribute('X',p.Position.X);script:SetAttribute('Red',p.Color.R)
        end)
    "#}});
    let mut h=Harness::from_raw(raw);
    h.run("p=workspace.SharedProbe;p.CFrame=CFrame.new(50,0,0);p.Color=Color3.new(1,0,0)");
    h.run("workspace.Worker:SendMessage('sample')");
    assert_eq!(h.run("return workspace.Worker.Reader:GetAttribute('X'),workspace.Worker.Reader:GetAttribute('Red')"),json!([50.,1.]));
    let (delta,view)=h.apply(json!({"version":1,"delta":true,"sequence":8,"actions":[{
        "kind":"set","sequence":8,"id":"scope:root-1","fields":{
            "CFrame":{"position":[4,5,6],"rotation_id":2},"Color3uint8":{"$color3":[0.25,0.5,0.75]}
        }
    }]}));
    h.update_vm(delta,&view);
    h.run("workspace.Worker:SendMessage('sample')");
    assert_eq!(h.run("return p.Position.X,p.Color.R,workspace.Worker.Reader:GetAttribute('X'),workspace.Worker.Reader:GetAttribute('Red')"),json!([4.,0.25,4.,0.25]));
}

#[test]
fn shared_owned_body_feedback_reads_received_authority_and_stops_after_owner_changes() {
    let mut raw=raw_snapshot();
    let part=raw["nodes"].as_array_mut().unwrap().iter_mut().find(|n|n["id"]=="scope:root-1").unwrap();
    part["properties"]["Anchored"]=json!(false);
    part["properties"]["NetworkOwnerV3"]=json!(7);
    part["property_revisions"]["Anchored"]=json!(7);
    part["property_revisions"]["NetworkOwnerV3"]=json!(7);
    let mut h=Harness::from_raw(raw);
    h.run("changes=0;workspace.SharedProbe:GetPropertyChangedSignal('Position'):Connect(function() changes+=1 end)");
    let feedback=|sequence:u64,x:i32|json!({"op":"update","timeout_ms":100,"delta":{
        "version":1,"delta":true,"sequence":sequence,"actions":[],"engine_owned_part_updates":{
            "scope":7,"source_revision":sequence,"frames":[{"id":"scope:root-1","cframe":{"$cframe":{"position":[x,2,3],"rotation":[1,0,0,0,1,0,0,0,1]}},"linear_velocity":[3,0,0],"angular_velocity":[0,0,0]}]
        }
    }});
    let reply=h.runtime.call(feedback(7,5)).unwrap();successful(&reply).unwrap();
    assert_eq!(reply["engine_owned_part_status"],"applied","{reply}");
    assert_eq!(h.run("return workspace.SharedProbe.Position.X,workspace.SharedProbe.AssemblyLinearVelocity.X,changes"),json!([5.,3.,0.]));
    let (delta,view)=h.apply(json!({"version":1,"delta":true,"sequence":8,"actions":[{"kind":"set","sequence":8,"id":"scope:root-1","fields":{"NetworkOwnerV3":8}}]}));
    h.update_vm(delta,&view);
    let reply=h.runtime.call(feedback(8,20)).unwrap();successful(&reply).unwrap();
    assert_eq!(reply["engine_owned_part_status"],"stale_authority","{reply}");
    assert_eq!(h.run("return workspace.SharedProbe.Position.X,changes"),json!([5.,0.]));
}

#[test]
fn shared_point_velocity_reads_current_received_assembly_motion() {
    let mut raw=raw_snapshot();
    let part=raw["nodes"].as_array_mut().unwrap().iter_mut().find(|n|n["id"]=="scope:root-1").unwrap();
    part["properties"]["AssemblyLinearVelocity"]=json!({"$vector3":[1,0,0]});
    part["properties"]["AssemblyAngularVelocity"]=json!({"$vector3":[0,0,2]});
    part["properties"]["AssemblyCenterOfMass"]=json!({"$vector3":[1,2,3]});
    let mut h=Harness::from_raw(raw);
    assert_eq!(h.run("local v=workspace.SharedProbe:GetVelocityAtPosition(Vector3.new(3,2,3));return v.X,v.Y,v.Z"),json!([1.,4.,0.]));
    let (delta,view)=h.apply(json!({"version":1,"delta":true,"sequence":8,"actions":[{"kind":"set","sequence":8,"id":"scope:root-1","fields":{"AssemblyAngularVelocity":{"$vector3":[0,0,0]}}}]}));
    h.update_vm(delta,&view);
    assert_eq!(h.run("local v=workspace.SharedProbe:GetVelocityAtPosition(Vector3.new(3,2,3));return v.X,v.Y,v.Z"),json!([1.,0.,0.]));
}

#[test]
fn shared_grounding_reads_new_received_anchoring() {
    let mut raw=raw_snapshot();
    let part=raw["nodes"].as_array_mut().unwrap().iter_mut().find(|n|n["id"]=="scope:root-1").unwrap();
    part["properties"]["Anchored"]=json!(false);
    let mut h=Harness::from_raw(raw);
    assert_eq!(h.run("return workspace.SharedProbe:IsGrounded()"),json!([false]));
    let (delta,view)=h.apply(json!({"version":1,"delta":true,"sequence":8,"actions":[{"kind":"set","sequence":8,"id":"scope:root-1","fields":{"Anchored":true}}]}));
    h.update_vm(delta,&view);
    assert_eq!(h.run("return workspace.SharedProbe:IsGrounded()"),json!([true]));
}

#[test]
fn shared_humanoid_health_preserves_payloads_and_does_not_author_mutations() {
    let mut raw=raw_snapshot();
    raw["nodes"].as_array_mut().unwrap().push(json!({
        "id":"scope:health-probe","class_name":"Humanoid","name":"HealthProbe","parent":"scope:workspace",
        "properties":{"Health":100,"MaxHealth":100},"property_revisions":{"Health":7,"MaxHealth":7}
    }));
    let mut h=Harness::from_raw(raw);
    h.run(r#"
        health={};deaths=0
        workspace.HealthProbe.HealthChanged:Connect(function(value) table.insert(health,value) end)
        workspace.HealthProbe.Died:Connect(function() deaths+=1 end)
    "#);
    let (delta,view)=h.apply(json!({"version":1,"delta":true,"sequence":9,"actions":[
        {"kind":"set","id":"scope:health-probe","sequence":8,"fields":{"Health":60}},
        {"kind":"set","id":"scope:health-probe","sequence":9,"fields":{"Health":0}}
    ]}));
    let reply=h.runtime.call_with_shared_world(json!({"op":"update","timeout_ms":500,"delta":delta}),&view).unwrap();
    successful(&reply).unwrap();
    assert!(reply["mutations"].as_array().is_none_or(|rows|rows.is_empty()),"{reply}");
    assert_eq!(h.run("return table.concat(health,','),deaths,workspace.HealthProbe.Health"),json!(["60,0",1.,0.]));
}

#[test]
fn shared_humanoid_state_updates_keep_intermediate_enum_payloads() {
    let mut raw=raw_snapshot();
    raw["nodes"].as_array_mut().unwrap().push(json!({
        "id":"scope:state-probe","class_name":"Humanoid","name":"StateProbe","parent":"scope:workspace",
        "properties":{"HumanoidStateType":8},"property_revisions":{"HumanoidStateType":7}
    }));
    let mut h=Harness::from_raw(raw);
    h.run(r#"
        states={}
        workspace.StateProbe.StateChanged:Connect(function(old,new)
            assert(old==Enum.HumanoidStateType[old.Name] and new==Enum.HumanoidStateType[new.Name])
            table.insert(states,old.Name..'>'..new.Name)
        end)
    "#);
    let (delta,view)=h.apply(json!({"version":1,"delta":true,"sequence":9,"actions":[
        {"kind":"set","id":"scope:state-probe","sequence":8,"fields":{"HumanoidStateType":3}},
        {"kind":"set","id":"scope:state-probe","sequence":9,"fields":{"HumanoidStateType":8}}
    ]}));
    h.update_vm(delta.clone(),&view);
    assert_eq!(h.run("return table.concat(states,','),workspace.StateProbe:GetState()==Enum.HumanoidStateType.Running"),json!(["Running>Jumping,Jumping>Running",true]));
    h.update_vm(delta,&view);
    assert_eq!(h.run("return #states"),json!([2.]));
}
