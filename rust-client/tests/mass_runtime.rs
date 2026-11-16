//! Mass queries over received primitive inputs and local physical edits.
#![cfg(feature = "rust-host")]
use headless_luau::{
    engine_world_projection::ProjectionContext,
    engine_world_view::SharedWorldView,
    live_runtime::{compact_delta, compact_snapshot},
    runtime_owner::{successful, Runtime},
};
use rove_client_core::{schema::Schema, world::WorldStore};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};
fn call(vm: &mut Runtime, request: Value) -> Value {
    let reply = vm.call(request).unwrap();
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
fn frame(x: f64) -> Value {
    json!({"$cframe":{"position":[x,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}})
}
fn properties() -> Value {
    json!({"Size":{"$vector3":[2,2,2]},"Shape":{"$enum":{"type":"PartType","value":1}},"CustomPhysicalProperties":{"$physical_properties":[1,0.3,0.5,1,1]},"Anchored":false,"Massless":false,"RootPriority":0,"CFrame":frame(0.)})
}
fn part(id: &str, x: f64) -> Value {
    let mut p = properties();
    p["CFrame"] = frame(x);
    json!({"id":id,"class_name":"Part","name":id,"parent":"ws","properties":p})
}
struct Harness {
    vm: Runtime,
    store: WorldStore,
    projection: Arc<ProjectionContext>,
    shared: bool,
}
impl Harness {
    fn new(shared: bool, extra: Vec<Value>) -> Self {
        Self::with_completion(shared, extra, false)
    }
    fn with_completion(shared: bool, extra: Vec<Value>, complete: bool) -> Self {
        let mut nodes = vec![
            json!({"id":"game","class_name":"DataModel","name":"game"}),
            json!({"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game"}),
        ];
        nodes.extend(extra);
        let mut snapshot =
            json!({"version":1,"sequence":0,"services":{"Workspace":"ws"},"nodes":nodes});
        if complete {
            snapshot["initial_replication_complete"] = json!(true);
        }
        if let Some(service) = snapshot["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["class_name"] == "MaterialService")
            .map(|n| n["id"].clone())
        {
            snapshot["services"]["MaterialService"] = service;
        }
        if snapshot["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["id"] == "reader")
        {
            snapshot["script_sources"] = json!({"reader":{"source":r#"
                local p=workspace.p
                local function sample()
                    assert(p.Mass==p:GetMass())
                    script:SetAttribute('Mass',p.Mass)
                    script:SetAttribute('Density',p.CurrentPhysicalProperties.Density)
                    script:SetAttribute('AssemblyMass',p.AssemblyMass)
                end
                local massSignals=0
                p:GetPropertyChangedSignal('Mass'):Connect(function(...)
                    assert(select('#',...)==0)
                    massSignals+=1;script:SetAttribute('MassSignals',massSignals);sample()
                end)
                p:GetPropertyChangedSignal('Size'):Connect(sample)
                script.Parent:BindToMessage('sample',sample)
                script.Parent:BindToMessage('inertia',function() script:SetAttribute('Torque',p:AngularAccelerationToTorque(Vector3.one)) end)
                script.Parent:BindToMessage('resize',function() p.Size=Vector3.new(6,2,2);sample() end)
                script.Parent:BindToMessage('scale',function() p.Parent:ScaleTo(3);sample() end)
                script.Parent:BindToMessage('material-override',function(name) game:GetService('MaterialService'):SetBaseMaterialOverride(Enum.Material.Wood,name);sample() end)
                sample()
            "#}});
        }
        let store = WorldStore::from_snapshot(&snapshot).unwrap();
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
        let vm = if shared {
            compact_snapshot(&mut snapshot, &view).unwrap();
            Runtime::new_with_shared_world(snapshot, 32, &view).unwrap()
        } else {
            Runtime::new(snapshot, 32).unwrap()
        };
        Self {
            vm,
            store,
            projection,
            shared,
        }
    }
    fn run(&mut self, source: &str) -> Value {
        call(
            &mut self.vm,
            json!({"op":"execute","source":source,"timeout_ms":100}),
        )["values"]
            .clone()
    }
    fn update(&mut self, id: &str, fields: Value) {
        self.updates(id, vec![fields]);
    }
    fn updates(&mut self, id: &str, fields: Vec<Value>) {
        let base = self.store.sequence();
        let actions: Vec<_> = fields.into_iter().enumerate().map(|(i, fields)|
            json!({"kind":"set","id":id,"sequence":base+i as u64+1,"fields":fields})).collect();
        let sequence = base + actions.len() as u64;
        let delta = json!({"version":1,"delta":true,"sequence":sequence,"actions":actions});
        self.apply(delta);
    }
    fn apply(&mut self, mut delta: Value) {
        self.store.apply_delta(&delta).unwrap();
        let view = SharedWorldView::projected(self.store.view(), self.projection.clone());
        let reply = if self.shared {
            compact_delta(&mut delta, &view).unwrap();
            self.vm
                .call_with_shared_world(
                    json!({"op":"update","timeout_ms":100,"delta":delta}),
                    &view,
                )
                .unwrap()
        } else {
            self.vm
                .call(json!({"op":"update","timeout_ms":100,"delta":delta}))
                .unwrap()
        };
        successful(&reply).unwrap_or_else(|e| panic!("{e}: {reply}"));
    }
}
#[test]
fn local_physical_edits_make_mass_property_and_method_agree() {
    for shared in [false, true] {
        let mut p = part("p", 0.);
        p["properties"]["Mass"] = json!(8);
        let mut h = Harness::new(shared, vec![p]);
        assert_eq!(
            h.run("local p=workspace.p;p.Size=Vector3.new(4,2,2);return p.Mass,p:GetMass()"),
            json!([16., 16.])
        );
        assert_eq!(h.run("local p=workspace.p;p.CustomPhysicalProperties=PhysicalProperties.new(2,0.3,0.5);return p.Mass,p:GetMass()"),json!([32.,32.]));
    }
}
#[test]
fn received_primitives_derive_mass_and_follow_received_size_and_density() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, vec![part("p", 0.)]);
        assert_eq!(
            h.run("return workspace.p.Mass,workspace.p:GetMass()"),
            json!([8., 8.])
        );
        h.update("p", json!({"Size":{"$vector3":[4,2,2]}}));
        assert_eq!(
            h.run("return workspace.p.Mass,workspace.p:GetMass()"),
            json!([16., 16.])
        );
        h.update(
            "p",
            json!({"CustomPhysicalProperties":{"$physical_properties":[2,0.3,0.5,1,1]}}),
        );
        assert_eq!(
            h.run("return workspace.p.Mass,workspace.p:GetMass()"),
            json!([32., 32.])
        );
    }
}
#[test]
fn received_assemblies_derive_mass_center_and_point_velocity() {
    for shared in [false, true] {
        let mut p = part("p", 0.);
        p["properties"]["RootPriority"] = json!(10);
        p["properties"]["AssemblyLinearVelocity"] = json!({"$vector3":[1,0,0]});
        p["properties"]["AssemblyAngularVelocity"] = json!({"$vector3":[0,0,2]});
        let mut h = Harness::new(
            shared,
            vec![
                p,
                part("q", 4.),
                json!({"id":"j","class_name":"WeldConstraint","name":"j","parent":"ws","properties":{"Enabled":true,"Part0":{"$ref":"p"},"Part1":{"$ref":"q"}}}),
            ],
        );
        assert_eq!(h.run("local p=workspace.p;local v=p:GetVelocityAtPosition(Vector3.new(4,0,0));return p.AssemblyMass,p.AssemblyCenterOfMass.X,p.CenterOfMass.X,v.X,v.Y"),json!([16.,2.,0.,1.,4.]));
        h.update("q", json!({"Massless":true}));
        assert_eq!(
            h.run("return workspace.p.AssemblyMass,workspace.p.AssemblyCenterOfMass.X"),
            json!([8., 0.])
        );
        h.update("q", json!({"Anchored":true}));
        assert_eq!(
            h.run("return workspace.p.AssemblyMass==math.huge,workspace.p.AssemblyCenterOfMass.X"),
            json!([true, 4.])
        );
    }
}
#[test]
fn known_mass_observations_are_usable_without_primitive_geometry() {
    let p = json!({"id":"p","class_name":"MeshPart","name":"p","parent":"ws","properties":{"Mass":12,"AssemblyMass":24,"AssemblyCenterOfMass":{"$vector3":[1,2,3]},"CenterOfMass":{"$vector3":[0,1,0]}}});
    let mut h = Harness::new(false, vec![p]);
    assert_eq!(h.run("local p=workspace.p;return p.Mass,p:GetMass(),p.CenterOfMass.Y,p.AssemblyMass,p.AssemblyCenterOfMass.Z"),json!([12.,12.,1.,24.,3.]));
}
#[test]
fn missing_density_or_unknown_mesh_geometry_does_not_become_a_box_mass() {
    for shared in [false, true] {
        let mut p = part("p", 0.);
        p["properties"]
            .as_object_mut()
            .unwrap()
            .remove("CustomPhysicalProperties");
        p["properties"]["Material"] = json!({"$enum":{"type":"Material","value":256}});
        let mut mesh = part("mesh", 0.);
        mesh["class_name"] = json!("MeshPart");
        let mut h = Harness::new(shared, vec![p, mesh]);
        assert_eq!(h.run("local a=pcall(function() return workspace.p:GetMass() end);local b=pcall(function() return workspace.mesh:GetMass() end);return a,b"),json!([false,false]));
        h.update(
            "p",
            json!({"CurrentPhysicalProperties":{"$physical_properties":[3,0.3,0.5,1,1]}}),
        );
        assert_eq!(h.run("return workspace.p:GetMass()"), json!([24.]));
    }
}

#[test]
fn actor_mass_queries_follow_received_and_actor_authored_size_changes() {
    for shared in [false, true] {
        let mut h = Harness::new(
            shared,
            vec![
                part("p", 0.),
                json!({"id":"actor","class_name":"Actor","name":"Worker","parent":"ws"}),
                json!({"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}}),
            ],
        );
        h.run("return true");
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(
            h.run("return workspace.Worker.Reader:GetAttribute('Mass')"),
            json!([8.])
        );
        h.update("p", json!({"Size":{"$vector3":[4,2,2]}}));
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(
            h.run("return workspace.p.Mass,workspace.Worker.Reader:GetAttribute('Mass')"),
            json!([16., 16.])
        );
        h.run("workspace.Worker:SendMessage('resize')");
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(
            h.run("return workspace.p.Mass,workspace.Worker.Reader:GetAttribute('Mass')"),
            json!([24., 24.])
        );
        h.update("p", json!({"Size":{"$vector3":[8,2,2]}}));
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(h.run("return workspace.p.Mass,workspace.Worker.Reader:GetAttribute('Mass'),workspace.Worker.Reader:GetAttribute('AssemblyMass')"),json!([32.,32.,32.]));
    }
}
#[test]
fn primitive_mass_and_centers_use_shape_geometry() {
    for (class, shape, size, expected, center) in [
        (
            "Part",
            0,
            [2, 2, 2],
            4. * std::f64::consts::PI / 3.,
            [0., 0., 0.],
        ),
        (
            "Part",
            2,
            [4, 2, 2],
            4. * std::f64::consts::PI,
            [0., 0., 0.],
        ),
        ("WedgePart", 1, [2, 6, 6], 36., [0., -1., 1.]),
        ("CornerWedgePart", 1, [8, 4, 8], 256. / 3., [1., -1., -1.]),
        ("Seat", 1, [2, 2, 2], 8., [0., 0., 0.]),
    ] {
        let mut p = part("p", 0.);
        p["class_name"] = json!(class);
        p["properties"]["Shape"] = json!({"$enum":{"type":"PartType","value":shape}});
        p["properties"]["Size"] = json!({"$vector3":size});
        let mut h = Harness::new(false, vec![p]);
        let values=h.run("local p=workspace.p;return p.Mass,p:GetMass(),p.CenterOfMass.X,p.CenterOfMass.Y,p.CenterOfMass.Z");
        for n in 0..2 {
            assert!(
                (values[n].as_f64().unwrap() - expected).abs() < 1e-6,
                "{class}: {values}"
            );
        }
        for axis in 0..3 {
            assert_eq!(values[axis + 2].as_f64(), Some(center[axis]));
        }
    }
}
#[test]
fn received_negative_center_is_valid_during_assembly_recalculation() {
    let p = json!({"id":"p","class_name":"MeshPart","name":"p","parent":"ws","properties":{"Mass":12,"CenterOfMass":{"$vector3":[-2,0,1]},"CFrame":frame(10.),"Anchored":false,"Massless":false}});
    let mut h = Harness::new(false, vec![p]);
    assert_eq!(h.run("local p=workspace.p;return p.CenterOfMass.X,p.AssemblyCenterOfMass.X,p.AssemblyCenterOfMass.Y,p.AssemblyMass"),json!([-2.,8.,0.,12.]));
}
#[test]
fn invalid_inputs_and_destroyed_parts_do_not_produce_mass() {
    for (key, value) in [
        ("Size", json!({"$vector3":[0,2,2]})),
        ("Size", json!({"$vector3":[-2,2,2]})),
        ("Shape", json!({"$enum":{"type":"PartType","value":99}})),
        ("Shape", json!({"$enum":{"type":"Material","value":1}})),
        (
            "CustomPhysicalProperties",
            json!({"$physical_properties":[0,0.3,0.5,1,1]}),
        ),
        (
            "CustomPhysicalProperties",
            json!({"$physical_properties":[-1,0.3,0.5,1,1]}),
        ),
        ("Mass", json!(-1)),
    ] {
        let mut p = part("p", 0.);
        p["properties"][key] = value;
        let mut h = Harness::new(false, vec![p]);
        assert_eq!(
            h.run("return pcall(function() return workspace.p:GetMass() end)")[0],
            false,
            "{key}"
        );
    }
    let mut h = Harness::new(false, vec![part("p", 0.)]);
    assert_eq!(h.run("local p=workspace.p;local f=p.GetMass;local receiver=pcall(function() f(workspace) end);p:Destroy();local dead=pcall(function() f(p) end);return receiver,dead"),json!([false,false]));
}

#[test]
fn shared_joint_edits_invalidate_old_assembly_mass_and_center_observations() {
    for shared in [false, true] {
        let mut p = part("p", 0.);
        p["properties"]["AssemblyMass"] = json!(16);
        p["properties"]["AssemblyCenterOfMass"] = json!({"$vector3":[2,0,0]});
        let mut h = Harness::new(
            shared,
            vec![
                p,
                part("q", 4.),
                part("r", 8.),
                json!({"id":"j","class_name":"WeldConstraint","name":"j","parent":"ws","properties":{"Enabled":true,"Part0":{"$ref":"p"},"Part1":{"$ref":"q"}}}),
            ],
        );
        assert_eq!(h.run("workspace.j.Enabled=false;return workspace.p.AssemblyMass,workspace.p.AssemblyCenterOfMass.X"),json!([8.,0.]),"shared={shared}");
        assert_eq!(h.run("workspace.j.Enabled=true;return workspace.p.AssemblyMass,workspace.p.AssemblyCenterOfMass.X"),json!([16.,2.]),"shared={shared}");
        assert_eq!(h.run("workspace.j.Part0=workspace.r;return workspace.p.AssemblyMass,workspace.p.AssemblyCenterOfMass.X"),json!([8.,0.]),"shared={shared}");
    }
}

#[test]
fn local_mass_notifications_follow_size_and_density_without_assembly_signals() {
    let mut h = Harness::new(false, vec![]);
    h.run(r#"
        p=Instance.new('Part',workspace);p.Size=Vector3.new(2,2,2)
        p.CustomPhysicalProperties=PhysicalProperties.new(1,.3,.5)
        named=0;changed=0;assembly=0
        p:GetPropertyChangedSignal('Mass'):Connect(function(...) assert(select('#',...)==0);named+=1 end)
        p.Changed:Connect(function(key) if key=='Mass' then changed+=1 elseif key=='AssemblyMass' then assembly+=1 end end)
        p:GetPropertyChangedSignal('AssemblyMass'):Connect(function() assembly+=1 end)
        p.Size=Vector3.new(4,2,2);p.Size=Vector3.new(4,2,2)
        p.CustomPhysicalProperties=PhysicalProperties.new(2,.3,.5)
    "#);
    assert_eq!(
        h.run("return named,changed,assembly,p.Mass"),
        json!([2., 2., 0., 32.])
    );
}
#[test]
fn received_physical_edits_expire_older_mass_observations() {
    for shared in [false, true] {
        let mut p = part("p", 0.);
        p["properties"]["Mass"] = json!(8);
        let mut h = Harness::new(shared, vec![p]);
        h.run(
            "count=0;workspace.p:GetPropertyChangedSignal('Mass'):Connect(function() count+=1 end)",
        );
        h.update("p", json!({"Size":{"$vector3":[4,2,2]}}));
        assert_eq!(
            h.run("return workspace.p.Mass,workspace.p:GetMass(),count"),
            json!([16., 16., 1.]),
            "shared={shared}"
        );
        // A current explicit Mass observation in the same receipt is authoritative.
        h.update("p", json!({"Size":{"$vector3":[6,2,2]},"Mass":30}));
        assert_eq!(
            h.run("return workspace.p.Mass,count"),
            json!([30., 2.]),
            "shared={shared}"
        );
    }
}
#[test]
fn received_mass_notifications_preserve_intermediate_edits_in_root_and_actor() {
    for shared in [false, true] {
        let mut h = Harness::new(
            shared,
            vec![
                part("p", 0.),
                json!({"id":"actor","class_name":"Actor","name":"Worker","parent":"ws"}),
                json!({"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}}),
            ],
        );
        h.run(
            "count=0;workspace.p:GetPropertyChangedSignal('Mass'):Connect(function() count+=1 end)",
        );
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        h.updates(
            "p",
            vec![
                json!({"Size":{"$vector3":[4,2,2]}}),
                json!({"Size":{"$vector3":[2,2,2]}}),
            ],
        );
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(
            h.run(
                "return count,workspace.Worker.Reader:GetAttribute('MassSignals'),workspace.p.Mass"
            ),
            json!([2., 2., 8.]),
            "shared={shared}"
        );
    }
}
#[test]
fn model_scaling_emits_one_mass_notification_for_each_changed_part() {
    let mut h = Harness::new(false, vec![]);
    h.run(
        r#"
        local m=Instance.new('Model',workspace)
        p=Instance.new('Part',m);p.Size=Vector3.new(2,2,2)
        p.CustomPhysicalProperties=PhysicalProperties.new(1,.3,.5)
        count=0;p:GetPropertyChangedSignal('Mass'):Connect(function() count+=1 end)
        m:ScaleTo(2)
    "#,
    );
    assert_eq!(h.run("return p.Mass,count"), json!([64., 1.]));
}

#[test]
fn mass_waiters_and_disconnected_observers_follow_only_real_changes() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, vec![part("p", 0.)]);
        h.run(r#"
            p=workspace.p;woke=false;count=0
            connection=p:GetPropertyChangedSignal('Mass'):Connect(function() count+=1 end)
            task.spawn(function() local args=table.pack(p:GetPropertyChangedSignal('Mass'):Wait());assert(args.n==0);woke=true end)
        "#);
        call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        h.update("p", json!({"Size":{"$vector3":[2,2,2]}}));
        assert_eq!(h.run("return woke,count"), json!([false, 0.]));
        h.update("p", json!({"Size":{"$vector3":[4,2,2]}}));
        assert_eq!(h.run("return woke,count"), json!([true, 1.]));
        h.run("connection:Disconnect();p.Size=Vector3.new(6,2,2)");
        assert_eq!(h.run("return count,p.Mass"), json!([1., 24.]));
    }
}

#[test]
fn newer_material_expires_effective_density_until_a_new_observation_arrives() {
    for shared in [false, true] {
        let mut p = part("p", 0.);
        p["properties"]["CustomPhysicalProperties"] = Value::Null;
        p["properties"]["CurrentPhysicalProperties"] =
            json!({"$physical_properties":[1,0.3,0.5,1,1]});
        let mut h = Harness::new(shared, vec![p]);
        h.run(
            "count=0;workspace.p:GetPropertyChangedSignal('Mass'):Connect(function() count+=1 end)",
        );
        assert_eq!(h.run("return workspace.p.Mass"), json!([8.]));
        h.update(
            "p",
            json!({"Material":{"$enum":{"type":"Material","value":256}}}),
        );
        assert_eq!(
            h.run("return (pcall(function() return workspace.p.Mass end)),count"),
            json!([false, 0.])
        );
        h.update(
            "p",
            json!({"CurrentPhysicalProperties":{"$physical_properties":[2,0.3,0.5,1,1]}}),
        );
        assert_eq!(h.run("return workspace.p.Mass,count"), json!([16., 1.]));
    }
}

#[test]
fn actor_authored_mass_edits_notify_root_and_actor_once() {
    for shared in [false, true] {
        let mut h = Harness::new(
            shared,
            vec![
                part("p", 0.),
                json!({"id":"actor","class_name":"Actor","name":"Worker","parent":"ws"}),
                json!({"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}}),
            ],
        );
        h.run(
            "count=0;workspace.p:GetPropertyChangedSignal('Mass'):Connect(function() count+=1 end)",
        );
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        h.run("workspace.Worker:SendMessage('resize')");
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(
            h.run(
                "return count,workspace.Worker.Reader:GetAttribute('MassSignals'),workspace.p.Mass"
            ),
            json!([1., 1., 24.])
        );
    }
}

#[test]
fn root_and_actor_scaling_forward_mass_notifications_once() {
    for shared in [false, true] {
        let mut h = Harness::new(
            shared,
            vec![
                part("p", 0.),
                json!({"id":"actor","class_name":"Actor","name":"Worker","parent":"ws"}),
                json!({"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}}),
            ],
        );
        h.run("count=0;p=workspace.p;p:GetPropertyChangedSignal('Mass'):Connect(function() count+=1 end)");
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        h.run("m=Instance.new('Model',workspace);p.Parent=m;m:ScaleTo(2)");
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(
            h.run("return count,workspace.Worker.Reader:GetAttribute('MassSignals'),p.Mass"),
            json!([1., 1., 64.])
        );
        h.run("workspace.Worker:SendMessage('scale')");
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(
            h.run("return count,workspace.Worker.Reader:GetAttribute('MassSignals'),p.Mass"),
            json!([2., 2., 216.])
        );
    }
}

#[test]
fn untyped_wire_size_does_not_replace_public_size_in_mass_notifications() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, vec![part("p", 0.)]);
        h.run(
            "count=0;workspace.p:GetPropertyChangedSignal('Mass'):Connect(function() count+=1 end)",
        );
        h.update("p", json!({"size":{"$vector3":[4,2,2]}}));
        assert_eq!(h.run("return workspace.p.Mass,count"), json!([8., 0.]));
        h.update(
            "p",
            json!({"size":{"$vector3":[6,2,2]},"Size":{"$vector3":[4,2,2]}}),
        );
        assert_eq!(h.run("return workspace.p.Mass,count"), json!([16., 1.]));
    }
}

#[test]
fn current_physical_properties_follow_local_custom_values_and_are_read_only() {
    let mut h = Harness::new(false, vec![]);
    assert_eq!(
        h.run(
            r#"
        p=Instance.new('Part',workspace)
        local first=PhysicalProperties.new(3,.7,.2,4,5,.6)
        p.CustomPhysicalProperties=first
        assert(p.CurrentPhysicalProperties==first)
        local before=p.CurrentPhysicalProperties
        assert(not pcall(function() p.CurrentPhysicalProperties=first end))
        assert(not pcall(function() p.CustomPhysicalProperties={Density=9} end))
        assert(p.CurrentPhysicalProperties==before)
        p.CustomPhysicalProperties=PhysicalProperties.new(2,.3,.5)
        return p.CurrentPhysicalProperties.Density,p.CurrentPhysicalProperties.AcousticAbsorption
    "#
        ),
        json!([2., 0.])
    );
}

#[test]
fn current_physical_properties_decode_legacy_reports_and_follow_received_updates() {
    for shared in [false, true] {
        let mut p = part("p", 0.);
        p["properties"]["CurrentPhysicalProperties"] =
            json!({"$physical_properties":[8,0.4,0.2,2,3,0.7]});
        let mut h = Harness::new(shared, vec![p]);
        assert_eq!(h.run("return workspace.p.CurrentPhysicalProperties.Density,workspace.p.CustomPhysicalProperties.Density"),json!([1.,1.]));
        h.update(
            "p",
            json!({"CustomPhysicalProperties":{"$physical_properties":[2,0.5,0.25,4,6,0.5]}}),
        );
        assert_eq!(h.run("local p=workspace.p.CurrentPhysicalProperties;return p.Density,p.Friction,p.Elasticity,p.FrictionWeight,p.ElasticityWeight,p.AcousticAbsorption"),json!([2.,0.5,0.25,4.,6.,0.5]));
        h.update("p", json!({"CustomPhysicalProperties":null}));
        assert_eq!(
            h.run("return (pcall(function() return workspace.p.CurrentPhysicalProperties end))"),
            json!([false])
        );
        h.update(
            "p",
            json!({"CurrentPhysicalProperties":{"$physical_properties":[4,0.5,0.25,2,3]}}),
        );
        assert_eq!(h.run("return workspace.p.CurrentPhysicalProperties.Density,workspace.p.CurrentPhysicalProperties.AcousticAbsorption,workspace.p.Mass"),json!([4.,0.,32.]));
    }
}

#[test]
fn material_constructor_and_local_effective_properties_agree() {
    let mut h = Harness::new(false, vec![]);
    assert_eq!(h.run(r#"
        local p=Instance.new('Part',workspace)
        for _,material in {Enum.Material.Plastic,Enum.Material.Metal,Enum.Material.Slate,Enum.Material.Ice,Enum.Material.Rubber} do
            p.Material=material
            local defaults=PhysicalProperties.new(material)
            assert(p.CurrentPhysicalProperties==defaults)
            assert(math.abs(p:GetMass()-defaults.Density*p.Size.X*p.Size.Y*p.Size.Z)<.001)
        end
        assert(not pcall(function() PhysicalProperties.new(Enum.PartType.Block) end))
        assert(not pcall(function() PhysicalProperties.new('Plastic') end))
        return true
    "#),json!([true]));
}

#[test]
fn all_published_material_defaults_match_the_reference_and_part_mass() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/material-physical-properties.json")).unwrap();
    let mut h = Harness::new(false, vec![]);
    for row in fixture["materials"].as_array().unwrap() {
        let name = row["name"].as_str().unwrap();
        let source = format!(
            r#"
            local material=Enum.Material.{name}
            local p=Instance.new('Part');p.Material=material;p.Size=Vector3.new(2,2,2)
            local v=PhysicalProperties.new(material)
            assert(p.CurrentPhysicalProperties==v)
            assert(math.abs(p:GetMass()-v.Density*8)<.00001)
            p:Destroy()
            return v.Density,v.Friction,v.Elasticity,v.FrictionWeight,v.ElasticityWeight
        "#
        );
        let expected = Value::Array(
            row["components"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| json!(v.as_f64().unwrap() as f32 as f64))
                .collect(),
        );
        assert_eq!(h.run(&source), expected, "material={name}");
    }
}

#[test]
fn actors_read_effective_properties_during_received_and_local_mass_callbacks() {
    for shared in [false, true] {
        let mut h = Harness::new(
            shared,
            vec![
                part("p", 0.),
                json!({"id":"actor","class_name":"Actor","name":"Worker","parent":"ws"}),
                json!({"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}}),
            ],
        );
        h.run("density=0;workspace.p:GetPropertyChangedSignal('Mass'):Connect(function() density=workspace.p.CurrentPhysicalProperties.Density end)");
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        h.update(
            "p",
            json!({"CustomPhysicalProperties":{"$physical_properties":[3,0.5,0.25,4,6,0.5]}}),
        );
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(
            h.run("return density,workspace.Worker.Reader:GetAttribute('Density')"),
            json!([3., 3.])
        );
        h.run("workspace.p.CustomPhysicalProperties=PhysicalProperties.new(4,.3,.5)");
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(
            h.run("return density,workspace.Worker.Reader:GetAttribute('Density')"),
            json!([4., 4.])
        );
    }
}

#[test]
fn missing_or_invalid_effective_properties_remain_unavailable() {
    for shared in [false, true] {
        let mut p = part("p", 0.);
        p["properties"]["CustomPhysicalProperties"] = Value::Null;
        p["properties"]["Material"] = json!({"$enum":{"type":"Material","value":256}});
        let mut h = Harness::new(shared, vec![p]);
        assert_eq!(
            h.run("return (pcall(function() return workspace.p.CurrentPhysicalProperties end))"),
            json!([false])
        );
        h.update(
            "p",
            json!({"CurrentPhysicalProperties":{"$physical_properties":[3,0.5,0.25,4,6,0.5]}}),
        );
        assert_eq!(
            h.run("return workspace.p.CurrentPhysicalProperties.Density"),
            json!([3.])
        );
        h.update("p", json!({"MaterialVariant":"Unknown"}));
        assert_eq!(
            h.run("return (pcall(function() return workspace.p.CurrentPhysicalProperties end))"),
            json!([false])
        );
        h.update(
            "p",
            json!({"CustomPhysicalProperties":{"$physical_properties":[2,10,0.25,4,6,0.5]}}),
        );
        assert_eq!(h.run("return (pcall(function() return workspace.p.CurrentPhysicalProperties end)),(pcall(function() return workspace.p:GetMass() end))"),json!([false,false]));
    }
}

#[test]
fn clamped_constructor_density_remains_valid_and_nil_restores_local_material() {
    let mut h = Harness::new(false, vec![]);
    assert_eq!(h.run(r#"
        local p=Instance.new('Part');p.Size=Vector3.new(2,2,2)
        p.Material=Enum.Material.Plastic
        p.CustomPhysicalProperties=PhysicalProperties.new(-1,4,-1,101,-1,2)
        local v=p.CurrentPhysicalProperties
        assert(v==p.CustomPhysicalProperties)
        assert(math.abs(p:GetMass()-v.Density*8)<.00000001)
        assert(v.Friction==2 and v.Elasticity==0 and v.FrictionWeight==100 and v.ElasticityWeight==0 and v.AcousticAbsorption==1)
        p.CustomPhysicalProperties=nil
        assert(p.CurrentPhysicalProperties==PhysicalProperties.new(Enum.Material.Plastic))
        p:Destroy()
        return true
    "#),json!([true]));
}

#[test]
fn material_service_finds_nested_variants_and_reacts_to_identity_changes() {
    let mut h = Harness::new(false, vec![]);
    assert_eq!(h.run(r#"
        local s=game:GetService('MaterialService')
        assert(s==game:GetService('MaterialService'))
        assert(s:GetMaterialVariant(Enum.Material.Wood,'Dense')==nil)
        local folder=Instance.new('Folder',s)
        local v=Instance.new('MaterialVariant',folder);v.Name='Dense';v.BaseMaterial=Enum.Material.Wood
        assert(s:GetMaterialVariant(Enum.Material.Wood,'Dense')==v)
        assert(s:GetMaterialVariant(Enum.Material.Concrete,'Dense')==nil)
        v.Name='Renamed'
        assert(s:GetMaterialVariant(Enum.Material.Wood,'Dense')==nil)
        assert(s:GetMaterialVariant(Enum.Material.Wood,'Renamed')==v)
        folder.Parent=workspace
        assert(s:GetMaterialVariant(Enum.Material.Wood,'Renamed')==nil)
        folder.Parent=s;v:Destroy()
        assert(s:GetMaterialVariant(Enum.Material.Wood,'Renamed')==nil)
        return true
    "#),json!([true]));
}

#[test]
fn material_override_methods_preserve_names_and_validate_arguments() {
    let mut h = Harness::new(false, vec![]);
    assert_eq!(
        h.run(
            r#"
        local s=game:GetService('MaterialService')
        assert(s:GetBaseMaterialOverride(Enum.Material.Wood)=='')
        assert(select('#',s:SetBaseMaterialOverride(Enum.Material.Wood,'MissingVariant'))==0)
        assert(s:GetBaseMaterialOverride(Enum.Material.Wood)=='MissingVariant')
        assert(s:GetMaterialVariant(Enum.Material.Wood,'MissingVariant')==nil)
        for _,v in {'Wood',512,Enum.PartType.Block,{}} do
            assert(not pcall(function() s:SetBaseMaterialOverride(v,'bad') end))
        end
        assert(not pcall(function() s:SetBaseMaterialOverride(Enum.Material.Wood,{}) end))
        assert(s:GetBaseMaterialOverride(Enum.Material.Wood)=='MissingVariant')
        s:SetBaseMaterialOverride(Enum.Material.Wood,'')
        return s:GetBaseMaterialOverride(Enum.Material.Wood)
    "#
        ),
        json!([""])
    );
}

#[test]
fn received_material_service_reads_follow_updates_in_both_storage_modes() {
    for shared in [false, true] {
        let mut h = Harness::new(
            shared,
            vec![
                json!({"id":"materials","class_name":"MaterialService","name":"MaterialService","parent":"game","properties":{"WoodName":"Dense"}}),
                json!({"id":"pack","class_name":"Folder","name":"Pack","parent":"materials"}),
                json!({"id":"variant","class_name":"MaterialVariant","name":"Dense","parent":"pack","properties":{"BaseMaterial":{"$enum":{"type":"Material","value":512}},"CustomPhysicalProperties":{"$physical_properties":[3,0.5,0.2,1,1,0]}}}),
            ],
        );
        assert_eq!(h.run("local s=game:GetService('MaterialService');return s:GetBaseMaterialOverride(Enum.Material.Wood),s:GetMaterialVariant(Enum.Material.Wood,'Dense').Name"),json!(["Dense","Dense"]));
        h.update("variant", json!({"Name":"Changed"}));
        h.update("materials", json!({"WoodName":"Changed"}));
        assert_eq!(h.run("local s=game:GetService('MaterialService');return s:GetBaseMaterialOverride(Enum.Material.Wood),s:GetMaterialVariant(Enum.Material.Wood,'Dense')==nil,s:GetMaterialVariant(Enum.Material.Wood,'Changed').Name"),json!(["Changed",true,"Changed"]));
    }
}

#[test]
fn local_material_physics_follow_override_priority_and_catalog_edits() {
    let mut h = Harness::new(false, vec![]);
    h.run(
        r#"
        service=game:GetService('MaterialService')
        function variant(name,density)
            local v=Instance.new('MaterialVariant');v.Name=name;v.BaseMaterial=Enum.Material.Wood
            v.CustomPhysicalProperties=PhysicalProperties.new(density,.3,.5);v.Parent=service
            return v
        end
        base=variant('Global',2);named=variant('Named',3)
        service:SetBaseMaterialOverride(Enum.Material.Wood,'Global')
        p=Instance.new('Part',workspace);p.Material=Enum.Material.Wood;p.Size=Vector3.new(2,2,2)
        assert(p.CurrentPhysicalProperties.Density==2 and p.Mass==16)
        p.MaterialVariant='Named'
        assert(p.CurrentPhysicalProperties.Density==3 and p.Mass==24)
        p.CustomPhysicalProperties=PhysicalProperties.new(4,.3,.5)
        assert(p.CurrentPhysicalProperties.Density==4 and p.Mass==32)
        p.CustomPhysicalProperties=nil
        count=0;p:GetPropertyChangedSignal('Mass'):Connect(function() count+=1 end)
        named.CustomPhysicalProperties=nil
        assert(p.CurrentPhysicalProperties.Density==2 and p.Mass==16)
    "#,
    );
    assert_eq!(h.run("return count"), json!([1.]));
    h.run("service:SetBaseMaterialOverride(Enum.Material.Wood,'Missing');assert(p.CurrentPhysicalProperties==PhysicalProperties.new(Enum.Material.Wood))");
    assert_eq!(h.run("return count"), json!([2.]));
    h.run("named.CustomPhysicalProperties=PhysicalProperties.new(5,.3,.5);assert(p.Mass==40);named.Parent=workspace;assert(p.CurrentPhysicalProperties==PhysicalProperties.new(Enum.Material.Wood))");
    assert_eq!(h.run("return count"), json!([4.]));
}

#[test]
fn authored_parts_observe_received_material_dependency_updates() {
    for shared in [false, true] {
        let mut h = Harness::new(
            shared,
            vec![
                json!({"id":"materials","class_name":"MaterialService","name":"MaterialService","parent":"game","properties":{"WoodName":"Dense"}}),
                json!({"id":"variant","class_name":"MaterialVariant","name":"Dense","parent":"materials","properties":{"BaseMaterial":{"$enum":{"type":"Material","value":512}},"CustomPhysicalProperties":{"$physical_properties":[2,0.5,0.2,1,1,0]}}}),
            ],
        );
        h.run("p=Instance.new('Part',workspace);p.Material=Enum.Material.Wood;p.Size=Vector3.new(2,2,2);count=0;p:GetPropertyChangedSignal('Mass'):Connect(function() count+=1 end);assert(p.Mass==16)");
        h.update(
            "variant",
            json!({"CustomPhysicalProperties":{"$physical_properties":[3,0.5,0.2,1,1,0]}}),
        );
        assert_eq!(
            h.run("return p.Mass,p.CurrentPhysicalProperties.Density,count"),
            json!([24., 3., 1.])
        );
        h.update("materials", json!({"WoodName":""}));
        assert_eq!(h.run("return p.CurrentPhysicalProperties==PhysicalProperties.new(Enum.Material.Wood),count"),json!([true,2.]));
    }
}

fn material_world_nodes(actor: bool) -> Vec<Value> {
    let mut p = part("p", 0.);
    p["server_owned"] = json!(false);
    p["properties"]["Material"] = json!({"$enum":{"type":"Material","value":512}});
    p["properties"]["CustomPhysicalProperties"] = Value::Null;
    p["properties"]["CurrentPhysicalProperties"] =
        json!({"$physical_properties":[9,0.5,0.25,1,1,0]});
    let mut nodes = vec![
        p,
        json!({"id":"materials","class_name":"MaterialService","name":"MaterialService","parent":"game","properties":{"WoodName":"Dense"}}),
        json!({"id":"variant","class_name":"MaterialVariant","name":"Dense","parent":"materials","properties":{"BaseMaterial":{"$enum":{"type":"Material","value":512}},"CustomPhysicalProperties":{"$physical_properties":[2,0.5,0.2,1,1,0]}}}),
    ];
    if actor {
        nodes.extend([
        json!({"id":"actor","class_name":"Actor","name":"Worker","parent":"ws"}),
        json!({"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}})]);
    }
    nodes
}
#[test]
fn material_dependency_changes_reach_actor_physics_and_overrides_round_trip() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, material_world_nodes(true));
        h.run(
            "count=0;workspace.p:GetPropertyChangedSignal('Mass'):Connect(function() count+=1 end)",
        );
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        h.run("game:GetService('MaterialService'):GetMaterialVariant(Enum.Material.Wood,'Dense').CustomPhysicalProperties=PhysicalProperties.new(3,.3,.5)");
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(
            h.run("return workspace.Worker.Reader:GetAttribute('Density'),count"),
            json!([3., 1.])
        );
        h.update(
            "variant",
            json!({"CustomPhysicalProperties":{"$physical_properties":[4,0.5,0.2,1,1,0]}}),
        );
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(
            h.run("return workspace.Worker.Reader:GetAttribute('Density'),count"),
            json!([4., 2.])
        );
        h.run("workspace.Worker:SendMessage('material-override','')");
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(h.run("return game:GetService('MaterialService'):GetBaseMaterialOverride(Enum.Material.Wood),workspace.p.CurrentPhysicalProperties==PhysicalProperties.new(Enum.Material.Wood),count,workspace.Worker.Reader:GetAttribute('MassSignals')"),json!(["",true,3.,3.]));
    }
}
#[test]
fn stale_part_receipts_cannot_hide_new_material_dependency_notifications() {
    let mut h = Harness::new(false, material_world_nodes(false));
    h.run("count=0;workspace.p:GetPropertyChangedSignal('Mass'):Connect(function() count+=1 end)");
    h.update(
        "variant",
        json!({"CustomPhysicalProperties":{"$physical_properties":[3,0.5,0.2,1,1,0]}}),
    );
    call(
        &mut h.vm,
        json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":2,"actions":[
            {"kind":"set","id":"p","sequence":1,"fields":{"Size":{"$vector3":[4,2,2]}}},
            {"kind":"set","id":"variant","sequence":2,"fields":{"CustomPhysicalProperties":{"$physical_properties":[4,0.5,0.2,1,1,0]}}}
        ]}}),
    );
    assert_eq!(h.run("return workspace.p.Mass,count"), json!([32., 2.]));
}
#[test]
fn all_published_override_methods_work_and_raw_fields_stay_non_scriptable() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/material-service-overrides.json")).unwrap();
    let mut h = Harness::new(false, vec![]);
    for row in fixture["overrides"].as_array().unwrap() {
        let name = row["name"].as_str().unwrap();
        let key = row["property"].as_str().unwrap();
        assert_eq!(
            h.run(&format!(
                r#"
            local s=game:GetService('MaterialService')
            s:SetBaseMaterialOverride(Enum.Material.{name},'Check')
            assert(s:GetBaseMaterialOverride(Enum.Material.{name})=='Check')
            assert(not pcall(function() return s.{key} end))
            assert(not pcall(function() s.{key}='Other' end))
            assert(s:GetBaseMaterialOverride(Enum.Material.{name})=='Check')
            s:SetBaseMaterialOverride(Enum.Material.{name},'')
            return true
        "#
            )),
            json!([true])
        );
    }
}

fn received_material_nodes(actor: bool) -> Vec<Value> {
    let mut nodes = material_world_nodes(actor);
    nodes[0]["server_owned"] = json!(true);
    nodes[0]["properties"]
        .as_object_mut()
        .unwrap()
        .remove("CurrentPhysicalProperties");
    nodes[0]["properties"]["MaterialVariant"] = json!("");
    nodes
}

#[test]
fn received_catalog_density_drives_physical_reads_mass_and_signals() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, received_material_nodes(false));
        h.run(
            "count=0;workspace.p:GetPropertyChangedSignal('Mass'):Connect(function() count+=1 end)",
        );
        assert_eq!(h.run("return workspace.p.Mass,workspace.p:GetMass(),workspace.p.CurrentPhysicalProperties.Density"),json!([16.,16.,2.]));
        h.update(
            "variant",
            json!({"CustomPhysicalProperties":{"$physical_properties":[3,0.5,0.2,1,1,0]}}),
        );
        assert_eq!(
            h.run("return workspace.p.Mass,workspace.p.CurrentPhysicalProperties.Density,count"),
            json!([24., 3., 1.])
        );
        h.run("game:GetService('MaterialService'):GetMaterialVariant(Enum.Material.Wood,'Dense').CustomPhysicalProperties=PhysicalProperties.new(4,.3,.5)");
        assert_eq!(h.run("return workspace.p.Mass,count"), json!([32., 2.]));
        h.update("materials", json!({"WoodName":""}));
        assert_eq!(h.run("return workspace.p.CurrentPhysicalProperties==PhysicalProperties.new(Enum.Material.Wood),count"),json!([true,3.]));
    }
}

#[test]
fn received_catalog_requires_known_part_inputs_and_override_configuration() {
    for shared in [false, true] {
        for missing in ["CustomPhysicalProperties", "MaterialVariant", "Material"] {
            let mut nodes = received_material_nodes(false);
            nodes[0]["properties"]
                .as_object_mut()
                .unwrap()
                .remove(missing);
            let mut h = Harness::new(shared, nodes);
            assert_eq!(h.run("return (pcall(function() return workspace.p.CurrentPhysicalProperties end)),(pcall(function() return workspace.p.Mass end))"),json!([false,false]),"{missing}");
        }
        let mut nodes = received_material_nodes(false);
        nodes[1]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("WoodName");
        let mut h = Harness::new(shared, nodes);
        assert_eq!(
            h.run("return (pcall(function() return workspace.p.CurrentPhysicalProperties end))"),
            json!([false])
        );
        h.update("p", json!({"MaterialVariant":"Dense"}));
        assert_eq!(
            h.run("return workspace.p.CurrentPhysicalProperties.Density,workspace.p.Mass"),
            json!([2., 16.])
        );
    }
}

#[test]
fn received_named_variants_take_priority_and_nil_inherits_the_base_override() {
    for shared in [false, true] {
        let mut nodes = received_material_nodes(false);
        nodes[0]["properties"]["MaterialVariant"] = json!("Named");
        nodes.push(json!({"id":"named","class_name":"MaterialVariant","name":"Named","parent":"materials","properties":{"BaseMaterial":{"$enum":{"type":"Material","value":512}},"CustomPhysicalProperties":{"$physical_properties":[3,0.5,0.2,1,1,0]}}}));
        let mut h = Harness::new(shared, nodes);
        assert_eq!(h.run("return workspace.p.Mass"), json!([24.]));
        h.update("named", json!({"CustomPhysicalProperties":null}));
        assert_eq!(h.run("return workspace.p.Mass"), json!([16.]));
        h.update(
            "p",
            json!({"CustomPhysicalProperties":{"$physical_properties":[4,0.5,0.2,1,1,0]}}),
        );
        assert_eq!(h.run("return workspace.p.Mass"), json!([32.]));
    }
}

#[test]
fn incomplete_received_catalog_does_not_turn_missing_variants_into_defaults() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, received_material_nodes(false));
        h.update("p", json!({"MaterialVariant":"NotReceivedYet"}));
        assert_eq!(
            h.run("return (pcall(function() return workspace.p.Mass end))"),
            json!([false])
        );
        h.update("p", json!({"MaterialVariant":""}));
        h.update("materials", json!({"WoodName":"NotReceivedYet"}));
        assert_eq!(
            h.run("return (pcall(function() return workspace.p.Mass end))"),
            json!([false])
        );
        h.update("materials", json!({"WoodName":"Dense"}));
        h.update(
            "variant",
            json!({"CustomPhysicalProperties":{"$physical_properties":[2,7,0.2,1,1,0]}}),
        );
        assert_eq!(
            h.run("return (pcall(function() return workspace.p.CurrentPhysicalProperties end))"),
            json!([false])
        );
    }
}

#[test]
fn received_catalog_density_updates_reach_actor_reads_and_notifications() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, received_material_nodes(true));
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(
            h.run("return workspace.Worker.Reader:GetAttribute('Density')"),
            json!([2.])
        );
        h.update(
            "variant",
            json!({"CustomPhysicalProperties":{"$physical_properties":[3,0.5,0.2,1,1,0]}}),
        );
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(h.run("return workspace.Worker.Reader:GetAttribute('Density'),workspace.Worker.Reader:GetAttribute('MassSignals')"),json!([3.,1.]));
        h.run("workspace.Worker:SendMessage('material-override','')");
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(h.run("return workspace.p.CurrentPhysicalProperties==PhysicalProperties.new(Enum.Material.Wood),workspace.Worker.Reader:GetAttribute('MassSignals')"),json!([true,2.]));
    }
}

#[test]
fn completed_received_catalog_follows_variant_removal_and_restoration() {
    for shared in [false, true] {
        let mut nodes = received_material_nodes(false);
        nodes[0]["properties"]["MaterialVariant"] = json!("Dense");
        let mut h = Harness::with_completion(shared, nodes, true);
        assert_eq!(h.run("return workspace.p.Mass"), json!([16.]));
        h.update("variant", json!({"Name":"Other"}));
        assert_eq!(h.run("return workspace.p.CurrentPhysicalProperties==PhysicalProperties.new(Enum.Material.Wood)"),json!([true]));
        h.update("variant", json!({"Name":"Dense"}));
        assert_eq!(h.run("return workspace.p.Mass"), json!([16.]));
        h.run("game:GetService('MaterialService'):GetMaterialVariant(Enum.Material.Wood,'Dense').Parent=workspace");
        assert_eq!(h.run("return workspace.p.CurrentPhysicalProperties==PhysicalProperties.new(Enum.Material.Wood)"),json!([true]));
        h.run("workspace.Dense.Parent=game:GetService('MaterialService')");
        assert_eq!(h.run("return workspace.p.Mass"), json!([16.]));
        h.run("game:GetService('MaterialService'):GetMaterialVariant(Enum.Material.Wood,'Dense'):Destroy()");
        assert_eq!(h.run("return workspace.p.CurrentPhysicalProperties==PhysicalProperties.new(Enum.Material.Wood)"),json!([true]));
    }
}

#[test]
fn received_parts_do_not_infer_server_defaults_from_a_synthesized_service() {
    for shared in [false, true] {
        let mut nodes = received_material_nodes(false);
        nodes.truncate(1);
        let mut h = Harness::with_completion(shared, nodes, true);
        assert_eq!(h.run("game:GetService('MaterialService');return (pcall(function() return workspace.p.Mass end))"),json!([false]));
        h.run("game:GetService('MaterialService'):SetBaseMaterialOverride(Enum.Material.Wood,'')");
        assert_eq!(h.run("return workspace.p.CurrentPhysicalProperties==PhysicalProperties.new(Enum.Material.Wood)"),json!([true]));
    }
}

fn reported_material_nodes(actor: bool) -> Vec<Value> {
    let mut nodes = received_material_nodes(actor);
    nodes[0]["properties"]["Mass"] = json!(16);
    nodes[0]["properties"]["CurrentPhysicalProperties"] =
        json!({"$physical_properties":[2,0.5,0.2,1,1,0]});
    nodes
}
#[test]
fn material_receipts_expire_reports_and_new_reports_restore_observation_priority() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, reported_material_nodes(false));
        h.run(
            "count=0;workspace.p:GetPropertyChangedSignal('Mass'):Connect(function() count+=1 end)",
        );
        h.update(
            "variant",
            json!({"CustomPhysicalProperties":{"$physical_properties":[3,0.5,0.2,1,1,0]}}),
        );
        assert_eq!(
            h.run("return workspace.p.Mass,workspace.p.CurrentPhysicalProperties.Density,count"),
            json!([24., 3., 1.])
        );
        h.update("p",json!({"Mass":80,"CurrentPhysicalProperties":{"$physical_properties":[10,0.5,0.2,1,1,0]}}));
        assert_eq!(
            h.run("return workspace.p.Mass,workspace.p.CurrentPhysicalProperties.Density"),
            json!([80., 10.])
        );
        h.update(
            "variant",
            json!({"CustomPhysicalProperties":{"$physical_properties":[4,0.5,0.2,1,1,0]}}),
        );
        assert_eq!(
            h.run("return workspace.p.Mass,workspace.p.CurrentPhysicalProperties.Density"),
            json!([32., 4.])
        );
        h.run("game:GetService('MaterialService'):GetMaterialVariant(Enum.Material.Wood,'Dense').CustomPhysicalProperties=PhysicalProperties.new(5,.3,.5)");
        h.update("p",json!({"Mass":88,"CurrentPhysicalProperties":{"$physical_properties":[11,0.5,0.2,1,1,0]}}));
        assert_eq!(
            h.run("return workspace.p.Mass,workspace.p.CurrentPhysicalProperties.Density"),
            json!([40., 5.])
        );
        h.update(
            "variant",
            json!({"CustomPhysicalProperties":{"$physical_properties":[6,0.5,0.2,1,1,0]}}),
        );
        h.update("p",json!({"Mass":56,"CurrentPhysicalProperties":{"$physical_properties":[7,0.5,0.2,1,1,0]}}));
        assert_eq!(
            h.run("return workspace.p.Mass,workspace.p.CurrentPhysicalProperties.Density"),
            json!([56., 7.])
        );
    }
}
#[test]
fn unrelated_materials_and_explicit_named_variants_keep_mass_reports() {
    for shared in [false, true] {
        let mut nodes = reported_material_nodes(false);
        let mut mesh = part("mesh", 0.);
        mesh["class_name"] = json!("MeshPart");
        mesh["properties"]["Material"] = json!({"$enum":{"type":"Material","value":256}});
        mesh["properties"]["MaterialVariant"] = json!("");
        mesh["properties"]["CustomPhysicalProperties"] = Value::Null;
        mesh["properties"]["Mass"] = json!(123);
        nodes.push(mesh);
        let mut named = part("named", 0.);
        named["class_name"] = json!("MeshPart");
        named["properties"]["Material"] = json!({"$enum":{"type":"Material","value":512}});
        named["properties"]["MaterialVariant"] = json!("Separate");
        named["properties"]["CustomPhysicalProperties"] = Value::Null;
        named["properties"]["Mass"] = json!(456);
        nodes.push(named);
        nodes.push(json!({"id":"separate","class_name":"MaterialVariant","name":"Separate","parent":"materials","properties":{"BaseMaterial":{"$enum":{"type":"Material","value":512}},"CustomPhysicalProperties":{"$physical_properties":[4,0.5,0.2,1,1,0]}}}));
        let mut h = Harness::new(shared, nodes);
        h.update(
            "variant",
            json!({"CustomPhysicalProperties":{"$physical_properties":[3,0.5,0.2,1,1,0]}}),
        );
        assert_eq!(
            h.run("return workspace.p.Mass,workspace.mesh.Mass,workspace.named.Mass"),
            json!([24., 123., 456.])
        );
        h.update(
            "separate",
            json!({"CustomPhysicalProperties":{"$physical_properties":[5,0.5,0.2,1,1,0]}}),
        );
        assert_eq!(
            h.run("return workspace.mesh.Mass,(pcall(function() return workspace.named.Mass end))"),
            json!([123., false])
        );
    }
}
#[test]
fn material_report_history_survives_rename_reparent_and_local_destruction() {
    for shared in [false, true] {
        let mut h = Harness::with_completion(shared, reported_material_nodes(false), true);
        h.update("variant", json!({"Name":"Renamed"}));
        assert_eq!(h.run("return workspace.p.CurrentPhysicalProperties==PhysicalProperties.new(Enum.Material.Wood)"),json!([true]));
        h.update("variant",json!({"Name":"Dense","CustomPhysicalProperties":{"$physical_properties":[3,0.5,0.2,1,1,0]}}));
        assert_eq!(h.run("return workspace.p.Mass"), json!([24.]));
        h.run("game:GetService('MaterialService'):GetMaterialVariant(Enum.Material.Wood,'Dense').Parent=workspace");
        assert_eq!(h.run("return workspace.p.CurrentPhysicalProperties==PhysicalProperties.new(Enum.Material.Wood)"),json!([true]));
        h.run("workspace.Dense.Parent=game:GetService('MaterialService')");
        assert_eq!(h.run("return workspace.p.Mass"), json!([24.]));
        h.run("game:GetService('MaterialService'):GetMaterialVariant(Enum.Material.Wood,'Dense'):Destroy()");
        assert_eq!(h.run("return workspace.p.CurrentPhysicalProperties==PhysicalProperties.new(Enum.Material.Wood)"),json!([true]));
    }
}
#[test]
fn actors_expire_old_reports_after_received_and_local_material_changes() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, reported_material_nodes(true));
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        h.update(
            "variant",
            json!({"CustomPhysicalProperties":{"$physical_properties":[3,0.5,0.2,1,1,0]}}),
        );
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(h.run("return workspace.p.Mass,workspace.Worker.Reader:GetAttribute('Mass'),workspace.Worker.Reader:GetAttribute('Density')"),json!([24.,24.,3.]));
        h.run("workspace.Worker:SendMessage('material-override','')");
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(h.run("return workspace.p.CurrentPhysicalProperties==PhysicalProperties.new(Enum.Material.Wood),workspace.Worker.Reader:GetAttribute('Mass')==workspace.p.Mass"),json!([true,true]));
    }
}

