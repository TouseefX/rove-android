//! Player/character receipts observed by actual scripts in the official VM.
#![cfg(feature = "rust-host")]

use headless_luau::runtime_owner::{successful, Runtime};
use serde_json::{json, Value};

fn snapshot(character: Value) -> Value {
    json!({"version":1,"sequence":0,"services":{"Players":"players","Workspace":"ws"},
        "local_player":"p","nodes":[
        {"id":"game","class_name":"DataModel","name":"game","properties":{}},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
        {"id":"players","class_name":"Players","name":"Players","parent":"game","properties":{}},
        {"id":"p","class_name":"Player","name":"First","parent":"players","properties":{"Character":character}}
    ]})
}

fn execute(vm: &mut Runtime, source: &str) -> Value {
    let reply = vm
        .call(json!({"op":"execute","source":source,"timeout_ms":100}))
        .unwrap();
    successful(&reply).unwrap_or_else(|e| panic!("{e}: {reply}"));
    assert!(
        reply["task_errors"]
            .as_array()
            .is_none_or(|errors| errors.is_empty()),
        "{reply}"
    );
    reply["values"].clone()
}

fn update(vm: &mut Runtime, actions: Vec<Value>) -> Value {
    let sequence = actions.last().unwrap()["sequence"].clone();
    let reply = vm
        .call(json!({"op":"update","timeout_ms":100,"delta":{
            "version":1,"delta":true,"sequence":sequence,"actions":actions
        }}))
        .unwrap();
    successful(&reply).unwrap_or_else(|e| panic!("{e}: {reply}"));
    assert!(
        reply["task_errors"]
            .as_array()
            .is_none_or(|errors| errors.is_empty()),
        "{reply}"
    );
    reply
}

fn model(id: &str, sequence: u64) -> Value {
    // CharacterAdded must not require Workspace parenting or a Humanoid.
    json!({"kind":"create","sequence":sequence,"node":{
        "id":id,"class_name":"Model","name":id,"parent":null,"properties":{}
    }})
}

fn character(id: Option<&str>, sequence: u64) -> Value {
    json!({"kind":"set","sequence":sequence,"id":"p","fields":{
        "Character":id.map(|id|json!({"$ref":id})).unwrap_or(Value::Null)
    }})
}

#[test]
fn a_waiting_script_resumes_with_the_received_character_before_its_root_arrives() {
    let mut vm = Runtime::new(snapshot(Value::Null), 32).unwrap();
    execute(
        &mut vm,
        r#"
        p=game:GetService('Players').LocalPlayer
        added, removed, appearance, changes = 0, 0, 0, 0
        p.CharacterAdded:Connect(function(c) added+=1; assert(c==p.Character) end)
        p.CharacterRemoving:Connect(function() removed+=1 end)
        p.CharacterAppearanceLoaded:Connect(function() appearance+=1 end)
        p:GetPropertyChangedSignal('Character'):Connect(function() changes+=1 end)
        task.spawn(function()
            spawned=p.Character or p.CharacterAdded:Wait()
            assert(spawned:IsA('Model') and spawned.Parent==nil)
            root=spawned:WaitForChild('HumanoidRootPart')
        end)
    "#,
    );
    update(&mut vm, vec![model("char", 1), character(Some("char"), 2)]);
    assert_eq!(
        execute(
            &mut vm,
            "return added, removed, appearance, changes, spawned==p.Character, root==nil"
        ),
        json!([1.0, 0.0, 0.0, 1.0, true, true])
    );
    update(
        &mut vm,
        vec![json!({"kind":"create","sequence":3,"node":{
            "id":"root","class_name":"Part","name":"HumanoidRootPart","parent":"char","properties":{}
        }})],
    );
    assert_eq!(
        execute(&mut vm, "return root==p.Character.HumanoidRootPart"),
        json!([true])
    );
}

