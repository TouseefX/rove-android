//! World-owner simulation for a bounded set of authored, received-owned boxes.
//! Teleports preserve momentum. Only successful physical steps produce touches.
use super::*;
use crate::{
    owned_part_dynamics as dynamics,
    owned_part_scene::{self as scene, Scene},
};
use rove_client_core::{
    physics::{oriented_boxes_overlap, OrientedBox},
    replication_out::contact_step_batch,
};

struct Body {
    intent: Value,
    frame: PartFrame,
    at: f64,
}
#[derive(Default)]
pub(crate) struct Step {
    pub parts: Vec<PartFrame>,
    pub contacts: Vec<Vec<u8>>,
}
pub(crate) struct Simulation {
    bodies: BTreeMap<String, Body>,
    scene: Scene,
    dirty: bool,
    pose_changes: HashSet<String>,
    scene_builds: u64,
    scene_pose_refreshes: u64,
    active: BTreeMap<(String, String), (u64, u64)>,
    steps: u64,
    seconds: f64,
    discarded_seconds: f64,
    begins: u64,
    ends: u64,
    skipped: BTreeMap<&'static str, u64>,
    last_contact_check: Value,
    contact_samples: Vec<Value>,
}
impl Default for Simulation {
    fn default() -> Self {
        Self {
            bodies: BTreeMap::new(),
            scene: Scene::default(),
            dirty: true,
            pose_changes: HashSet::new(),
            scene_builds: 0,
            scene_pose_refreshes: 0,
            active: BTreeMap::new(),
            steps: 0,
            seconds: 0.,
            discarded_seconds: 0.,
            begins: 0,
            ends: 0,
            skipped: BTreeMap::new(),
            last_contact_check: Value::Null,
            contact_samples: Vec::new(),
        }
    }
}
impl Simulation {
    /// Compatibility path ported from the exact Python CP10 source snapshot:
    /// owned_part_contacts.py a5c92806..., isr_sender.py 162f05dc.... Contacts
    /// follow the authored pose that the transport sends, with no isolated-body
    /// force integration. This is an explicit model, not native solver parity.
    pub fn advance_submitted(
        &mut self, owner: &mut OwnedPartPoses, view: &WorldView,
        parents: &HashMap<String,String>, scope: u64, legacy: bool,
    ) -> Step {
        let mut output = Step {parts:owner.prepare(view,parents,scope,legacy), contacts:vec![]};
        if !legacy { self.active.clear(); return output; }
        self.active.retain(|(a,b),(ga,gb)| {
            view.read_only_node(a).and_then(|n|n.created_sequence())==Some(*ga)
                && view.read_only_node(b).and_then(|n|n.created_sequence())==Some(*gb)
                && scene::field(view,a,"NetworkOwnerV3").as_ref().and_then(super::owner)==Some(scope)
                && scene::field(view,a,"Anchored")==Some(json!(false))
                && scene::field(view,b,"Anchored")==Some(json!(true))
        });
        if output.parts.is_empty() { return output; }
        self.refresh_scene(view, parents);
        if self.scene.overflow { self.skip("listener_budget"); return output; }
        let mut updated=self.active.clone();
        let mut transitions=vec![];
        for frame in &output.parts {
            let id=format!("{}:{}",frame.id>>32,frame.id as u32);
            updated.retain(|(a,_),_|a!=&id);
            let Some(root)=scene::workspace(view,&id) else {continue;};
            if !scene::is_box(view,&id) || scene::field(view,&id,"CanTouch")!=Some(json!(true))
                || scene::field(view,&root,"TouchesUseCollisionGroups")!=Some(json!(false)) {continue;}
            let Some(size)=scene::size(view,&id) else {continue;};
            let Ok(query)=OrientedBox::new(frame.position,size.map(|v|v/2.),frame.rotation) else {continue;};
            let Some(generation)=view.read_only_node(&id).and_then(|n|n.created_sequence()) else {continue;};
            for target in &self.scene.listeners {
                if target.workspace!=root || target.id==id || !target.supported {continue;}
                let Some(bounds)=target.bounds else {continue;};
                let Some(other)=parse_guid(&target.id) else {continue;};
                let Some(target_generation)=view.read_only_node(&target.id).and_then(|n|n.created_sequence()) else {continue;};
                let pair=(id.clone(),target.id.clone());
                let overlap=oriented_boxes_overlap(query,bounds);
                if overlap {updated.insert(pair.clone(),(generation,target_generation));}
                if overlap!=self.active.contains_key(&pair) {
                    transitions.push((((frame.id>>32)as u32,frame.id as u32),other,overlap));
                    if self.contact_samples.len()<8 {
                        self.contact_samples.push(json!({"source":id,"target":target.id,
                            "submitted_position":frame.position,"target_center":bounds.center,
                            "began":overlap,"source_owner":scope,"model":"python_submitted_pose"}));
                    }
                }
            }
        }
        if transitions.len()>64 {self.skip("contact_transition_budget");return output;}
        if !transitions.is_empty() {
            match rove_client_core::replication_out::contact_batch(&transitions) {
                Ok(packet)=>{
                    self.begins+=transitions.iter().filter(|v|v.2).count()as u64;
                    self.ends+=transitions.iter().filter(|v|!v.2).count()as u64;
                    output.contacts.push(packet);
                },
                Err(_)=>{self.skip("contact_reference_invalid");return output;}
            }
        }
        self.active=updated;
        output
    }
    fn refresh_scene(&mut self, view: &WorldView, parents: &HashMap<String, String>) {
        if self.dirty {
            self.scene = Scene::build(view, parents);
            self.scene_builds += 1;
            self.dirty = false;
        } else if !self.pose_changes.is_empty() {
            self.scene.refresh_poses(view, &self.pose_changes);
            self.scene_pose_refreshes += 1;
        }
        self.pose_changes.clear();
    }
    pub fn observe_replication(&mut self, record: &SessionRecord) {
        if let Some(rows) = record.world_delta.lifecycle_actions() {
            self.observe_scene_actions(rows);
        }
    }
    fn observe_scene_actions(&mut self, rows: &[Value]) {
        for action in rows {
            if matches!(action["kind"].as_str(), Some("create" | "remove")) {
                self.dirty = true;
            }
            if let Some(fields) = action["fields"].as_object() {
                for key in fields.keys() {
                    match key.as_str() {
                        "CFrame" | "ReplicationPV" => {
                            if let Some(id) = action["id"].as_str() {
                                if self.pose_changes.len() < 4096 {
                                    self.pose_changes.insert(id.to_owned());
                                } else {
                                    self.dirty = true;
                                }
                            }
                        }
                        "Parent" | "Anchored" | "CanCollide" | "CanTouch" | "Size" | "size"
                        | "Shape" | "shape" | "CollisionGroup" | "CollisionGroupId"
                        | "CollisionGroupReplicate" => self.dirty = true,
                        _ => {}
                    }
                }
            }
        }
    }
    pub fn discard(&mut self, id: &str) {
        self.bodies.remove(id);
        self.active.retain(|(a, _), _| a != id);
    }
    fn skip(&mut self, reason: &'static str) {
        *self.skipped.entry(reason).or_default() += 1;
    }
    pub fn advance(
        &mut self,
        owner: &mut OwnedPartPoses,
        view: &WorldView,
        parents: &HashMap<String, String>,
        scope: u64,
        legacy: bool,
        now: f64,
    ) -> Step {
        let mut output = Step::default();
        if !now.is_finite() {
            self.skip("invalid_simulation_clock");
            return output;
        }
        if !legacy {
            owner.pending.clear();
            self.bodies.clear();
            self.active.clear();
            self.skip("negotiated_physics_format_unsupported");
            return output;
        }
        let mut authored = HashSet::new();
        for (id, intent) in std::mem::take(&mut owner.pending) {
            match owner.frame(view, parents, scope, &id, &intent) {
                Ok((mut frame, source)) => {
                    if !self.bodies.contains_key(&id) && self.bodies.len() >= LIMIT {
                        owner.reject("active_body_budget");
                        continue;
                    }
                    let at = if let Some(body) = self.bodies.get(&id).filter(|b| {
                        b.intent["created_sequence"] == intent["created_sequence"]
                            && owner.frame(view, parents, scope, &id, &b.intent).is_ok()
                    }) {
                        frame.linear = body.frame.linear;
                        frame.angular = body.frame.angular;
                        body.at
                    } else {
                        self.active.retain(|(a, _), _| a != &id);
                        now
                    };
                    *owner.velocity_sources.entry(source).or_default() += 1;
                    authored.insert(id.clone());
                    self.bodies.insert(id, Body { intent, frame, at });
                }
                Err(reason) => {
                    self.discard(&id);
                    owner.reject(reason);
                }
            }
        }
        if self.bodies.is_empty() {
            return output;
        }
        self.refresh_scene(view, parents);
        // Stream-out/recreation or changed listener authority does not invent
        // TouchEnded. A later supported step may establish a new contact.
        self.active.retain(|(a, b), (ga, gb)| {
            view.read_only_node(a).and_then(|n| n.created_sequence()) == Some(*ga)
                && view.read_only_node(b).and_then(|n| n.created_sequence()) == Some(*gb)
                && scene::field(view, b, "Anchored") == Some(json!(true))
                && scene::field(view, b, "CanTouch") == Some(json!(true))
        });
        let mut bodies = std::mem::take(&mut self.bodies);
        let mut transitioned = self.active.clone();
        let mut transitions = Vec::new();
        for (id, mut body) in std::mem::take(&mut bodies) {
            if let Err(reason) = owner.frame(view, parents, scope, &id, &body.intent) {
                self.active.retain(|(a, _), _| a != &id);
                transitioned.retain(|(a, _), _| a != &id);
                owner.reject(reason);
                continue;
            }
            let elapsed = now - body.at;
            body.at = now;
            let dt = elapsed.clamp(0., 0.25);
            self.discarded_seconds += (elapsed - dt).max(0.);
            let physical = (|| {
                if elapsed < 0. {
                    return Err("simulation_clock_moved_backward");
                }
                if dt <= 0. {
                    return Err("awaiting_physical_step");
                }
                if !scene::is_box(view, &id) {
                    return Err("owned_shape_unsupported");
                }
                let root = scene::workspace(view, &id).ok_or("workspace_unavailable")?;
                let gravity = view
                    .get_property(&root, "Gravity")
                    .and_then(|v| v.as_f64())
                    .filter(|v| v.is_finite() && *v >= 0. && *v <= 1000.)
                    .ok_or("received_gravity_unavailable")?;
                let size = scene::size(view, &id).ok_or("body_size_unavailable")?;
                let servo = self.scene.servo(view, &id, size)?;
                let geometry = self.scene.motion_geometry(
                    view,
                    &id,
                    &root,
                    body.frame.position,
                    body.frame.linear,
                    size,
                    dt,
                    gravity,
                )?;
                let frame = dynamics::step(&body.frame, size, dt, gravity, servo, &geometry)?;
                Ok((frame, size, root, servo.is_some()))
            })();
            let step_start = body.frame.clone();
            match physical {
                Ok((frame, size, root, servo)) => {
                    body.frame = frame;
                    self.steps += 1;
                    self.seconds += dt;
                    *owner
                        .velocity_sources
                        .entry(if servo {
                            "local_owned_part_body_velocity_model"
                        } else {
                            "local_owned_part_gravity_model"
                        })
                        .or_default() += 1;
                    self.last_contact_check = json!({"position": body.frame.position, "size":size,
                        "can_touch":scene::field(view,&id,"CanTouch"),
                        "workspace_touch_groups":scene::field(view,&root,"TouchesUseCollisionGroups"),
                        "listeners":self.scene.listeners.len(),
                        "supported_listeners":self.scene.listeners.iter().filter(|v|v.supported && v.bounds.is_some()).count()});
                    if scene::field(view, &id, "CanTouch") == Some(json!(true))
                        && scene::field(view, &root, "TouchesUseCollisionGroups")
                            == Some(json!(false))
                    {
                        let query = OrientedBox::new(
                            body.frame.position,
                            size.map(|v| v / 2.),
                            body.frame.rotation,
                        )
                        .expect("validated physical box");
                        for target in &self.scene.listeners {
                            if target.workspace != root || target.id == id || !target.supported {
                                continue;
                            }
                            let Some(bounds) = target.bounds else {
                                continue;
                            };
                            let Some(generation) = view
                                .read_only_node(&target.id)
                                .and_then(|n| n.created_sequence())
                            else {
                                continue;
                            };
                            let pair = (id.clone(), target.id.clone());
                            let overlaps = oriented_boxes_overlap(query, bounds);
                            if overlaps {
                                transitioned.insert(
                                    pair.clone(),
                                    (
                                        body.intent["created_sequence"].as_u64().unwrap(),
                                        generation,
                                    ),
                                );
                            } else {
                                transitioned.remove(&pair);
                            }
                            // A supported physical phase can begin and end a contact
                            // within one interval, including depenetration followed
                            // by separation. A teleport alone never reaches here.
                            let start_box = OrientedBox::new(step_start.position,
                                size.map(|v|v/2.),step_start.rotation).expect("validated physical box");
                            let started_inside = oriented_boxes_overlap(start_box, bounds);
                            let was_active = self.active.contains_key(&pair);
                            if let Some(other) = parse_guid(&target.id) {
                                let first = ((body.frame.id >> 32) as u32, body.frame.id as u32);
                                if self.contact_samples.len() < 8 &&
                                    ((!was_active && started_inside) || overlaps != (was_active || started_inside)) {
                                    self.contact_samples.push(json!({"source":id,"target":target.id,
                                        "at_seconds":now,"start":step_start.position,"end":body.frame.position,
                                        "target_center":bounds.center,"target_half_size":bounds.half_size,
                                        "source_size":size,"source_owner":scene::field(view,&id,"NetworkOwnerV3"),
                                        "client_scope":scope,"was_active":was_active,
                                        "started_inside":started_inside,"ended_inside":overlaps}));
                                }
                                if !was_active && started_inside { transitions.push((first,other,true)); }
                                if overlaps != (was_active || started_inside) {
                                    transitions.push((first,other,overlaps));
                                }
                            }
                        }
                    }
                    owner.prepared += 1;
                    output.parts.push(body.frame.clone());
                }
                Err(reason) => {
                    if reason != "awaiting_physical_step" {
                        self.skip(reason);
                    }
                    if authored.contains(&id) {
                        owner.prepared += 1;
                        output.parts.push(body.frame.clone());
                    }
                }
            }
            self.bodies.insert(id, body);
        }
        if transitions.len() > 64 {
            self.skip("contact_transition_budget");
        } else if !transitions.is_empty() {
            match contact_step_batch(&transitions) {
                Ok(packet) => {
                    self.begins += transitions.iter().filter(|v| v.2).count() as u64;
                    self.ends += transitions.iter().filter(|v| !v.2).count() as u64;
                    output.contacts.push(packet);
                    self.active = transitioned;
                }
                Err(_) => self.skip("contact_reference_invalid"),
            }
        } else {
            self.active = transitioned;
        }
        output
    }
    pub fn report(&self, submitted: bool) -> Value {
        json!({"model":if submitted {"python_submitted_owned_box_overlap"} else {"isolated_boxes_gravity_single_body_velocity_static_contacts"},
            "scene_builds":self.scene_builds,"scene_pose_refreshes":self.scene_pose_refreshes,"active_bodies":self.bodies.len(),"physical_steps":self.steps,"simulated_body_seconds":self.seconds,
            "discarded_wall_seconds":self.discarded_seconds,"contact_begins_prepared":self.begins,
            "contact_ends_prepared":self.ends,"active_contacts":self.active.len(),"skipped":self.skipped,
            "last_contact_check":self.last_contact_check,
            "contact_samples":self.contact_samples,
            "listener_samples":self.scene.listeners.iter().take(8).map(|v|json!({"id":v.id,"supported":v.supported,"bounds":v.bounds.map(|b|json!({"position":b.center,"half_size":b.half_size,"rotation":b.rotation}))})).collect::<Vec<_>>(),
            "native_solver_parity":false,"server_acceptance_confirmed":false})
    }
}
fn parse_guid(id: &str) -> Option<(u32, u32)> {
    let (a, b) = id.split_once(':')?;
    let (a, b) = (a.parse::<u32>().ok()?, b.parse::<u32>().ok()?);
    (a > 0 && a < u32::MAX).then_some((a, b))
}