#[test]
fn report_freshness_uses_its_own_materials_receipt_sequence() {
    for shared in [false, true] {
        let mut nodes = reported_material_nodes(false);
        nodes.push(json!({"id":"other","class_name":"MaterialVariant","name":"Other","parent":"materials","properties":{"BaseMaterial":{"$enum":{"type":"Material","value":256}},"CustomPhysicalProperties":{"$physical_properties":[1,0.5,0.2,1,1,0]}}}));
        let mut h = Harness::new(shared, nodes);
        h.apply(json!({"version":1,"delta":true,"sequence":3,"actions":[
            {"kind":"set","id":"variant","sequence":1,"fields":{"CustomPhysicalProperties":{"$physical_properties":[3,0.5,0.2,1,1,0]}}},
            {"kind":"set","id":"p","sequence":2,"fields":{"Mass":40,"CurrentPhysicalProperties":{"$physical_properties":[5,0.5,0.2,1,1,0]}}},
            {"kind":"set","id":"other","sequence":3,"fields":{"CustomPhysicalProperties":{"$physical_properties":[4,0.5,0.2,1,1,0]}}}
        ]}));
        assert_eq!(
            h.run("return workspace.p.Mass,workspace.p.CurrentPhysicalProperties.Density"),
            json!([40., 5.])
        );
        h.apply(json!({"version":1,"delta":true,"sequence":5,"actions":[
            {"kind":"set","id":"p","sequence":4,"fields":{"Mass":48,"CurrentPhysicalProperties":{"$physical_properties":[6,0.5,0.2,1,1,0]}}},
            {"kind":"set","id":"variant","sequence":5,"fields":{"CustomPhysicalProperties":{"$physical_properties":[4,0.5,0.2,1,1,0]}}}
        ]}));
        assert_eq!(
            h.run("return workspace.p.Mass,workspace.p.CurrentPhysicalProperties.Density"),
            json!([32., 4.])
        );
    }
}
#[test]
fn equal_part_input_cannot_swallow_a_material_mass_notification() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, reported_material_nodes(true));
        h.run(
            "count=0;workspace.p:GetPropertyChangedSignal('Mass'):Connect(function() count+=1 end)",
        );
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        h.apply(json!({"version":1,"delta":true,"sequence":2,"actions":[
            {"kind":"set","id":"p","sequence":1,"fields":{"Size":{"$vector3":[2,2,2]}}},
            {"kind":"set","id":"variant","sequence":2,"fields":{"CustomPhysicalProperties":{"$physical_properties":[3,0.5,0.2,1,1,0]}}}
        ]}));
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(
            h.run(
                "return workspace.p.Mass,count,workspace.Worker.Reader:GetAttribute('MassSignals')"
            ),
            json!([24., 1., 1.])
        );
    }
}
#[test]
fn failed_catalog_transactions_preserve_observations_and_history() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, reported_material_nodes(false));
        let response=h.vm.call(json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":2,"actions":[
            {"kind":"set","id":"variant","sequence":1,"fields":{"CustomPhysicalProperties":{"$physical_properties":[3,0.5,0.2,1,1,0]}}},
            {"kind":"bad-action","sequence":2}
        ]}})).unwrap();
        assert!(successful(&response).is_err(), "{response}");
        assert_eq!(
            h.run("return workspace.p.Mass,workspace.p.CurrentPhysicalProperties.Density"),
            json!([16., 2.])
        );
        h.update(
            "variant",
            json!({"CustomPhysicalProperties":{"$physical_properties":[4,0.5,0.2,1,1,0]}}),
        );
        assert_eq!(h.run("return workspace.p.Mass"), json!([32.]));
    }
}
#[test]
fn received_removal_of_a_variant_subtree_invalidates_old_reports() {
    for shared in [false, true] {
        let mut nodes = reported_material_nodes(false);
        nodes[2]["parent"] = json!("pack");
        nodes.push(json!({"id":"pack","class_name":"Folder","name":"Pack","parent":"materials"}));
        let mut h = Harness::with_completion(shared, nodes, true);
        h.apply(json!({"version":1,"delta":true,"sequence":1,"actions":[{"kind":"remove","sequence":1,"id":"pack"}]}));
        assert_eq!(h.run("return workspace.p.CurrentPhysicalProperties==PhysicalProperties.new(Enum.Material.Wood)"),json!([true]));
    }
}