#[test]
fn unresolved_character_retargets_cancel_old_waits_and_do_not_fire_nil_events() {
    let mut vm = Runtime::new(snapshot(Value::Null), 32).unwrap();
    execute(
        &mut vm,
        r#"
        p=game:GetService('Players').LocalPlayer
        events={}; changes=0
        p.CharacterAdded:Connect(function(c) assert(c~=nil); table.insert(events,'+'..c.Name) end)
        p.CharacterRemoving:Connect(function(c) assert(c~=nil); table.insert(events,'-'..c.Name) end)
        p:GetPropertyChangedSignal('Character'):Connect(function() changes+=1 end)
    "#,
    );
    update(&mut vm, vec![character(Some("old"), 1)]);
    assert_eq!(
        execute(&mut vm, "return p.Character==nil, #events, changes"),
        json!([true, 0.0, 0.0])
    );
    update(&mut vm, vec![character(Some("new"), 2), model("old", 3)]);
    assert_eq!(
        execute(&mut vm, "return p.Character==nil, #events, changes"),
        json!([true, 0.0, 0.0])
    );
    update(&mut vm, vec![model("new", 4)]);
    assert_eq!(
        execute(&mut vm, "return table.concat(events,','), changes"),
        json!(["+new", 1.0])
    );
    update(&mut vm, vec![character(Some("new"), 5), character(None, 6)]);
    assert_eq!(
        execute(
            &mut vm,
            "return table.concat(events,','), changes, p.Character==nil"
        ),
        json!(["+new,-new", 2.0, true])
    );
    update(
        &mut vm,
        vec![
            character(Some("late"), 7),
            character(None, 8),
            model("late", 9),
        ],
    );
    assert_eq!(
        execute(&mut vm, "return table.concat(events,',')"),
        json!(["+new,-new"])
    );
}

#[test]
fn received_membership_fires_once_and_removing_precedes_child_removed() {
    let mut vm = Runtime::new(snapshot(Value::Null), 32).unwrap();
    execute(
        &mut vm,
        r#"
        players=game:GetService('Players'); events={}
        players.PlayerAdded:Connect(function(p) table.insert(events,'+'..p.Name) end)
        players.PlayerRemoving:Connect(function(p,reason)
            table.insert(events,'-'..p.Name)
            assert(reason==Enum.PlayerExitReason.Unknown)
        end)
        players.ChildRemoved:Connect(function(p) table.insert(events,'child-'..p.Name) end)
    "#,
    );
    let create = |sequence| {
        json!({"kind":"create","sequence":sequence,"node":{
            "id":"second","class_name":"Player","name":"Second","parent":"players","properties":{"Character":null}
        }})
    };
    update(&mut vm, vec![create(1)]);
    update(&mut vm, vec![create(2)]);
    assert_eq!(
        execute(
            &mut vm,
            "return table.concat(events,','), #players:GetPlayers()"
        ),
        json!(["+Second", 2.0])
    );
    update(
        &mut vm,
        vec![json!({"kind":"remove","id":"second","sequence":3})],
    );
    assert_eq!(
        execute(
            &mut vm,
            "return table.concat(events,','), #players:GetPlayers()"
        ),
        json!(["+Second,-Second,child-Second", 1.0])
    );
}

#[test]
fn respawns_within_one_receipt_keep_order_and_duplicate_receipts_are_silent() {
    let mut initial = snapshot(json!({"$ref":"original"}));
    initial["nodes"]
        .as_array_mut()
        .unwrap()
        .push(model("original", 0)["node"].clone());
    let mut vm = Runtime::new(initial, 32).unwrap();
    execute(
        &mut vm,
        r#"
        p=game:GetService('Players').LocalPlayer; events={}
        p.CharacterAdded:Connect(function(c) table.insert(events,'+'..c.Name) end)
        p.CharacterRemoving:Connect(function(c) table.insert(events,'-'..c.Name) end)
    "#,
    );
    let actions = vec![
        model("a", 1),
        model("b", 2),
        character(Some("a"), 3),
        character(Some("b"), 4),
        character(None, 5),
    ];
    update(&mut vm, actions.clone());
    let expected = json!(["-original,+a,-a,+b,-b", true]);
    assert_eq!(
        execute(&mut vm, "return table.concat(events,','), p.Character==nil"),
        expected
    );
    update(&mut vm, actions);
    assert_eq!(
        execute(&mut vm, "return table.concat(events,','), p.Character==nil"),
        expected
    );
}

