use super::super::scheduler;
use super::*;
use mlua::AnyUserData;
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

#[path = "attachment_actor_tests.rs"]
mod attachment_tests;
#[path = "bulk_move_actor_tests.rs"]
mod bulk_move_tests;
#[path = "joint_pose_actor_tests.rs"]
mod joint_pose_tests;
#[path = "model_scaling_actor_tests.rs"]
mod model_scaling_tests;

fn setup_worlds() -> (Lua, WorldRef, Lua, WorldRef) {
    let snapshot = json!({
        "version":1,"sequence":0,
        "services":{"Workspace":"workspace","CollectionService":"collection"},
        "nodes":[
            {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
            {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
            {"id":"collection","class_name":"CollectionService","name":"CollectionService","parent":"game","properties":{}},
            {"id":"worker","class_name":"Actor","name":"worker","parent":"workspace","properties":{}},
            {"id":"part","class_name":"Part","name":"Part","parent":"workspace","properties":{"Anchored":false},"attributes":{},"attributes_known":true,"collection_tags":[],"collection_tags_known":true}
        ]
    });
    let root_world = Rc::new(RefCell::new(
        world::World::from_snapshot(&snapshot, None).unwrap(),
    ));
    let root_lua = Lua::new();
    let root_scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
    scheduler::install(&root_lua, &root_scheduler).unwrap();
    super::super::install_bindings(&root_lua, &root_world).unwrap();

    let actor_world = Rc::new(RefCell::new(root_world.borrow().fork_for_actor("worker")));
    let actor_lua = Lua::new();
    let actor_scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
    scheduler::install(&actor_lua, &actor_scheduler).unwrap();
    super::super::install_bindings(&actor_lua, &actor_world).unwrap();
    (root_lua, root_world, actor_lua, actor_world)
}

fn actor_journal(actor_world: &WorldRef) -> Vec<Json> {
    actor_world
        .borrow()
        .effects
        .get("mutations")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default()
}

fn actor_instance(lua: &Lua, world: &WorldRef, id: &str) -> AnyUserData {
    match world::instance(lua, world, id).unwrap() {
        Value::UserData(value) => value,
        _ => panic!("expected Instance userdata"),
    }
}

fn prepare_pivot_model(root_lua: &Lua, root: &WorldRef, actor: &WorldRef) {
    root_lua
        .load(
            r#"
        local m=Instance.new("Model",workspace);m.Name="Mover"
        local a=Instance.new("Part",m);a.Name="A";a.CFrame=CFrame.new(-2,0,0)
        local b=Instance.new("Part",m);b.Name="B";b.CFrame=CFrame.new(2,0,0)
        m.WorldPivot=CFrame.new()
    "#,
        )
        .exec()
        .unwrap();
    actor.borrow_mut().sync_actor_fork(&root.borrow());
    root.borrow_mut().effects.clear();
    actor.borrow_mut().effects.clear();
}

#[test]
fn actor_model_pivots_merge_as_atomic_batches_with_matching_revision_fences() {
    let (root_lua, root, actor_lua, actor) = setup_worlds();
    prepare_pivot_model(&root_lua, &root, &actor);
    actor_lua
        .load(
            r#"
        workspace.Mover:PivotTo(CFrame.new(10,0,0))
        workspace.Mover:PivotTo(CFrame.new(20,0,0))
    "#,
        )
        .exec()
        .unwrap();
    let journal = actor_journal(&actor);
    assert_eq!(journal.len(), 2);
    assert!(journal.iter().all(|row| row["kind"] == "pose_batch"));
    let report = merge_actor_journal(&root_lua, &root, &actor, "worker", &journal).unwrap();
    assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
    assert_eq!(report.applied, 2);
    root_lua
        .load(
            r#"
        local m=workspace.Mover
        assert(m.A.Position.X==18 and m.B.Position.X==22 and m:GetPivot().X==20)
    "#,
        )
        .exec()
        .unwrap();
    assert!(root.borrow().effects["mutations"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["kind"] == "set"
            && row["actor_origin"] == "worker"
            && row["_pivot_cframe_only"] == true));
}

#[test]
fn stale_actor_pivot_rejects_the_entire_batch_without_moving_other_parts() {
    let (root_lua, root, actor_lua, actor) = setup_worlds();
    prepare_pivot_model(&root_lua, &root, &actor);
    actor_lua
        .load("workspace.Mover:PivotTo(CFrame.new(10,0,0))")
        .exec()
        .unwrap();
    root_lua
        .load("workspace.Mover.A.CFrame=CFrame.new(99,0,0)")
        .exec()
        .unwrap();
    let journal = actor_journal(&actor);
    let report = merge_actor_journal(&root_lua, &root, &actor, "worker", &journal).unwrap();
    assert_eq!(report.applied, 0);
    assert_eq!(report.conflicts.len(), 1);
    root_lua
        .load(
            r#"
        local m=workspace.Mover
        assert(m.A.Position.X==99 and m.B.Position.X==2 and m:GetPivot().X==0)
    "#,
        )
        .exec()
        .unwrap();
}

#[test]
fn actor_properties_attributes_and_tags_merge_through_root_setters() {
    let (root_lua, root_world, actor_lua, actor_world) = setup_worlds();
    let part = actor_instance(&actor_lua, &actor_world, "part");
    part.set("Anchored", true).unwrap();
    part.call_method::<()>("SetAttribute", ("ActorMark", 73))
        .unwrap();
    let collection = actor_instance(&actor_lua, &actor_world, "collection");
    let target = actor_instance(&actor_lua, &actor_world, "part");
    collection
        .call_method::<()>("AddTag", (target, "actor-tag"))
        .unwrap();
    let journal = actor_journal(&actor_world);

    let report =
        merge_actor_journal(&root_lua, &root_world, &actor_world, "worker", &journal).unwrap();
    assert_eq!(report.applied, 3);
    assert!(report.conflicts.is_empty());
    assert_eq!(
        world::get_property(&root_world, "part", "Anchored").unwrap(),
        json!(true)
    );
    let root_part = actor_instance(&root_lua, &root_world, "part");
    assert_eq!(
        root_part
            .call_method::<i64>("GetAttribute", "ActorMark")
            .unwrap(),
        73
    );
    assert_eq!(
        root_world.borrow().nodes["part"].collection_tags,
        ["actor-tag"]
    );
    assert!(
        root_world.borrow().effects["mutations"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["actor_origin"] == "worker")
            .count()
            >= 3
    );
}

#[test]
fn actor_create_is_adopted_only_with_its_reserved_instance_id() {
    let (root_lua, root_world, actor_lua, actor_world) = setup_worlds();
    let created =
        world::create_instance(&actor_lua, &actor_world, "Folder", Some("workspace")).unwrap();
    let id = world::instance_id(&created).unwrap();
    assert!(id.starts_with("local:actor:776f726b6572:"));
    let journal = actor_journal(&actor_world);
    let report =
        merge_actor_journal(&root_lua, &root_world, &actor_world, "worker", &journal).unwrap();
    assert_eq!(report.applied, 2);
    assert!(root_world.borrow().nodes.contains_key(&id));
    assert_eq!(
        root_world.borrow().nodes[&id].parent.as_deref(),
        Some("workspace")
    );

    let fabricated = json!({"kind":"create","node":{
        "id":"local:actor:6f74686572:1","class_name":"Folder","name":"fake",
        "parent":"workspace","properties":{},"server_owned":false
    }});
    assert!(merge_actor_journal(
        &root_lua,
        &root_world,
        &actor_world,
        "worker",
        &[fabricated]
    )
    .is_err());
}

#[test]
fn stale_actor_property_write_is_rejected_and_local_override_is_cleared() {
    let (root_lua, root_world, actor_lua, actor_world) = setup_worlds();
    let part = actor_instance(&actor_lua, &actor_world, "part");
    part.set("Anchored", true).unwrap();
    let journal = actor_journal(&actor_world);
    world::set_property(&root_lua, &root_world, "part", "Anchored", json!(false)).unwrap();
    let report =
        merge_actor_journal(&root_lua, &root_world, &actor_world, "worker", &journal).unwrap();
    assert_eq!(report.applied, 0);
    assert_eq!(report.conflicts, ["set:part.Anchored"]);
    assert_eq!(
        world::get_property(&root_world, "part", "Anchored").unwrap(),
        json!(false)
    );
    assert_eq!(
        world::get_property(&actor_world, "part", "Anchored").unwrap(),
        json!(false)
    );
}

#[test]
fn committed_actor_property_change_fires_in_peer_vm() {
    let (root_lua, root_world, actor_lua, actor_world) = setup_worlds();
    let peer_world = Rc::new(RefCell::new(root_world.borrow().fork_for_actor("peer")));
    let peer_lua = Lua::new();
    let peer_scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
    scheduler::install(&peer_lua, &peer_scheduler).unwrap();
    super::super::install_bindings(&peer_lua, &peer_world).unwrap();
    let peer_part = world::instance(&peer_lua, &peer_world, "part").unwrap();
    peer_lua.globals().set("peer_part", peer_part).unwrap();
    peer_lua
        .load("seen=false; peer_part:GetPropertyChangedSignal('Anchored'):Connect(function() seen=true end)")
        .exec()
        .unwrap();

    let part = actor_instance(&actor_lua, &actor_world, "part");
    part.set("Anchored", true).unwrap();
    let journal = actor_journal(&actor_world);
    merge_actor_journal(&root_lua, &root_world, &actor_world, "worker", &journal).unwrap();
    sync_peer(&peer_lua, &peer_world, &root_world, &journal).unwrap();
    scheduler::step(
        &peer_lua,
        &peer_scheduler,
        Instant::now(),
        Duration::from_millis(30),
    );
    assert!(peer_lua.globals().get::<bool>("seen").unwrap());
}

#[test]
fn status_strips_internal_fences_but_keeps_actor_origin() {
    let mut status = json!({"actor_id":"worker","mutations":[{
        "kind":"set","_base_local_revision":4,"_result_local_revision":5
    }],"world_effects":{"mutations":[{
        "kind":"set","_base_local_revision":4,"_result_local_revision":5
    }]}});
    strip_internal_fields(&mut status);
    assert_eq!(status["mutations"][0]["actor_origin"], "worker");
    assert_eq!(
        status["world_effects"]["mutations"][0]["actor_origin"],
        "worker"
    );
    assert!(status["mutations"][0].get("_base_local_revision").is_none());
    assert!(status["mutations"][0].get("_result_local_revision").is_none());
    assert!(status["world_effects"]["mutations"][0].get("_result_local_revision").is_none());
    assert!(status["world_effects"]["mutations"][0]
        .get("_base_local_revision")
        .is_none());
}

#[test]
fn sequential_actor_writes_survive_independent_revision_counters() {
    let (root_lua,root,actor_lua,actor)=setup_worlds();
    root.borrow_mut().effects.clear(); actor.borrow_mut().effects.clear();
    // Fork-local work can consume revision numbers without a root mutation.
    for _ in 0..100 { world::bump_local_revision(&actor,"workspace").unwrap(); }
    actor_lua.load(r#"
        local p=workspace.Part
        p:SetAttribute('Destroyed',true)
        p:SetAttribute('Released',true)
        p.Anchored=true
        p:AddTag('watch')
        p:RemoveTag('watch')
        p:SetAttribute('Done',true)
    "#).exec().unwrap();
    let journal=actor_journal(&actor);
    let report=merge_actor_journal(&root_lua,&root,&actor,"worker",&journal).unwrap();
    assert!(report.conflicts.is_empty(),"{:?}",report.conflicts);
    assert_eq!(report.applied,6);
    root_lua.load("local p=workspace.Part; assert(p:GetAttribute('Destroyed') and p:GetAttribute('Released') and p:GetAttribute('Done') and p.Anchored and not p:HasTag('watch'))").exec().unwrap();
    actor.borrow_mut().effects.clear();
    actor_lua.load("workspace.Part:SetAttribute('NextFrame',true); workspace.Part.Anchored=false").exec().unwrap();
    let report=merge_actor_journal(&root_lua,&root,&actor,"worker",&actor_journal(&actor)).unwrap();
    assert!(report.conflicts.is_empty(),"{:?}",report.conflicts);
    assert_eq!(report.applied,2);
    root_lua.load("assert(workspace.Part:GetAttribute('NextFrame') and not workspace.Part.Anchored)").exec().unwrap();
}

#[test]
fn revision_translation_does_not_accept_a_stale_actor_write_chain() {
    let (root_lua,root,actor_lua,actor)=setup_worlds();
    root.borrow_mut().effects.clear(); actor.borrow_mut().effects.clear();
    for _ in 0..100 { world::bump_local_revision(&actor,"workspace").unwrap(); }
    actor_lua.load("workspace.Part:SetAttribute('Value',1); workspace.Part:SetAttribute('Value',2)").exec().unwrap();
    root_lua.load("workspace.Part:SetAttribute('Value',99)").exec().unwrap();
    let report=merge_actor_journal(&root_lua,&root,&actor,"worker",&actor_journal(&actor)).unwrap();
    assert_eq!(report.applied,0);
    assert_eq!(report.conflicts.len(),2);
    assert_eq!(root_lua.load("return workspace.Part:GetAttribute('Value')").eval::<i64>().unwrap(),99);
}

#[test]
fn actor_created_then_destroyed_instances_replay_their_tagged_lifetime() {
    let (root_lua,root,actor_lua,actor)=setup_worlds();
    root.borrow_mut().effects.clear(); actor.borrow_mut().effects.clear();
    root_lua.load(r#"
        events={}; local cs=game:GetService('CollectionService')
        cs:GetInstanceAddedSignal('ephemeral'):Connect(function(item) table.insert(events,'+'..item.Name) end)
        cs:GetInstanceRemovedSignal('ephemeral'):Connect(function(item) table.insert(events,'-'..item.Name) end)
    "#).exec().unwrap();
    actor_lua.load(r#"
        local folder=Instance.new('Folder',workspace); folder.Name='Transient'
        local item=Instance.new('Part',folder); item.Name='Tagged'; item:AddTag('ephemeral')
        folder:Destroy()
    "#).exec().unwrap();
    let journal=actor_journal(&actor);
    let report=merge_actor_journal(&root_lua,&root,&actor,"worker",&journal).unwrap();
    assert_eq!(report.applied,journal.len());
    assert!(report.conflicts.is_empty(),"{:?}",report.conflicts);
    root_lua.load("assert(workspace:FindFirstChild('Transient')==nil and #game:GetService('CollectionService'):GetTagged('ephemeral')==0)").exec().unwrap();
    let sched=root_lua.app_data_ref::<scheduler::SchedulerRef>().unwrap().clone();
    let step=scheduler::step(&root_lua,&sched,Instant::now(),Duration::from_millis(100));
    assert!(step.errors.is_empty(),"{:?}",step.errors);
    assert_eq!(root_lua.load("return table.concat(events,',')").eval::<String>().unwrap(),"+Tagged,-Tagged");
}

#[test]
fn destroyed_actor_sources_require_a_matching_later_destroy_fence() {
    let (root_lua,root,actor_lua,actor)=setup_worlds();
    root.borrow_mut().effects.clear(); actor.borrow_mut().effects.clear();
    actor_lua.load("local p=Instance.new('Part',workspace); p:Destroy()").exec().unwrap();
    let mut journal=actor_journal(&actor);
    journal.retain(|row|row["kind"]!="destroy");
    let error=merge_actor_journal(&root_lua,&root,&actor,"worker",&journal).unwrap_err();
    assert!(error.to_string().contains("destroyed node"),"{error}");
    assert!(!root.borrow().nodes.ids().any(|id|id.starts_with("local:actor:")));
}

#[test]
fn actor_movement_uses_committed_prefix_before_a_later_conflict() {
    let (root_lua,root,actor_lua,actor)=setup_worlds();
    root_lua.load("local p=Instance.new('Part',workspace);p.Name='Other'").exec().unwrap();
    actor.borrow_mut().sync_actor_fork(&root.borrow());
    root.borrow_mut().effects.clear();actor.borrow_mut().effects.clear();
    actor_lua.load("workspace.Part.Anchored=true;workspace.Other.Anchored=true").exec().unwrap();
    let journal=actor_journal(&actor);assert_eq!(journal.len(),2);
    root_lua.load("workspace.Other.Name='ChangedByRoot'").exec().unwrap();
    let trace=merge_actor_journal_traced(&root_lua,&root,&actor,"worker",&journal,true).unwrap();
    assert_eq!(trace.first_conflict,Some(1));assert_eq!(trace.mutation_cursors.len(),3);
    let status=json!({"movement_intents":[
        {"sequence":1,"mutation_cursor":1,"kind":"Move","instance_id":"hum","vector":[1,0,0],"relative_to_camera":false},
        {"sequence":2,"mutation_cursor":2,"kind":"Move","instance_id":"hum","vector":[2,0,0],"relative_to_camera":false}
    ]});
    let report=super::super::actor_movement::forward(&root_lua,&root,"worker",&status,&trace);
    assert_eq!(report["forwarded"],1);assert_eq!(report["rejected"],1);
    let w=root.borrow();let rows=w.effects["movement_intents"].as_array().unwrap();
    assert_eq!(rows.len(),1);assert_eq!(rows[0]["mutation_cursor"],trace.mutation_cursors[1]);
    assert_eq!(rows[0]["actor_origin"],"worker");assert_eq!(rows[0]["vector"],json!([1,0,0]));
}
#[test]
fn failed_actor_merge_restores_root_intent_emission_and_rejects_captured_commands() {
    let (root_lua,root,_actor_lua,actor)=setup_worlds();
    assert!(merge_actor_journal_traced(&root_lua,&root,&actor,"worker",&[json!({"kind":"invalid"})],true).is_err());
    let trace=MergeReport{mutation_cursors:vec![0],..Default::default()};
    let status=json!({"shadow_merge_error":"bad journal","movement_intents":[{"sequence":1,"mutation_cursor":0,"kind":"Move"}]});
    let report=super::super::actor_movement::forward(&root_lua,&root,"worker",&status,&trace);
    assert_eq!(report["forwarded"],0);assert_eq!(report["rejected"],1);
    root_lua.load("local h=Instance.new('Humanoid',workspace);h:Move(Vector3.new(1,0,0))").exec().unwrap();
    assert_eq!(root.borrow().effects["movement_intents"].as_array().unwrap().len(),1);
}