#[test]
fn newly_received_variants_expire_reports_but_later_duplicates_do_not() {
    for shared in [false, true] {
        let mut nodes = reported_material_nodes(false);
        let mut variant = nodes.pop().unwrap();
        variant["properties"]["CustomPhysicalProperties"] =
            json!({"$physical_properties":[3,0.5,0.2,1,1,0]});
        let mut h = Harness::new(shared, nodes);
        assert_eq!(h.run("return workspace.p.Mass"), json!([16.]));
        h.apply(json!({"version":1,"delta":true,"sequence":1,"actions":[{"kind":"create","sequence":1,"node":variant}]}));
        assert_eq!(h.run("return workspace.p.Mass"), json!([24.]));
        h.update("p",json!({"Mass":40,"CurrentPhysicalProperties":{"$physical_properties":[5,0.5,0.2,1,1,0]}}));
        variant["id"] = json!("duplicate");
        variant["properties"]["CustomPhysicalProperties"] =
            json!({"$physical_properties":[9,0.5,0.2,1,1,0]});
        h.apply(json!({"version":1,"delta":true,"sequence":3,"actions":[{"kind":"create","sequence":3,"node":variant}]}));
        assert_eq!(h.run("return workspace.p.Mass"), json!([40.]));
        h.apply(json!({"version":1,"delta":true,"sequence":4,"actions":[{"kind":"remove","sequence":4,"id":"variant"}]}));
        assert_eq!(h.run("return workspace.p.Mass"), json!([72.]));
    }
}