#[test]
fn invalid_receipt_leaves_character_waiters_and_pending_references_unchanged() {
    let mut vm = Runtime::new(snapshot(json!({"$ref":"pending"})), 32).unwrap();
    execute(
        &mut vm,
        r#"
        p=game:GetService('Players').LocalPlayer; events={}
        p.CharacterAdded:Connect(function(c) table.insert(events,c.Name) end)
        task.spawn(function() waited=p.CharacterAdded:Wait() end)
    "#,
    );
    let failure = vm
        .call(
            json!({"op":"update","delta":{"version":1,"delta":true,"sequence":3,"actions":[
                model("wrong",1), character(Some("wrong"),2), {"kind":"unsupported","sequence":3}
            ]}}),
        )
        .unwrap();
    assert_eq!(failure["ok"], false, "{failure}");
    assert_eq!(
        execute(&mut vm, "return #events, waited==nil, p.Character==nil"),
        json!([0.0, true, true])
    );
    update(&mut vm, vec![model("pending", 1)]);
    assert_eq!(
        execute(
            &mut vm,
            "return table.concat(events,','), waited==p.Character"
        ),
        json!(["pending", true])
    );
}

#[test]
fn local_character_changes_validate_models_and_reparenting_has_player_events() {
    let mut vm = Runtime::new(snapshot(Value::Null), 32).unwrap();
    execute(
        &mut vm,
        r#"
        players=game:GetService('Players'); p=players.LocalPlayer; events={}
        p.CharacterAdded:Connect(function(c) table.insert(events,'+'..c.Name) end)
        p.CharacterRemoving:Connect(function(c) table.insert(events,'-'..c.Name) end)
        players.PlayerAdded:Connect(function(who) assert(who==p); table.insert(events,'join') end)
        players.PlayerRemoving:Connect(function(who,reason)
            assert(who==p and reason==Enum.PlayerExitReason.Unknown); table.insert(events,'leave')
        end)
        a=Instance.new('Model'); a.Name='local'
        p.Character=a; p.Character=a
        for _,bad in ipairs({Instance.new('Part'), false, 'local', 1, {}}) do
            assert(not pcall(function() p.Character=bad end)); assert(p.Character==a)
        end
        p.Character=nil; p.Parent=nil; p.Parent=players
    "#,
    );
    assert_eq!(
        execute(&mut vm, "return table.concat(events,',')"),
        json!(["+local,-local,leave,join"])
    );
    execute(&mut vm, "p:Destroy()");
    assert_eq!(
        execute(&mut vm, "return table.concat(events,',')"),
        json!(["+local,-local,leave,join,leave"])
    );
}

#[test]
fn explicit_lifecycle_events_are_not_doubled_and_keep_the_received_exit_reason() {
    let mut vm = Runtime::new(snapshot(Value::Null), 32).unwrap();
    execute(
        &mut vm,
        r#"
        players=game:GetService('Players'); p=players.LocalPlayer; added=0; leaving=0
        p.CharacterAdded:Connect(function(c) assert(c==p.Character); added+=1 end)
        players.PlayerRemoving:Connect(function(who,why) leaving+=1; reason=why; assert(who==p) end)
    "#,
    );
    update(
        &mut vm,
        vec![
            model("char", 1),
            character(Some("char"), 2),
            json!({
                "kind":"event","sequence":3,"id":"p","name":"CharacterAdded","arguments":[{"$ref":"char"}]
            }),
        ],
    );
    assert_eq!(execute(&mut vm, "return added"), json!([1.0]));
    update(
        &mut vm,
        vec![
            json!({"kind":"set","sequence":4,"id":"p","fields":{"Parent":null}}),
            json!({
                "kind":"event","sequence":5,"id":"players","name":"PlayerRemoving",
                "arguments":[{"$ref":"p"},{"$enum":{"type":"PlayerExitReason","name":"CreatorKick","value":2}}]
            }),
        ],
    );
    assert_eq!(
        execute(
            &mut vm,
            "return leaving, reason==Enum.PlayerExitReason.CreatorKick"
        ),
        json!([1.0, true])
    );
    // An explicit event with no paired state transition is still delivered.
    update(
        &mut vm,
        vec![
            json!({"kind":"event","sequence":6,"id":"p","name":"CharacterAdded","arguments":[{"$ref":"char"}]}),
        ],
    );
    assert_eq!(execute(&mut vm, "return added"), json!([2.0]));
}

