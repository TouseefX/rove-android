//! Received movement -> world delta -> official Luau VM, without a server.
use headless_luau::{
    engine_world_projection::ProjectionContext,
    engine_world_view::SharedWorldView,
    live_runtime::{compact_delta, compact_snapshot},
    runtime_owner::{successful, Runtime},
};
use rove_client_core::{
    instance_state::decode_packet,
    runtime_delta::RuntimeDeltaNormalizer,
    runtime_snapshot::{build_runtime_snapshot, RuntimeSnapshotMetadata},
    schema::{ClassDesc, PropertyDesc, Schema},
    session::SessionRecord,
    world::WorldStore,
};
use serde_json::{json, Value};
use std::{thread, time::Duration};

fn schema() -> Schema {
    Schema {
        version: 3,
        flags: 0,
        enums: vec![],
        classes: [
            "Part",
            "Player",
            "Model",
            "Players",
            "Workspace",
            "DataModel",
        ]
        .into_iter()
        .enumerate()
        .map(|(id, name)| ClassDesc {
            id,
            name: name.into(),
            properties: vec![],
            events: vec![],
            isr_properties: if id == 0 { vec![0] } else { vec![] },
            unknown_u16: 0,
        })
        .collect(),
        properties: vec![PropertyDesc {
            id: 0,
            class: 0,
            name: "ReplicationPV".into(),
            type_id: 57,
            enum_id: None,
            lr_type: None,
        }],
        events: vec![],
        strings: vec![],
        names: vec![],
        lr_types: vec![],
        parent_property_id: 1,
        decoded_bytes: 0,
    }
}