fn reported_assembly_nodes(actor: bool) -> Vec<Value> {
    let mut nodes = reported_material_nodes(actor);
    nodes[0]["properties"]["AssemblyMass"] = json!(24);
    nodes[0]["properties"]["AssemblyCenterOfMass"] = json!({"$vector3":[2,0,0]});
    nodes[0]["properties"]["RootPriority"] = json!(10);
    nodes[0]["properties"]["AssemblyLinearVelocity"] = json!({"$vector3":[0,0,0]});
    nodes[0]["properties"]["AssemblyAngularVelocity"] = json!({"$vector3":[0,0,2]});
    let mut q = part("q", 6.);
    q["properties"]["AssemblyMass"] = json!(24);
    q["properties"]["AssemblyCenterOfMass"] = json!({"$vector3":[2,0,0]});
    nodes.push(q);
    nodes.push(json!({"id":"joint","class_name":"WeldConstraint","name":"Joint","parent":"ws","properties":{"Part0":{"$ref":"p"},"Part1":{"$ref":"q"},"Enabled":true}}));
    nodes
}
#[test]
fn assembly_reports_expire_when_a_members_material_density_changes() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, reported_assembly_nodes(true));
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        h.update(
            "variant",
            json!({"CustomPhysicalProperties":{"$physical_properties":[3,0.5,0.2,1,1,0]}}),
        );
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(h.run("return workspace.p.AssemblyMass,workspace.q.AssemblyMass,workspace.p.AssemblyCenterOfMass.X,workspace.q.AssemblyCenterOfMass.X,workspace.Worker.Reader:GetAttribute('AssemblyMass')"),json!([32.,32.,1.5,1.5,32.]));
        assert_eq!(
            h.run("return workspace.p:GetVelocityAtPosition(Vector3.zero).Y"),
            json!([-3.])
        );
        h.run("game:GetService('MaterialService'):GetMaterialVariant(Enum.Material.Wood,'Dense').CustomPhysicalProperties=PhysicalProperties.new(5,.3,.5)");
        assert_eq!(h.run("return workspace.p.AssemblyMass,workspace.q.AssemblyMass,workspace.p.AssemblyCenterOfMass.X"),json!([48.,48.,1.]));
    }
}
#[test]
fn assembly_reports_follow_received_geometry_and_accept_newer_observations() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, reported_assembly_nodes(false));
        h.update("q", json!({"Size":{"$vector3":[4,2,2]}}));
        assert_eq!(h.run("return workspace.p.AssemblyMass,workspace.q.AssemblyMass,workspace.p.AssemblyCenterOfMass.X"),json!([32.,32.,3.]));
        h.update(
            "p",
            json!({"AssemblyMass":99,"AssemblyCenterOfMass":{"$vector3":[7,0,0]}}),
        );
        assert_eq!(
            h.run("return workspace.p.AssemblyMass,workspace.p.AssemblyCenterOfMass.X"),
            json!([99., 7.])
        );
        h.update("q", json!({"CFrame":frame(12.)}));
        assert_eq!(
            h.run("return workspace.p.AssemblyMass,workspace.p.AssemblyCenterOfMass.X"),
            json!([99., 6.])
        );
        h.update("q", json!({"Massless":true}));
        assert_eq!(
            h.run("return workspace.p.AssemblyMass,workspace.p.AssemblyCenterOfMass.X"),
            json!([16., 0.])
        );
        h.update("q", json!({"Anchored":true}));
        assert_eq!(
            h.run("return workspace.p.AssemblyMass==math.huge,workspace.p.AssemblyCenterOfMass.X"),
            json!([true, 12.])
        );
    }
}
#[test]
fn received_local_centers_expire_with_geometry_but_not_with_density() {
    for shared in [false, true] {
        let mut p = part("p", 0.);
        p["class_name"] = json!("WedgePart");
        p["properties"]["Size"] = json!({"$vector3":[2,6,6]});
        p["properties"]["CenterOfMass"] = json!({"$vector3":[0,-1,1]});
        let mut mesh = part("mesh", 0.);
        mesh["class_name"] = json!("MeshPart");
        mesh["properties"]["CenterOfMass"] = json!({"$vector3":[1,2,3]});
        let mut h = Harness::new(shared, vec![p, mesh]);
        h.update("p", json!({"Size":{"$vector3":[2,12,12]}}));
        assert_eq!(
            h.run("return workspace.p.CenterOfMass.Y,workspace.p.CenterOfMass.Z"),
            json!([-2., 2.])
        );
        h.update("p", json!({"CenterOfMass":{"$vector3":[0,7,8]}}));
        assert_eq!(
            h.run("return workspace.p.CenterOfMass.Y,workspace.p.CenterOfMass.Z"),
            json!([7., 8.])
        );
        h.run("workspace.mesh.CustomPhysicalProperties=PhysicalProperties.new(2,.3,.5)");
        assert_eq!(h.run("return workspace.mesh.CenterOfMass.Y"), json!([2.]));
        h.update("mesh", json!({"Size":{"$vector3":[4,4,4]}}));
        assert_eq!(
            h.run("return (pcall(function() return workspace.mesh.CenterOfMass end))"),
            json!([false])
        );
    }
}
#[test]
fn invalid_assembly_reports_do_not_escape_as_physical_values() {
    for shared in [false, true] {
        for (key, value) in [
            ("AssemblyMass", json!(-3)),
            ("AssemblyMass", json!({"$number":"nan"})),
            ("AssemblyCenterOfMass", json!({"$vector3":[0,1e100,0]})),
        ] {
            let mut p = part("p", 0.);
            p["properties"][key] = value;
            let mut h = Harness::new(shared, vec![p]);
            assert_eq!(
                h.run(&format!(
                    "return (pcall(function() return workspace.p.{key} end))"
                )),
                json!([false]),
                "{key}"
            );
        }
    }
}