#[test]
fn shared_world_compact_creates_resolve_characters_without_replaying_existing_joins() {
    use headless_luau::{
        engine_world_projection::ProjectionContext,
        engine_world_view::SharedWorldView,
        live_runtime::{compact_delta, compact_snapshot},
    };
    use rove_client_core::{schema::Schema, world::WorldStore};
    let initial = snapshot(json!({"$ref":"char"}));
    let mut store = WorldStore::from_snapshot(&initial).unwrap();
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
    let projection = ProjectionContext::new(&schema, std::collections::HashMap::new()).unwrap();
    let view = SharedWorldView::projected(store.view(), projection.clone());
    let mut compact = initial;
    compact_snapshot(&mut compact, &view).unwrap();
    let mut vm = Runtime::new_with_shared_world(compact, 32, &view).unwrap();
    execute(
        &mut vm,
        r#"
        players=game:GetService('Players'); p=players.LocalPlayer; joined=0; spawned=0
        players.PlayerAdded:Connect(function() joined+=1 end)
        p.CharacterAdded:Connect(function(c) spawned+=1; assert(c==p.Character) end)
    "#,
    );
    let mut delta = json!({"version":1,"delta":true,"sequence":2,"actions":[model("char",1),{
        "kind":"create","sequence":2,"node":{"id":"second","class_name":"Player","name":"Second","parent":"players",
            "properties":{"Character":{"$ref":"char"}}}
    }]});
    store.apply_delta(&delta).unwrap();
    let view = SharedWorldView::projected(store.view(), projection);
    assert_eq!(compact_delta(&mut delta, &view).unwrap(), 2);
    let reply = vm
        .call_with_shared_world(json!({"op":"update","delta":delta,"timeout_ms":100}), &view)
        .unwrap();
    successful(&reply).unwrap();
    assert_eq!(
        execute(
            &mut vm,
            "return joined, spawned, players.Second.Character==p.Character"
        ),
        json!([1.0, 1.0, true])
    );
}

#[test]
fn actor_sync_and_local_changes_keep_event_subscriptions_in_each_vm() {
    use headless_luau::rust_host::{
        self,
        lua_api::Lua,
        scheduler::{self, Scheduler},
        world::{self, World},
    };
    use std::{
        cell::RefCell,
        rc::Rc,
        time::{Duration, Instant},
    };
    let mut initial = snapshot(Value::Null);
    initial["nodes"]
        .as_array_mut()
        .unwrap()
        .push(model("char", 0)["node"].clone());
    let root = Rc::new(RefCell::new(World::from_snapshot(&initial, None).unwrap()));
    let actor = Rc::new(RefCell::new(root.borrow().fork_for_actor("actor")));
    let setup = |ctx: &world::WorldRef| {
        let lua = Lua::new();
        let sched = Rc::new(RefCell::new(Scheduler::new()));
        scheduler::install(&lua, &sched).unwrap();
        rust_host::install_bindings(&lua, ctx).unwrap();
        lua.load(
            r#"
            p=game:GetService('Players').LocalPlayer; events={}
            p.CharacterAdded:Connect(function(c) table.insert(events,'+'..c.Name) end)
            p.CharacterRemoving:Connect(function(c) table.insert(events,'-'..c.Name) end)
        "#,
        )
        .exec()
        .unwrap();
        (lua, sched)
    };
    let (root_lua, root_sched) = setup(&root);
    let (actor_lua, actor_sched) = setup(&actor);
    root.borrow_mut().effects.clear();
    world::set_property(&root_lua, &root, "p", "Character", json!({"$ref":"char"})).unwrap();
    let journal = root.borrow_mut().effects.remove("mutations").unwrap();
    world::sync_actor_fork_with_lua(&actor_lua, &actor, &root, journal.as_array().unwrap())
        .unwrap();
    for (lua, sched) in [(&root_lua, &root_sched), (&actor_lua, &actor_sched)] {
        scheduler::step(lua, sched, Instant::now(), Duration::from_millis(100));
        assert_eq!(
            lua.load("return table.concat(events,',')")
                .eval::<String>()
                .unwrap(),
            "+char"
        );
    }
    world::set_property(&actor_lua, &actor, "p", "Character", Value::Null).unwrap();
    scheduler::step(
        &actor_lua,
        &actor_sched,
        Instant::now(),
        Duration::from_millis(100),
    );
    assert_eq!(
        actor_lua
            .load("return table.concat(events,',')")
            .eval::<String>()
            .unwrap(),
        "+char,-char"
    );
    assert_eq!(
        root_lua
            .load("return table.concat(events,',')")
            .eval::<String>()
            .unwrap(),
        "+char"
    );
}