fn raw() -> Value {
    let mut nodes = vec![
        json!({"id":"game","class_name":"DataModel","name":"Game","properties":{}}),
        json!({"id":"players","class_name":"Players","name":"Players","parent":"game","top_container_flags":[false,false],"properties":{}}),
        json!({"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"game","top_container_flags":[false,false],"properties":{}}),
    ];
    for n in 0..2 {
        nodes.extend([
            json!({"id":format!("player{n}"),"class_name":"Player","name":format!("Player{n}"),"parent":"players","properties":{"Character":{"$ref":format!("character{n}")}}}),
            json!({"id":format!("character{n}"),"class_name":"Model","name":format!("Character{n}"),"parent":"workspace","properties":{"PrimaryPart":{"$ref":format!("1:{}",42+n)}}}),
            json!({"id":format!("1:{}",42+n),"class_name":"Part","name":"HumanoidRootPart","parent":format!("character{n}"),"properties":{
                "ReplicationPV":{"position":[n,0,0],"rotation":[1,0,0,0,1,0,0,0,1],"decoded_velocities":[[0,0,0],[0,0,0]]}
            },"property_revisions":{"ReplicationPV":1},"server_owned":true}),
        ]);
    }
    json!({"version":1,"sequence":1,"nodes":nodes})
}

fn packet(cases: &[Value], swapped: bool) -> Vec<u8> {
    let mut out = vec![161];
    out.extend(100u32.to_le_bytes());
    out.extend(2u32.to_le_bytes());
    out.extend(0i32.to_le_bytes());
    for n in 0..2 {
        let case = &cases[n ^ usize::from(swapped)];
        let hex = case["body_hex"].as_str().unwrap();
        let body: Vec<_> = hex
            .as_bytes()
            .chunks_exact(2)
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect();
        out.extend(1u16.to_le_bytes());
        out.extend([case["lod"].as_u64().unwrap() as u8, 0]);
        out.push(1);
        out.extend((42 + n as u32).to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(1u16.to_le_bytes());
        out.extend(1u64.to_le_bytes());
        out.extend((body.len() as u16).to_le_bytes());
        out.extend(body);
    }
    out
}

#[test]
fn received_movement_updates_distinct_player_roots_across_luau_yields() {
    let fixtures: Value =
        serde_json::from_str(include_str!("../../client-core/tests/fixtures/isr-pv.json")).unwrap();
    let cases = fixtures["cases"].as_array().unwrap();
    // Both VM transport modes must expose the same values and notifications.
    for shared in [false, true] {
        let schema = schema();
        let raw = raw();
        let mut store = WorldStore::from_snapshot(&raw).unwrap();
        let mut normalizer = RuntimeDeltaNormalizer::new(&schema).unwrap();
        let mut snapshot =
            build_runtime_snapshot(&schema, &raw, &RuntimeSnapshotMetadata::default()).unwrap();
        normalizer.bind_snapshot_services(&snapshot).unwrap();
        let projection = ProjectionContext::from_normalizer(&normalizer);
        let view = SharedWorldView::projected(store.view(), projection.clone());
        let mut vm = if shared {
            compact_snapshot(&mut snapshot, &view).unwrap();
            Runtime::new_with_shared_world(snapshot, 32, &view).unwrap()
        } else {
            Runtime::new(snapshot, 32).unwrap()
        };
        let first = vm.call(json!({"op":"execute","timeout_ms":50,"source":r#"
            local players=game:GetService('Players'):GetPlayers()
            table.sort(players,function(a,b) return a.Name<b.Name end)
            assert(#players==2)
            local roots={}
            changes={0,0}; velocities={0,0}
            for n,p in ipairs(players) do
                roots[n]=p.Character:FindFirstChild('HumanoidRootPart')
                roots[n]:GetPropertyChangedSignal('Position'):Connect(function() changes[n]+=1 end)
                roots[n]:GetPropertyChangedSignal('AssemblyLinearVelocity'):Connect(function() velocities[n]+=1 end)
            end
            assert(roots[1]~=roots[2])
            roots[1].Position=Vector3.new(999,999,999)
            local out={}
            for i=1,3 do
                local row={}
                for n,p in ipairs(players) do
                    local r=p.Character:FindFirstChild('HumanoidRootPart')
                    assert(r==roots[n] and r.Position==r.CFrame.Position)
                    local pos,vel=r.Position,r.AssemblyLinearVelocity
                    assert(r.Velocity==vel and r.RotVelocity==r.AssemblyAngularVelocity)
                    row[n]={pos.X,pos.Y,pos.Z,vel.X,vel.Y,vel.Z}
                end
                out[i]=row
                if i<3 then task.wait(0.01) end
            end
            local flat={}
            for _,row in ipairs(out) do for _,values in ipairs(row) do
                for _,value in ipairs(values) do table.insert(flat,value) end
            end end
            return table.unpack(flat)
        "#})).unwrap();
        successful(&first).unwrap();
        assert_eq!(first["completed"], false, "{first}");
        let job = first["job_id"].as_u64().unwrap();
        let mut result = None;
        for step in 0..2 {
            let decoded = decode_packet(&packet(cases, step == 1), &schema, true).unwrap();
            let sequence = store.sequence();
            let actions: Vec<_> = decoded.records.into_iter().enumerate().map(|(n, record)|
                json!({"kind":"set","id":record.id.unwrap(),"sequence":sequence+n as u64+1,"fields":record.fields})).collect();
            let delta = json!({"version":1,"delta":true,"sequence":sequence+2,"actions":actions});
            let before = store.view();
            store.apply_delta(&delta).unwrap();
            let record = SessionRecord {
                packet_kind: 161,
                world_sequence: sequence + 2,
                world_delta: delta.into(),
                events: vec![],
                pings: vec![],
                tags: vec![],
                regions: vec![],
                metadata: vec![],
                property_changes: vec![],
            };
            let mut normalized = normalizer
                .normalize_for_shared_view_worker(&before, &store.view(), &record, true)
                .unwrap()
                .worker_delta;
            let view = SharedWorldView::projected(store.view(), projection.clone());
            let root = view.lookup_node("1:42").unwrap();
            let full = root.section("properties").unwrap();
            if let rove_client_core::world::ReadOnlyField::Value(Value::Object(properties)) = full {
                assert_eq!(
                    root.section_len("properties").unwrap(),
                    Some(properties.len())
                );
            } else {
                panic!("missing projected properties");
            }
            thread::sleep(Duration::from_millis(20));
            let reply = if shared {
                compact_delta(&mut normalized, &view).unwrap();
                vm.call_with_shared_world(
                    json!({"op":"update","timeout_ms":50,"delta":normalized}),
                    &view,
                )
                .unwrap()
            } else {
                vm.call(json!({"op":"update","timeout_ms":50,"delta":normalized}))
                    .unwrap()
            };
            successful(&reply).unwrap();
            if let Some(jobs) = reply["completed_jobs"].as_array() {
                result = jobs
                    .iter()
                    .find(|j| j["job_id"].as_u64() == Some(job))
                    .cloned()
                    .or(result);
            }
        }
        let result = result.expect("sampler must finish after two received updates");
        assert_eq!(result["ok"], true, "shared={shared}: {result}");
        assert_eq!(result["values"].as_array().unwrap().len(), 36);
        for step in 0..2 {
            for n in 0..2 {
                let expected = &cases[n ^ step];
                let actual = &result["values"].as_array().unwrap()[(step + 1) * 12 + n * 6..];
                for axis in 0..3 {
                    assert!(
                        (actual[axis].as_f64().unwrap()
                            - expected["position"][axis].as_f64().unwrap())
                        .abs()
                            < 0.001,
                        "shared={shared}: {result}"
                    );
                    assert!(
                        (actual[axis + 3].as_f64().unwrap()
                            - expected["velocities"][0][axis].as_f64().unwrap())
                        .abs()
                            < 0.001
                    );
                }
            }
        }
        let signals = vm
            .execute_control(
                "return changes[1],changes[2],velocities[1],velocities[2]".into(),
                50,
            )
            .unwrap();
        assert_eq!(signals["values"], json!([1., 0., 0., 0.]), "{signals}");
    }
}

struct Harness {
    vm: Runtime,
    store: WorldStore,
    normalizer: RuntimeDeltaNormalizer,
    projection: std::sync::Arc<ProjectionContext>,
    shared: bool,
}
fn checked(reply: Value) -> Value {
    successful(&reply).unwrap_or_else(|e| panic!("{e}: {reply}"));
    assert!(
        reply["task_errors"].as_array().is_none_or(|e| e.is_empty()),
        "{reply}"
    );
    for actor in reply["actor_runtime"].as_array().into_iter().flatten() {
        assert!(
            actor["task_errors"].as_array().is_none_or(|e| e.is_empty()),
            "{actor}"
        );
        assert!(actor.get("shadow_merge_error").is_none(), "{actor}");
        assert!(
            actor["shadow_sync"]["conflicts"]
                .as_array()
                .is_none_or(|e| e.is_empty()),
            "{actor}"
        );
    }
    reply
}
impl Harness {
    fn new(shared: bool) -> Self {
        let schema = schema();
        let mut raw = raw();
        raw["nodes"].as_array_mut().unwrap().extend([
            json!({"id":"actor","class_name":"Actor","name":"Worker","parent":"workspace"}),
            json!({"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}}),
        ]);
        let store = WorldStore::from_snapshot(&raw).unwrap();
        let mut normalizer = RuntimeDeltaNormalizer::new(&schema).unwrap();
        let mut snapshot =
            build_runtime_snapshot(&schema, &raw, &RuntimeSnapshotMetadata::default()).unwrap();
        normalizer.bind_snapshot_services(&snapshot).unwrap();
        snapshot["script_sources"] = json!({"reader":{"source":r#"
            local p=workspace.Character0.HumanoidRootPart
            local changed,positions,velocities=0,0,0
            p.Changed:Connect(function() changed+=1 end)
            p:GetPropertyChangedSignal('Position'):Connect(function() positions+=1 end)
            p:GetPropertyChangedSignal('AssemblyLinearVelocity'):Connect(function() velocities+=1 end)
            script.Parent:BindToMessage('write',function() p.CFrame=CFrame.new(999,0,0) end)
            script.Parent:BindToMessage('reset',function() changed=0;positions=0;velocities=0 end)
            script.Parent:BindToMessage('sample',function()
                script:SetAttribute('X',p.Position.X)
                script:SetAttribute('V',p.AssemblyLinearVelocity.X)
                script:SetAttribute('ChangedCount',changed)
                script:SetAttribute('Positions',positions)
                script:SetAttribute('Velocities',velocities)
            end)
        "#}});
        let projection = ProjectionContext::from_normalizer(&normalizer);
        let view = SharedWorldView::projected(store.view(), projection.clone());
        let vm = if shared {
            compact_snapshot(&mut snapshot, &view).unwrap();
            Runtime::new_with_shared_world(snapshot, 32, &view).unwrap()
        } else {
            Runtime::new(snapshot, 32).unwrap()
        };
        let mut h = Self {
            vm,
            store,
            normalizer,
            projection,
            shared,
        };
        h.execute(r#"
            p=workspace.Character0.HumanoidRootPart;a=workspace.Worker.Reader
            changed=0;positions=0;velocities=0
            p.Changed:Connect(function() changed+=1 end)
            p:GetPropertyChangedSignal('Position'):Connect(function() positions+=1 end)
            p:GetPropertyChangedSignal('AssemblyLinearVelocity'):Connect(function() velocities+=1 end)
        "#);
        h.settle();
        h
    }
    fn execute(&mut self, source: &str) -> Value {
        checked(
            self.vm
                .call(json!({"op":"execute","source":source,"timeout_ms":100}))
                .unwrap(),
        )["values"]
            .clone()
    }
    fn settle(&mut self) {
        for _ in 0..4 {
            checked(self.vm.call(json!({"op":"poll","timeout_ms":100})).unwrap());
        }
    }
    fn receive(&mut self, fields: Vec<Value>) {
        let sequence = self.store.sequence();
        let actions: Vec<_> = fields
            .into_iter()
            .enumerate()
            .map(|(n, fields)| {
                json!({
                    "kind":"set","id":"1:42","sequence":sequence+n as u64+1,"fields":fields
                })
            })
            .collect();
        let sequence = sequence + actions.len() as u64;
        let delta = json!({"version":1,"delta":true,"sequence":sequence,"actions":actions});
        let before = self.store.view();
        self.store.apply_delta(&delta).unwrap();
        let record = SessionRecord {
            packet_kind: 161,
            world_sequence: sequence,
            world_delta: delta.into(),
            events: vec![],
            pings: vec![],
            tags: vec![],
            regions: vec![],
            metadata: vec![],
            property_changes: vec![],
        };
        let mut normalized = self
            .normalizer
            .normalize_for_shared_view_worker(&before, &self.store.view(), &record, true)
            .unwrap()
            .worker_delta;
        let view = SharedWorldView::projected(self.store.view(), self.projection.clone());
        let reply = if self.shared {
            compact_delta(&mut normalized, &view).unwrap();
            self.vm
                .call_with_shared_world(
                    json!({"op":"update","timeout_ms":100,"delta":normalized}),
                    &view,
                )
                .unwrap()
        } else {
            self.vm
                .call(json!({"op":"update","timeout_ms":100,"delta":normalized}))
                .unwrap()
        };
        checked(reply);
        self.settle();
    }
    fn sample(&mut self) -> Value {
        self.execute("workspace.Worker:SendMessage('sample')");
        self.settle();
        self.execute("return p.Position.X,p.AssemblyLinearVelocity.X,changed,positions,velocities,a:GetAttribute('X'),a:GetAttribute('V'),a:GetAttribute('ChangedCount'),a:GetAttribute('Positions'),a:GetAttribute('Velocities')")
    }
}
fn pv(x: i32, speed: i32) -> Value {
    json!({"position":[x,0,0],"rotation":[1,0,0,0,1,0,0,0,1],"decoded_velocities":[[speed,0,0],[0,speed,0]]})
}
fn frame(x: i32) -> Value {
    json!({"$cframe":{"position":[x,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}})
}
#[test]
fn physics_receipts_update_actor_reads_and_keep_unrelated_property_notifications() {
    for shared in [false, true] {
        let mut h = Harness::new(shared);
        h.execute("workspace.Worker:SendMessage('write')");
        h.settle();
        assert_eq!(h.execute("return p.Position.X"), json!([999.]));
        h.execute("changed=0;positions=0;velocities=0;workspace.Worker:SendMessage('reset')");
        h.settle();
        h.receive(vec![json!({"ReplicationPV":pv(10,3),"Transparency":0.5})]);
        assert_eq!(
            h.sample(),
            json!([10., 3., 1., 0., 0., 10., 3., 1., 0., 0.]),
            "shared={shared}"
        );
        h.receive(vec![json!({"ReplicationPV":pv(20,4)})]);
        assert_eq!(
            h.sample(),
            json!([20., 4., 1., 0., 0., 20., 4., 1., 0., 0.]),
            "shared={shared}"
        );
        // An explicit frame and velocity win over projections in the same row.
        h.receive(vec![json!({"ReplicationPV":pv(30,5),"CFrame":frame(25),"AssemblyLinearVelocity":{"$vector3":[7,0,0]}})]);
        h.sample();
        assert_eq!(h.execute("return p.Position.X,p.AssemblyLinearVelocity.X,positions,velocities,a:GetAttribute('X'),a:GetAttribute('V'),a:GetAttribute('Positions'),a:GetAttribute('Velocities')"),json!([25.,7.,1.,1.,25.,7.,1.,1.]),"shared={shared}");
        h.execute("p.Position=Vector3.new(40,0,0)");
        h.settle();
        h.sample();
        assert_eq!(
            h.execute(
                "return p.Position.X,positions,a:GetAttribute('X'),a:GetAttribute('Positions')"
            ),
            json!([40., 2., 40., 2.]),
            "shared={shared}"
        );
    }
}
#[test]
fn silent_physics_receipts_advance_comparisons_for_later_explicit_edits() {
    for shared in [false, true] {
        let mut h = Harness::new(shared);
        h.receive(vec![
            json!({"ReplicationPV":pv(10,3)}),
            json!({"CFrame":frame(10),"AssemblyLinearVelocity":{"$vector3":[3,0,0]}}),
        ]);
        h.sample();
        assert_eq!(h.execute("return p.Position.X,positions,velocities,a:GetAttribute('X'),a:GetAttribute('Positions'),a:GetAttribute('Velocities'),changed,a:GetAttribute('ChangedCount')"),json!([10.,0.,0.,10.,0.,0.,0.,0.]),"shared={shared}");
        // The final received pose is different, so exactly one explicit edit fires.
        h.receive(vec![
            json!({"ReplicationPV":pv(20,4)}),
            json!({"CFrame":frame(10),"AssemblyLinearVelocity":{"$vector3":[3,0,0]}}),
        ]);
        h.sample();
        assert_eq!(h.execute("return p.Position.X,positions,velocities,a:GetAttribute('X'),a:GetAttribute('Positions'),a:GetAttribute('Velocities'),changed,a:GetAttribute('ChangedCount')"),json!([10.,1.,1.,10.,1.,1.,3.,3.]),"shared={shared}");
    }
}