#[test]
fn assembly_report_root_selection_tracks_received_and_authored_names() {
    for shared in [false, true] {
        let mut nodes = reported_assembly_nodes(false);
        nodes[0]["properties"]["RootPriority"] = json!(0);
        let mut h = Harness::new(shared, nodes);
        h.update("p", json!({"Massless":true}));
        h.update("q", json!({"Massless":true}));
        assert_eq!(h.run("return workspace.p.AssemblyMass"), json!([16.]));
        h.update("q", json!({"Name":"HumanoidRootPart"}));
        assert_eq!(
            h.run("return workspace.p.AssemblyMass,workspace.p.AssemblyCenterOfMass.X"),
            json!([8., 6.])
        );
        h.update(
            "p",
            json!({"AssemblyMass":81,"AssemblyCenterOfMass":{"$vector3":[9,0,0]}}),
        );
        assert_eq!(h.run("return workspace.p.AssemblyMass"), json!([81.]));
        h.run("workspace.HumanoidRootPart.Name='q';workspace.p.Name='HumanoidRootPart'");
        assert_eq!(h.run("return workspace.HumanoidRootPart.AssemblyMass,workspace.HumanoidRootPart.AssemblyCenterOfMass.X"),json!([16.,0.]));
    }
}
#[test]
fn derived_mass_and_center_properties_reject_script_assignment() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, vec![part("p", 0.)]);
        assert_eq!(h.run(r#"
            local p=workspace.p
            for _,key in {'Mass','CenterOfMass','AssemblyMass','AssemblyCenterOfMass'} do
                local before=p[key]
                local ok,message=pcall(function() p[key]=if string.find(key,'Center') then Vector3.new(1,2,3) else 123 end)
                assert(not ok and string.find(message,'read%-only'))
                assert(p[key]==before)
            end
            return true
        "#),json!([true]));
    }
}
#[test]
fn unrelated_assembly_reports_remain_valid_after_another_component_changes() {
    for shared in [false, true] {
        let mut nodes = reported_assembly_nodes(false);
        let mut separate = part("separate", 20.);
        separate["class_name"] = json!("MeshPart");
        separate["properties"]["AssemblyMass"] = json!(123);
        separate["properties"]["AssemblyCenterOfMass"] = json!({"$vector3":[20,3,0]});
        nodes.push(separate);
        let mut h = Harness::new(shared, nodes);
        h.update("q", json!({"Size":{"$vector3":[4,2,2]},"Massless":true}));
        h.update(
            "variant",
            json!({"CustomPhysicalProperties":{"$physical_properties":[3,0.5,0.2,1,1,0]}}),
        );
        assert_eq!(h.run("return workspace.p.AssemblyMass,workspace.separate.AssemblyMass,workspace.separate.AssemblyCenterOfMass.Y"),json!([24.,123.,3.]));
    }
}