#[test]
fn wait_for_child_handles_local_renames_reparents_timeout_and_parent_destruction() {
    let mut vm = Runtime::new(snapshot(Value::Null), 32).unwrap();
    execute(
        &mut vm,
        r#"
        parent=Instance.new('Folder'); nested=Instance.new('Folder',parent)
        child=Instance.new('Part',nested); child.Name='wanted'
        assert(parent:WaitForChild('wanted',0)==nil) -- direct children only
        assert(nested:WaitForChild('wanted',0)==child)
        for _,timeout in ipairs({-1,math.huge,0/0}) do
            assert(not pcall(function() parent:WaitForChild('wanted',timeout) end))
        end
        task.spawn(function() reparented=parent:WaitForChild('wanted',1) end)
        task.spawn(function() renamed=parent:WaitForChild('renamed',1) end)
        task.spawn(function() timedOut=parent:WaitForChild('missing',0.02)==nil end)
    "#,
    );
    execute(&mut vm, "child.Parent=parent");
    assert_eq!(
        execute(&mut vm, "return reparented==child, renamed==nil"),
        json!([true, true])
    );
    execute(&mut vm, "child.Name='renamed'");
    assert_eq!(execute(&mut vm, "return renamed==child"), json!([true]));
    std::thread::sleep(std::time::Duration::from_millis(35));
    let poll = vm.call(json!({"op":"poll","timeout_ms":100})).unwrap();
    successful(&poll).unwrap();
    assert!(
        poll["task_errors"]
            .as_array()
            .is_none_or(|errors| errors.is_empty()),
        "{poll}"
    );
    assert_eq!(execute(&mut vm, "return timedOut"), json!([true]));
    execute(
        &mut vm,
        "task.spawn(function() destroyedWait=parent:WaitForChild('never')==nil end)",
    );
    execute(&mut vm, "parent:Destroy()");
    assert_eq!(execute(&mut vm, "return destroyedWait"), json!([true]));
}

#[test]
fn a_reflected_spawn_waits_for_its_missing_model_and_a_departed_player_cannot_spawn_late() {
    let mut vm = Runtime::new(snapshot(Value::Null), 32).unwrap();
    execute(
        &mut vm,
        r#"
        p=game:GetService('Players').LocalPlayer; spawned=0
        p.CharacterAdded:Connect(function(c) assert(c and c:IsA('Model')); spawned+=1 end)
        task.spawn(function() waited=p.CharacterAdded:Wait() end)
    "#,
    );
    update(
        &mut vm,
        vec![
            character(Some("late"), 1),
            json!({
                "kind":"event","sequence":2,"id":"p","name":"CharacterAdded","arguments":[{"$ref":"late"}]
            }),
        ],
    );
    assert_eq!(
        execute(&mut vm, "return waited==nil,spawned"),
        json!([true, 0.0])
    );
    update(&mut vm, vec![model("late", 3)]);
    assert_eq!(
        execute(&mut vm, "return waited==p.Character,spawned"),
        json!([true, 1.0])
    );
    update(
        &mut vm,
        vec![
            character(Some("never"), 4),
            json!({"kind":"destroy","sequence":5,"id":"p"}),
        ],
    );
    update(&mut vm, vec![model("never", 6)]);
    assert_eq!(execute(&mut vm, "return spawned"), json!([1.0]));
}

#[test]
fn lifecycle_uses_the_same_property_precedence_as_the_world_transaction() {
    let mut vm = Runtime::new(snapshot(Value::Null), 32).unwrap();
    execute(
        &mut vm,
        r#"
        p=game:GetService('Players').LocalPlayer; events={}
        p.CharacterAdded:Connect(function(c) assert(c==p.Character); table.insert(events,c.Name) end)
    "#,
    );
    update(
        &mut vm,
        vec![
            model("a", 1),
            model("b", 2),
            json!({
                "kind":"set","sequence":3,"id":"p","fields":{"Character":{"$ref":"a"}},
                "property":"Character","value":{"$ref":"b"}
            }),
        ],
    );
    assert_eq!(
        execute(&mut vm, "return table.concat(events,','), p.Character.Name"),
        json!(["b", "b"])
    );
}