#[cfg(test)]
mod scene_cache_tests {
    use super::*;
    use rove_client_core::world::WorldStore;

    #[test]
    fn pose_refresh_matches_rebuild_and_membership_changes_invalidate() {
        let mut store = WorldStore::from_snapshot(&json!({"version":1,"sequence":1,"nodes":[
            {"id":"workspace","class_name":"Workspace","received_creation_complete":true,"properties":{}},
            {"id":"1:8","class_name":"Part","parent":"workspace","received_creation_complete":true,
             "properties":{"Anchored":true,"CanTouch":true,"CanCollide":true,"Size":{"$vector3":[2,2,2]},
                "CFrame":{"$cframe":{"position":[0,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}}}},
            {"id":"1:9","class_name":"TouchTransmitter","parent":"1:8","properties":{}}
        ]})).unwrap();
        let parents = HashMap::from([("Part".into(), "BasePart".into())]);
        let mut simulation = Simulation::default();
        simulation.refresh_scene(&store.view(), &parents);
        assert_eq!(simulation.scene_builds, 1);
        assert_eq!(simulation.scene.listeners.len(), 1);
        for (sequence, frame) in [(2, json!({"$cframe":{"position":[10,2,3],"rotation":[1,0,0,0,1,0,0,0,1]}})),
                                  (3, json!({"$cframe":{"position":[12,2,3],"rotation":[0,0,1,0,1,0,-1,0,0]}}))] {
            let rows = vec![json!({"kind":"set","sequence":sequence,"id":"1:8","fields":{"CFrame":frame}})];
            store.apply_delta(&json!({"version":1,"delta":true,"sequence":sequence,"actions":rows})).unwrap();
            simulation.observe_scene_actions(&rows);
            simulation.refresh_scene(&store.view(), &parents);
            let rebuilt = Scene::build(&store.view(), &parents);
            assert_eq!(simulation.scene_builds, 1);
            for (cached, fresh) in simulation.scene.colliders.iter().chain(&simulation.scene.listeners)
                .zip(rebuilt.colliders.iter().chain(&rebuilt.listeners)) {
                assert_eq!(cached.id, fresh.id);
                assert_eq!(cached.bounds.unwrap().center, fresh.bounds.unwrap().center);
                assert_eq!(cached.bounds.unwrap().rotation, fresh.bounds.unwrap().rotation);
            }
        }
        assert_eq!(simulation.scene_pose_refreshes, 2);
        let rows = vec![json!({"kind":"set","sequence":4,"id":"1:8","fields":{"CanCollide":false,"CanTouch":false}})];
        store.apply_delta(&json!({"version":1,"delta":true,"sequence":4,"actions":rows})).unwrap();
        simulation.observe_scene_actions(&rows);
        simulation.refresh_scene(&store.view(), &parents);
        assert_eq!(simulation.scene_builds, 2);
        assert!(simulation.scene.listeners.is_empty());
        assert!(simulation.scene.colliders.is_empty());
    }

    #[test]
    fn pose_refresh_pending_ids_are_bounded() {
        let mut simulation = Simulation {dirty:false, ..Default::default()};
        for id in 0..5000 {
            simulation.observe_scene_actions(&[json!({"kind":"set","id":id.to_string(),"fields":{"CFrame":null}})]);
        }
        assert_eq!(simulation.pose_changes.len(), 4096);
        assert!(simulation.dirty);
    }
}