fn topology_nodes(actor: bool, chain: bool) -> Vec<Value> {
    let mut nodes = reported_assembly_nodes(actor);
    let mut r = part("r", 12.);
    r["properties"]["AssemblyMass"] = json!(8);
    r["properties"]["AssemblyCenterOfMass"] = json!({"$vector3":[12,0,0]});
    nodes.push(r);
    if chain {
        nodes.push(json!({"id":"tail","class_name":"WeldConstraint","name":"Tail","parent":"ws","properties":{"Part0":{"$ref":"q"},"Part1":{"$ref":"r"},"Enabled":true}}));
        for node in &mut nodes {
            if ["p", "q", "r"].contains(&node["id"].as_str().unwrap_or("")) {
                node["properties"]["AssemblyMass"] = json!(32);
                node["properties"]["AssemblyCenterOfMass"] = json!({"$vector3":[4.5,0,0]});
            }
        }
    }
    nodes
}
#[test]
fn received_joint_retarget_refreshes_both_former_and_current_assemblies() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, topology_nodes(true, false));
        h.update("joint", json!({"Part1":{"$ref":"r"}}));
        assert_eq!(h.run("return workspace.p.AssemblyMass,workspace.q.AssemblyMass,workspace.r.AssemblyMass,workspace.p.AssemblyCenterOfMass.X,workspace.q.AssemblyCenterOfMass.X,workspace.p:GetVelocityAtPosition(Vector3.zero).Y"),json!([24.,8.,24.,4.,6.,-8.]));
        h.run("workspace.Worker:SendMessage('sample')");
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(
            h.run("return workspace.Worker.Reader:GetAttribute('AssemblyMass')"),
            json!([24.])
        );
        h.update("joint", json!({"Enabled":false}));
        h.run("workspace.Worker:SendMessage('sample')");
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(h.run("return workspace.p.AssemblyMass,workspace.Worker.Reader:GetAttribute('AssemblyMass')"),json!([16.,16.]));
    }
}
#[test]
fn received_joint_splits_merges_and_deletion_invalidate_entire_components() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, topology_nodes(false, true));
        h.update("joint", json!({"Enabled":false}));
        assert_eq!(h.run("return workspace.p.AssemblyMass,workspace.q.AssemblyMass,workspace.r.AssemblyMass,workspace.q.AssemblyCenterOfMass.X"),json!([16.,16.,16.,9.]));
        h.update("joint", json!({"Enabled":true}));
        assert_eq!(h.run("return workspace.p.AssemblyMass,workspace.q.AssemblyMass,workspace.r.AssemblyCenterOfMass.X"),json!([32.,32.,4.5]));
        h.apply(json!({"version":1,"delta":true,"sequence":3,"actions":[{"kind":"remove","id":"joint","sequence":3}]}));
        assert_eq!(
            h.run(
                "return workspace.p.AssemblyMass,workspace.q.AssemblyMass,workspace.r.AssemblyMass"
            ),
            json!([16., 16., 16.])
        );
        h.update(
            "p",
            json!({"AssemblyMass":91,"AssemblyCenterOfMass":{"$vector3":[7,0,0]}}),
        );
        assert_eq!(
            h.run("return workspace.p.AssemblyMass,workspace.p.AssemblyCenterOfMass.X"),
            json!([91., 7.])
        );
    }
}
#[test]
fn received_joint_and_part_ancestor_moves_refresh_assembly_reports() {
    for shared in [false, true] {
        let mut nodes = reported_assembly_nodes(false);
        let joint = nodes.iter_mut().find(|n| n["id"] == "joint").unwrap();
        joint["parent"] = json!("pack");
        nodes.push(json!({"id":"pack","class_name":"Folder","name":"Pack","parent":"ws"}));
        nodes.push(json!({"id":"other","class_name":"WorldModel","name":"Other","parent":"ws"}));
        let mut h = Harness::new(shared, nodes);
        h.update("pack", json!({"Parent":null}));
        assert_eq!(
            h.run("return workspace.p.AssemblyMass,workspace.q.AssemblyMass"),
            json!([16., 8.])
        );
        h.update("pack", json!({"Parent":{"$ref":"ws"}}));
        assert_eq!(h.run("return workspace.p.AssemblyMass"), json!([24.]));
        h.update("q", json!({"Parent":{"$ref":"other"}}));
        assert_eq!(
            h.run("return workspace.p.AssemblyMass,workspace.Other.q.AssemblyMass"),
            json!([16., 8.])
        );
    }
}
#[test]
fn redundant_joint_removal_preserves_reports_when_membership_is_unchanged() {
    for shared in [false, true] {
        let mut nodes = topology_nodes(false, true);
        nodes[0]["properties"]["AssemblyMass"] = json!(99);
        nodes.push(json!({"id":"spare","class_name":"WeldConstraint","name":"Spare","parent":"ws","properties":{"Part0":{"$ref":"p"},"Part1":{"$ref":"r"},"Enabled":true}}));
        let mut h = Harness::new(shared, nodes);
        h.apply(json!({"version":1,"delta":true,"sequence":1,"actions":[{"kind":"remove","id":"joint","sequence":1}]}));
        assert_eq!(
            h.run("return workspace.p.AssemblyMass,#workspace.p:GetConnectedParts(true)"),
            json!([99., 2.])
        );
    }
}
#[test]
fn local_joint_detachments_confirmed_by_receipts_do_not_revive_old_reports() {
    for shared in [false, true] {
        for mode in ["disable", "retarget", "destroy"] {
            let mut h = Harness::new(shared, topology_nodes(false, false));
            match mode {
                "disable" => {
                    h.run("workspace.Joint.Enabled=false");
                    h.update("joint", json!({"Enabled":false}));
                }
                "retarget" => {
                    h.run("workspace.Joint.Part1=workspace.r");
                    h.update("joint", json!({"Part1":{"$ref":"r"}}));
                }
                _ => {
                    h.run("workspace.Joint:Destroy()");
                    h.apply(json!({"version":1,"delta":true,"sequence":1,"actions":[{"kind":"remove","id":"joint","sequence":1}]}));
                }
            }
            assert_eq!(
                h.run("return workspace.q.AssemblyMass"),
                json!([8.]),
                "{mode} shared={shared}"
            );
            h.update("q", json!({"AssemblyMass":71}));
            assert_eq!(
                h.run("return workspace.q.AssemblyMass"),
                json!([71.]),
                "{mode} shared={shared}"
            );
        }
    }
}
#[test]
fn topology_freshness_tracks_each_affected_component_not_the_whole_batch() {
    for shared in [false, true] {
        let mut nodes = reported_assembly_nodes(false);
        nodes.extend([part("u",20.),part("v",24.),json!({"id":"unrelated","class_name":"WeldConstraint","parent":"ws","properties":{"Part0":{"$ref":"u"},"Part1":{"$ref":"v"},"Enabled":true}})]);
        let mut h = Harness::new(shared, nodes);
        h.apply(json!({"version":1,"delta":true,"sequence":3,"actions":[
            {"kind":"set","id":"joint","sequence":1,"fields":{"Enabled":false}},
            {"kind":"set","id":"p","sequence":2,"fields":{"AssemblyMass":77}},
            {"kind":"set","id":"unrelated","sequence":3,"fields":{"Enabled":false}}
        ]}));
        assert_eq!(
            h.run("return workspace.p.AssemblyMass,workspace.q.AssemblyMass"),
            json!([77., 8.])
        );
        h.apply(json!({"version":1,"delta":true,"sequence":5,"actions":[
            {"kind":"set","id":"p","sequence":4,"fields":{"AssemblyMass":88}},
            {"kind":"set","id":"joint","sequence":5,"fields":{"Enabled":true}}
        ]}));
        assert_eq!(h.run("return workspace.p.AssemblyMass"), json!([24.]));
    }
}
#[test]
fn failed_received_joint_deletion_rolls_back_history_and_confirmation() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, reported_assembly_nodes(false));
        let response=h.vm.call(json!({"op":"update","timeout_ms":100,"delta":{"version":1,"delta":true,"sequence":2,"actions":[
            {"kind":"remove","id":"joint","sequence":1},{"kind":"invalid","sequence":2}
        ]}})).unwrap();
        assert!(successful(&response).is_err());
        assert_eq!(
            h.run("return workspace.p.AssemblyMass,#workspace.p:GetConnectedParts(true)"),
            json!([24., 1.])
        );
        h.update("p", json!({"AssemblyMass":89}));
        assert_eq!(h.run("return workspace.p.AssemblyMass"), json!([89.]));
        h.run("workspace.Joint:Destroy()");
        assert_eq!(h.run("return workspace.p.AssemblyMass"), json!([16.]));
    }
}

#[test]
fn received_joint_creation_subtree_removal_and_recreation_refresh_reports() {
    for shared in [false, true] {
        let mut nodes = reported_assembly_nodes(false);
        let index = nodes.iter().position(|n| n["id"] == "joint").unwrap();
        let mut joint = nodes.remove(index);
        joint["parent"] = json!("pack");
        for n in &mut nodes {
            if n["id"] == "p" {
                n["properties"]["AssemblyMass"] = json!(16);
                n["properties"]["AssemblyCenterOfMass"] = json!({"$vector3":[0,0,0]});
            }
            if n["id"] == "q" {
                n["properties"]["AssemblyMass"] = json!(8);
                n["properties"]["AssemblyCenterOfMass"] = json!({"$vector3":[6,0,0]});
            }
        }
        nodes.push(json!({"id":"pack","class_name":"Folder","name":"Pack","parent":"ws"}));
        let mut h = Harness::new(shared, nodes);
        h.apply(json!({"version":1,"delta":true,"sequence":1,"actions":[{"kind":"create","sequence":1,"node":joint}]}));
        assert_eq!(
            h.run("return workspace.p.AssemblyMass,workspace.q.AssemblyMass"),
            json!([24., 24.])
        );
        h.apply(json!({"version":1,"delta":true,"sequence":2,"actions":[{"kind":"remove","sequence":2,"id":"pack"}]}));
        assert_eq!(
            h.run("return workspace.p.AssemblyMass,workspace.q.AssemblyMass"),
            json!([16., 8.])
        );
        h.update("p", json!({"AssemblyMass":91}));
        assert_eq!(h.run("return workspace.p.AssemblyMass"), json!([91.]));
        joint["parent"] = json!("ws");
        h.apply(json!({"version":1,"delta":true,"sequence":4,"actions":[{"kind":"create","sequence":4,"node":joint}]}));
        assert_eq!(
            h.run("return workspace.p.AssemblyMass,workspace.q.AssemblyMass"),
            json!([24., 24.])
        );
    }
}

#[test]
fn angular_conversion_box_tensor_rotation_and_gyroscopic_terms() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, vec![part("p", 0.)]);
        h.run("workspace.p.Size=Vector3.new(2,4,6)");
        assert_eq!(h.run("local p=workspace.p;local t=p:AngularAccelerationToTorque(Vector3.new(2,-1,.5),Vector3.new(1,2,3));local a=p:TorqueToAngularAcceleration(t,Vector3.new(1,2,3));return t.X,t.Y,t.Z,a.X,a.Y,a.Z"),json!([-64.,224.,-56.,2.,-1.,0.5]));
        assert_eq!(h.run("local p=workspace.p;local t=p:AngularAccelerationToTorque(Vector3.new(1,1,1));return t.X,t.Y,t.Z"),json!([208.,160.,80.]));
        let result=h.run("local p=workspace.p;p.CFrame=CFrame.Angles(0,0,math.pi/2);local t=p:AngularAccelerationToTorque(Vector3.new(1,1,1));return t.X,t.Y,t.Z");
        for (i, want) in [160., 208., 80.].iter().enumerate() {
            assert!(
                (result[i].as_f64().unwrap() - want).abs() < 0.001,
                "{result}"
            );
        }
    }
}
#[test]
fn angular_conversion_round_shapes_and_wedges_use_physical_inertia() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, vec![part("p", 0.)]);
        assert_eq!(h.run(r#"
            local function near(a,b) assert((a-b).Magnitude < .0002,tostring(a)..' ~= '..tostring(b)) end
            local p=workspace.p;p.Size=Vector3.new(2,4,6);p.Shape=Enum.PartType.Ball
            near(p:AngularAccelerationToTorque(Vector3.one),Vector3.one*(8*math.pi/15))
            p.Size=Vector3.new(4,2,6);p.Shape=Enum.PartType.Cylinder
            near(p:AngularAccelerationToTorque(Vector3.one),Vector3.new(2*math.pi,19*math.pi/3,19*math.pi/3))
            local w=Instance.new('WedgePart',workspace);w.Size=Vector3.new(2,4,6);w.CustomPhysicalProperties=PhysicalProperties.new(1,0,0)
            near(w:AngularAccelerationToTorque(Vector3.new(0,1,0)),Vector3.new(0,56,-16))
            near(w:AngularAccelerationToTorque(Vector3.new(0,0,1)),Vector3.new(0,-16,88/3))
            local c=Instance.new('CornerWedgePart',workspace);c.Size=Vector3.new(2,4,6);c.CustomPhysicalProperties=PhysicalProperties.new(1,0,0)
            near(c:AngularAccelerationToTorque(Vector3.new(0,1,0)),Vector3.new(-2.4,38,7.2))
            for _,part in {p,w,c} do
                local a=Vector3.new(2,3,-4);local omega=Vector3.new(-3,1,5)
                near(part:TorqueToAngularAcceleration(part:AngularAccelerationToTorque(a,omega),omega),a)
            end
            return true
        "#),json!([true]));
    }
}
#[test]
fn angular_conversion_assembly_parallel_axes_massless_and_topology() {
    for shared in [false, true] {
        let mut h = Harness::new(
            shared,
            vec![
                part("p", 0.),
                part("q", 4.),
                json!({"id":"j","class_name":"WeldConstraint","name":"Joint","parent":"ws","properties":{"Part0":{"$ref":"p"},"Part1":{"$ref":"q"},"Enabled":true}}),
            ],
        );
        h.update("p", json!({"RootPriority":10}));
        assert_eq!(h.run("local p,q=workspace.p,workspace.q;local a=p:AngularAccelerationToTorque(Vector3.one);local b=q:AngularAccelerationToTorque(Vector3.one);assert((a-b).Magnitude<.001);assert(math.abs(a.X-32/3)<.001 and math.abs(a.Y-224/3)<.001);return true"),json!([true]));
        h.update("q", json!({"Massless":true}));
        assert_eq!(
            h.run(
                "return math.abs(workspace.q:AngularAccelerationToTorque(Vector3.one).Y-16/3)<.001"
            ),
            json!([true])
        );
        h.update("q", json!({"Massless":false,"CFrame":frame(8.)}));
        assert_eq!(h.run("return math.abs(workspace.p:AngularAccelerationToTorque(Vector3.one).Y-800/3)<.001"),json!([true]));
        h.update("j", json!({"Enabled":false}));
        assert_eq!(
            h.run(
                "return math.abs(workspace.p:AngularAccelerationToTorque(Vector3.one).Y-16/3)<.001"
            ),
            json!([true])
        );
    }
}
#[test]
fn angular_conversion_immovable_unknown_and_invalid_inputs() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, vec![part("p", 0.)]);
        assert_eq!(h.run(r#"
            local p=workspace.p;p.Anchored=true
            local t=p:AngularAccelerationToTorque(Vector3.zero);local a=p:TorqueToAngularAcceleration(Vector3.one)
            assert(t.X==math.huge and t.Y==math.huge and t.Z==math.huge and a==Vector3.zero)
            p.Anchored=false;p.Parent=nil
            assert(p:AngularAccelerationToTorque(Vector3.one).X==math.huge and p:TorqueToAngularAcceleration(Vector3.one)==Vector3.zero)
            p.Parent=workspace
            local bad1=pcall(function() p:AngularAccelerationToTorque({1,2,3}) end)
            local bad2=pcall(function() p:TorqueToAngularAcceleration(Vector3.new(math.huge,0,0)) end)
            local bad3=pcall(function() p:AngularAccelerationToTorque(Vector3.one,Vector3.new(0/0,0,0)) end)
            local bad4=pcall(function() p.AngularAccelerationToTorque(workspace,Vector3.one) end)
            local f=p.AngularAccelerationToTorque;p:Destroy()
            local dead=pcall(function() f(p,Vector3.one) end);return bad1,bad2,bad3,bad4,dead
        "#),json!([false,false,false,false,false]));
        let mut unknown = part("p", 0.);
        unknown["class_name"] = json!("MeshPart");
        let mut h = Harness::new(shared, vec![unknown]);
        assert_eq!(
            h.run(
                "return pcall(function() workspace.p:AngularAccelerationToTorque(Vector3.one) end)"
            )[0],
            false
        );
    }
}
#[test]
fn angular_conversion_receipts_replace_local_density_and_size_without_cache() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, vec![part("p", 0.)]);
        h.run("workspace.p.Size=Vector3.new(4,4,4);workspace.p.CustomPhysicalProperties=PhysicalProperties.new(2,0,0)");
        assert_eq!(h.run("return math.abs(workspace.p:AngularAccelerationToTorque(Vector3.one).X-1024/3)<.001"),json!([true]));
        h.update("p",json!({"Size":{"$vector3":[2,2,2]},"CustomPhysicalProperties":{"$physical_properties":[1,0,0,1,1]}}));
        assert_eq!(
            h.run(
                "return math.abs(workspace.p:AngularAccelerationToTorque(Vector3.one).X-16/3)<.001"
            ),
            json!([true])
        );
    }
}

#[test]
fn angular_conversion_actor_reads_current_receipts_and_local_edits() {
    for shared in [false, true] {
        let mut h = Harness::new(
            shared,
            vec![
                part("p", 0.),
                json!({"id":"actor","class_name":"Actor","name":"Worker","parent":"ws"}),
                json!({"id":"reader","class_name":"LocalScript","name":"Reader","parent":"actor","properties":{"Enabled":true,"Disabled":false},"attributes_known":true,"attributes":{}}),
            ],
        );
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        for (size, expected) in [(2., 16. / 3.), (4., 512. / 3.)] {
            h.update("p", json!({"Size":{"$vector3":[size,size,size]}}));
            h.run("workspace.Worker:SendMessage('inertia')");
            for _ in 0..4 {
                call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
            }
            let result = h.run("return workspace.Worker.Reader:GetAttribute('Torque').X");
            assert!(
                (result[0].as_f64().unwrap() - expected).abs() < 0.001,
                "{result}"
            );
        }
        h.run("workspace.Worker:SendMessage('resize')");
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        h.run("workspace.Worker:SendMessage('inertia')");
        for _ in 0..4 {
            call(&mut h.vm, json!({"op":"poll","timeout_ms":100}));
        }
        assert_eq!(h.run("local t=workspace.Worker.Reader:GetAttribute('Torque');local root=workspace.p:AngularAccelerationToTorque(Vector3.one);return t.X,t.Y,t.Z,(t-root).Magnitude"),json!([16.,80.,80.,0.]));
    }
}
#[test]
fn angular_conversion_root_massless_anchor_and_large_translation() {
    for shared in [false, true] {
        let mut h = Harness::new(shared, vec![part("p", 0.)]);
        assert_eq!(h.run(r#"
            local p=workspace.p;p.RootPriority=10;p.Massless=true
            local q=Instance.new('Part',workspace);q.Size=Vector3.new(2,2,2);q.Position=Vector3.new(4,0,0)
            q.CustomPhysicalProperties=PhysicalProperties.new(1,0,0);q.Massless=true
            local j=Instance.new('WeldConstraint',p);j.Part0=p;j.Part1=q
            assert(math.abs(q:AngularAccelerationToTorque(Vector3.one).X-16/3)<.001)
            q.Anchored=true
            assert(p:AngularAccelerationToTorque(Vector3.one).X==math.huge)
            assert(p:TorqueToAngularAcceleration(Vector3.one)==Vector3.zero)
            j:Destroy();q:Destroy();p.Massless=false;p.Size=Vector3.new(2,4,6)
            p.CFrame=CFrame.new(1e9,-1e9,1e9)
            local t=p:AngularAccelerationToTorque(Vector3.one)
            return t.X,t.Y,t.Z
        "#),json!([208.,160.,80.]));
    }
}
#[test]
fn angular_conversion_rejects_zero_mass_unknown_size_and_output_overflow() {
    for shared in [false, true] {
        let mut p = part("p", 0.);
        p["properties"]["Mass"] = json!(0);
        let mut h = Harness::new(shared, vec![p]);
        assert_eq!(
            h.run(
                "return pcall(function() workspace.p:TorqueToAngularAcceleration(Vector3.one) end)"
            )[0],
            false
        );
        let mut p = part("p", 0.);
        p["properties"].as_object_mut().unwrap().remove("Size");
        let mut h = Harness::new(shared, vec![p]);
        assert_eq!(
            h.run(
                "return pcall(function() workspace.p:AngularAccelerationToTorque(Vector3.one) end)"
            )[0],
            false
        );
        let mut h = Harness::new(shared, vec![part("p", 0.)]);
        assert_eq!(h.run("return pcall(function() workspace.p:AngularAccelerationToTorque(Vector3.new(3e38,0,0)) end)")[0],false);
    }
}
