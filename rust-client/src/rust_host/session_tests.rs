use super::*;

#[test]
fn authored_environment_is_an_overlay_over_persistent_executor_globals() {
    let mut session = Session::new();
    command(&mut session, json!({"op":"init","memory_mb":32,"snapshot":{
        "version":1,"sequence":0,"nodes":[
            {"id":"game","class_name":"DataModel","name":"game"}
        ]}}));
    let result = command(&mut session, json!({"op":"execute","source":r#"
        local root = getgenv()
        assert(root ~= getfenv(0))
        root.sharedValue = 'root'
        getfenv().threadValue = 'thread'
        assert(root.threadValue == nil)
        assert(sharedValue == 'root')
        sharedValue = 'shadow'
        assert(sharedValue == 'shadow' and root.sharedValue == 'root')
        sharedValue = nil
        assert(sharedValue == 'root')
        getfenv(getgenv).probe = true
        assert(getgenv().probe == nil)
        local nested = assert(loadstring('return getgenv(), sharedValue'))
        local nestedRoot, nestedValue = nested()
        assert(nestedRoot == root and nestedValue == 'root')
        return true
    "#}));
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["values"], json!([true]), "{result}");

    let later = command(&mut session, json!({"op":"execute","source":
        "assert(getgenv().sharedValue == 'root'); assert(getgenv().threadValue == nil); assert(threadValue == 'thread'); return true"}));
    assert_eq!(later["ok"], true, "{later}");
    assert_eq!(later["values"], json!([true]), "{later}");
}

#[test]
fn received_archivable_alias_tracks_server_changes_in_shared_and_eager_worlds() {
    use crate::{engine_world_projection::ProjectionContext, engine_world_view::SharedWorldView};
    use rove_client_core::{schema::Schema, world::WorldStore};
    use std::collections::HashMap;
    for (shared, omitted) in [(false,false),(true,false),(false,true),(true,true)] {
        let mut store=WorldStore::from_snapshot(&json!({"version":1,"sequence":1,"nodes":[
            {"id":"game","class_name":"DataModel","name":"game"},
            {"id":"p","class_name":"Part","name":"Display","parent":"game","received_creation_complete":true,
             "properties":if omitted {json!({})} else {json!({"archivable":true,"Transparency":0.4})}},
            {"id":"q","class_name":"Part","name":"Unknown","parent":"game","properties":{}}
        ]})).unwrap();
        let schema=Schema {version:3,flags:0,enums:vec![],classes:vec![],properties:vec![],events:vec![],strings:vec![],names:vec![],lr_types:vec![],parent_property_id:0,decoded_bytes:0};
        let context=ProjectionContext::new(&schema,HashMap::new()).unwrap();
        let view=SharedWorldView::projected(store.view(),context.clone());
        let mut session=Session::new();
        session.request_value_with_world(&json!({"op":"init","memory_mb":32,"snapshot":store.to_snapshot()}),shared.then_some(&view)).unwrap();
        let initial=command(&mut session,json!({"op":"execute","source":
            format!("p=game.Display;assert(p.Archivable and p.archivable);assert(p.Transparency=={});assert(not pcall(function() return game.Unknown.Archivable end));assert(not pcall(function() return game.Unknown.Transparency end)); changes=0;p:GetPropertyChangedSignal('Archivable'):Connect(function() changes+=1 end)", if omitted {0.0} else {0.4})}));
        assert_eq!(initial["ok"],true,"{initial}");
        for sequence in 2..=3 {
            if sequence==3 {
                let changed=command(&mut session,json!({"op":"execute","source":"p.Archivable=true;assert(p.Archivable)"}));
                assert_eq!(changed["ok"],true,"{changed}");
            }
            let delta=json!({"version":1,"delta":true,"sequence":sequence,"actions":[
                {"kind":"set","sequence":sequence,"id":"p","fields":{"archivable":false}}
            ]});
            store.apply_delta(&delta).unwrap();
            let next=SharedWorldView::projected(store.view(),context.clone());
            let update=session.request_value_with_world(&json!({"op":"update","timeout_ms":50,"delta":delta}),shared.then_some(&next)).unwrap();
            assert_eq!(update["ok"],true,"{update}");
            let result=command(&mut session,json!({"op":"execute","source":"return p.Archivable,p.archivable,changes"}));
            assert_eq!(result["values"],json!([false,false,if sequence==2 {1.} else {3.}]),"{result}");
        }
    }
    assert!(super::super::world_aliases::from_wire("Part","archivable",&json!("false")).is_err());
}

#[test]
fn answered_root_invocations_do_not_exhaust_unbound_actor_queues() {
    let mut session=Session::new();
    command(&mut session,json!({"op":"init","memory_mb":32,"snapshot":{
        "version":1,"sequence":0,"nodes":[
            {"id":"game","class_name":"DataModel","name":"game"},
            {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game"},
            {"id":"actor","class_name":"Actor","name":"actor","parent":"ws"},
            {"id":"remote","class_name":"RemoteFunction","name":"F","parent":"ws","server_owned":true}
        ]}}));
    {
        let vm=session.vm.as_ref().unwrap();
        let pool=vm.lua.app_data_ref::<super::super::shared_tables::SharedTableRegistry>().unwrap().clone();
        super::super::actors::ensure_actor(&vm.lua,&vm.world,&pool,"actor").unwrap();
    }
    let result=command(&mut session,json!({"op":"execute","source":
        "workspace.F.OnClientInvoke=function(value) return value+1 end"}));
    assert_eq!(result["ok"],true,"{result}");
    for request in 1..=520 {
        let result=command(&mut session,json!({"op":"update","timeout_ms":50,"delta":{
            "version":1,"delta":true,"sequence":request,"actions":[
                {"kind":"event","sequence":request,"id":"remote","name":"RemoteOnInvokeClient",
                 "arguments":[request,[request]]}
            ]}}));
        assert_eq!(result["ok"],true,"request {request}: {result}");
        let replies=result["remote_client_replies"].as_array().unwrap();
        assert_eq!(replies.len(),1,"request {request}: {result}");
        assert_eq!(replies[0]["request_id"],request);
        assert_eq!(replies[0]["ok"],true);
    }
}

#[test]
fn fresh_received_tag_defaults_reach_the_shared_vm_without_hiding_unknown_data() {
    use crate::{engine_world_projection::ProjectionContext, engine_world_view::SharedWorldView};
    use rove_client_core::{schema::Schema, world::WorldStore};
    use std::collections::HashMap;
    let mut store=WorldStore::from_snapshot(&json!({"version":1,"sequence":0,"nodes":[
        {"id":"game","class_name":"DataModel","name":"game"}
    ]})).unwrap();
    store.apply_delta(&json!({"version":1,"delta":true,"sequence":2,"actions":[
        {"sequence":1,"kind":"create","id":"p","node":{"id":"p","class_name":"Part","name":"Probe",
            "parent":"game","received_creation_complete":true,"properties":{}}},
        {"sequence":2,"kind":"create","id":"q","node":{"id":"q","class_name":"Part","name":"Unknown",
            "parent":"game","received_creation_complete":true,"properties":{"Tags":{"$bytes_hex":"ff"}}}}
    ]})).unwrap();
    let schema=Schema {version:3,flags:0,enums:vec![],classes:vec![],properties:vec![],events:vec![],strings:vec![],names:vec![],lr_types:vec![],parent_property_id:0,decoded_bytes:0};
    let context=ProjectionContext::new(&schema,HashMap::new()).unwrap();
    let view=SharedWorldView::projected(store.view(),context);
    for shared in [None,Some(&view)] {
        let mut session=Session::new();
        session.request_value_with_world(&json!({"op":"init","memory_mb":32,"snapshot":store.to_snapshot()}),shared).unwrap();
        let result=command(&mut session,json!({"op":"execute","source":r#"
            assert(not game.Probe:HasTag('absent'))
            game.Probe:RemoveTag('absent')
            game.Probe:AddTag('local')
            assert(game.Probe:HasTag('local'))
            assert(not pcall(function() return game.Unknown:HasTag('absent') end))
            assert(not pcall(function() game.Unknown:RemoveTag('absent') end))
            return true
        "#}));
        assert_eq!(result["ok"],true,"{result}");
        assert_eq!(result["values"],json!([true]),"{result}");
    }
}

#[test]
fn message_out_delivers_real_output_once_with_message_types() {
    let mut session = Session::new();
    command(&mut session, json!({"op":"init","memory_mb":32,"snapshot":{
        "version":1,"sequence":0,"nodes":[
            {"id":"game","class_name":"DataModel","name":"game"},
            {"id":"log","class_name":"LogService","name":"LogService","parent":"game"},
            {"id":"folder","class_name":"Folder","name":"Folder","parent":"game"}
        ]}}));
    let result = command(&mut session, json!({"op":"execute","source":r#"
        print('before subscription')
        rows={}
        local signal=game:GetService('LogService').MessageOut
        assert(typeof(signal)=='RBXScriptSignal')
        assert(not pcall(function() return game.Folder.MessageOut end))
        connection=signal:Connect(function(message, kind, context)
            assert(context==nil)
            table.insert(rows,{message,kind.Name})
        end)
        print('hello',5)
        warn('careful')
        printidentity('identity')
    "#}));
    assert_eq!(result["ok"], true, "{result}");
    for _ in 0..3 { command(&mut session, json!({"op":"poll","timeout_ms":25})); }
    let result = command(&mut session, json!({"op":"execute","source":
        "connection:Disconnect(); print('after disconnect')"}));
    assert_eq!(result["ok"], true, "{result}");
    session.vm.as_ref().unwrap().lua.load(r#"
        assert(#rows==3)
        assert(rows[1][1]=='hello\t5' and rows[1][2]=='MessageOutput')
        assert(rows[2][1]=='careful' and rows[2][2]=='MessageWarning')
        assert(string.sub(rows[3][1],1,9)=='identity ' and rows[3][2]=='MessageOutput')
    "#).set_environment(session.vm.as_ref().unwrap().authored_environment.clone()).exec().unwrap();
    command(&mut session, json!({"op":"poll","timeout_ms":25}));
    assert_eq!(session.vm.as_ref().unwrap().lua.load("return #rows").set_environment(session.vm.as_ref().unwrap().authored_environment.clone()).eval::<i64>().unwrap(),3);
}

#[test]
fn message_out_preserves_binary_strings_while_console_uses_valid_text() {
    let mut session = Session::new();
    command(&mut session, json!({"op":"init","memory_mb":32,"snapshot":{
        "version":1,"sequence":0,"nodes":[
            {"id":"game","class_name":"DataModel","name":"game"},
            {"id":"log","class_name":"LogService","name":"LogService","parent":"game"}
        ]}}));
    let result = command(&mut session, json!({"op":"execute","source":r#"
        binary_rows={}
        game:GetService('LogService').MessageOut:Connect(function(message, kind)
            table.insert(binary_rows,{message,kind.Name})
        end)
        local bytes=string.char(255,0,128)
        print(bytes,'hello')
        warn(setmetatable({}, {__tostring=function() return bytes end}))
        print()
        print('caf\195\169')
        assert(not pcall(print,setmetatable({}, {__tostring=function() error('expected') end})))
        return 'continued'
    "#}));
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["values"], json!(["continued"]), "{result}");
    let output = result["output"].as_str().unwrap();
    assert!(output.contains('\u{fffd}'), "{result}");
    assert!(output.contains("café"), "{result}");
    for _ in 0..3 { command(&mut session, json!({"op":"poll","timeout_ms":25})); }
    session.vm.as_ref().unwrap().lua.load(r#"
        assert(#binary_rows==4)
        local bytes=string.char(255,0,128)
        assert(binary_rows[1][1]==bytes..'\thello' and binary_rows[1][2]=='MessageOutput')
        assert(binary_rows[2][1]==bytes and binary_rows[2][2]=='MessageWarning')
        assert(binary_rows[3][1]=='')
        assert(binary_rows[4][1]=='caf\195\169')
    "#).set_environment(session.vm.as_ref().unwrap().authored_environment.clone()).exec().unwrap();
}

#[test]
fn message_out_listener_can_log_without_recursive_borrows() {
    let mut session = Session::new();
    command(&mut session, json!({"op":"init","memory_mb":32,"snapshot":{
        "version":1,"sequence":0,"nodes":[
            {"id":"game","class_name":"DataModel","name":"game"},
            {"id":"log","class_name":"LogService","name":"LogService","parent":"game"}
        ]}}));
    let result=command(&mut session,json!({"op":"execute","source":r#"
        count=0
        game:GetService('LogService').MessageOut:Connect(function(message)
            count+=1
            if message=='outer' then print('inner') end
        end)
        print('outer')
    "#}));
    assert_eq!(result["ok"],true,"{result}");
    for _ in 0..3 { command(&mut session,json!({"op":"poll","timeout_ms":25})); }
    assert_eq!(session.vm.as_ref().unwrap().authored_environment.get::<i64>("count").unwrap(),2);
}

#[test]
fn bounded_receive_batch_keeps_each_view_and_delivers_notifications_on_final_step() {
    use crate::{engine_world_projection::ProjectionContext, engine_world_view::SharedWorldView};
    use rove_client_core::{schema::Schema, world::WorldStore};
    use std::collections::HashMap;
    let snapshot=json!({"version":1,"sequence":0,"nodes":[
        {"id":"game","class_name":"DataModel","name":"game"},
        {"id":"p","class_name":"Folder","name":"Probe","parent":"game","attributes_known":true,"attributes":{"score":0}}
    ]});
    let schema = Schema {version:3,flags:0,enums:vec![],classes:vec![],properties:vec![],events:vec![],strings:vec![],names:vec![],lr_types:vec![],parent_property_id:0,decoded_bytes:0};
    let context=ProjectionContext::new(&schema,HashMap::new()).unwrap();
    let mut store=WorldStore::from_snapshot(&snapshot).unwrap();
    let view=SharedWorldView::projected(store.view(),context.clone());
    let mut session=Session::new();
    session.request_value_with_world(&json!({"op":"init","memory_mb":32,"snapshot":snapshot}),Some(&view)).unwrap();
    command(&mut session,json!({"op":"execute","source":"count=0; game.Probe:GetAttributeChangedSignal('score'):Connect(function() count+=1 end)"}));
    for sequence in 1..=2 {
        let delta=json!({"version":1,"delta":true,"sequence":sequence,"actions":[
            {"kind":"attribute","id":"p","sequence":sequence,"attribute_update":{"name":"score","known":true,"value":sequence}}
        ]});
        store.apply_delta(&delta).unwrap();
        let current=SharedWorldView::projected(store.view(),context.clone());
        let reply=session.request_value_with_world(&json!({"op":"update","delta":delta,"defer_script_step":sequence==1}),Some(&current)).unwrap();
        assert_eq!(reply["ok"],true,"{reply}");
        let vm=session.vm.as_ref().unwrap();
        assert_eq!(vm.lua.load("return game.Probe:GetAttribute('score')").eval::<i32>().unwrap(),sequence);
        assert_eq!(vm.authored_environment.get::<i32>("count").unwrap(),if sequence==1 {0} else {2});
        assert_eq!(reply["script_step_deferred"]==true,sequence==1);
    }
    assert!(session.vm.as_ref().unwrap().deferred_receipts.is_empty());
    assert_eq!(view.lookup_node("p").unwrap().raw().metadata_field("attributes","score"),Some(json!(0)));
}

fn command(session: &mut Session, request: Json) -> Json {
    serde_json::from_slice(
        &session
            .request(&serde_json::to_vec(&request).unwrap())
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn counted_json_limits_match_wire_bytes_including_escaping_and_unicode() {
    for value in [json!(null), json!({}), json!([1, -0.0, 1.234567890123]),
        json!({"quoted\"key":"line\n\t\u{0}slash\\ and unicode \u{1f600}","array":[true,false,null]})] {
        let encoded = serde_json::to_vec(&value).unwrap();
        assert!(json_fits(&value, encoded.len()));
        assert!(!json_fits(&value, encoded.len() - 1));
    }
    assert!(!json_fits(&json!({"ok":true,"values":["x".repeat(MAX_RESULT)]}), MAX_RESULT));
}

#[test]
fn direct_requests_keep_the_byte_parsers_nesting_limit() {
    for object in [false, true] {
        for depth in 124..=130 {
            let mut value = Json::Null;
            for _ in 0..depth {
                value = if object { json!({"nested":value}) } else { json!([value]) };
            }
            let wire = serde_json::to_vec(&value).unwrap();
            let accepted = serde_json::from_slice::<Json>(&wire).is_ok();
            assert_eq!(json_parser_depth(&value, 128), accepted, "depth={depth}, object={object}");
            if !accepted {
                let error = Session::new().request_value_with_world(&value, None).unwrap_err();
                assert!(error.contains("recursion limit exceeded"));
            }
        }
    }
}

#[test]
fn direct_and_byte_requests_return_the_same_values_and_preserve_session_state() {
    let mut direct = Session::new();
    let mut wire = Session::new();
    for request in [
        json!({"op":"init","snapshot":empty_snapshot("direct-request-test"),"memory_mb":16}),
        json!({"op":"execute","source":"shared.total=7; return shared.total, nil, {yes=true, n=1.25}, Vector3.new(1,2,3)"}),
        json!({"op":"execute","source":"shared.total+=5; return shared.total, 'line\\nquote\\\"'"}),
        json!({"op":"execute","source":"error('ordinary script failure')"}),
        json!({"op":"execute","source":"return shared.total"}),
    ] {
        let a = direct.request_value_with_world(&request, None).unwrap();
        let b = command(&mut wire, request);
        for field in ["ok", "values", "error", "persistent"] {
            assert_eq!(a.get(field), b.get(field), "{field}: direct={a}; wire={b}");
        }
    }
}

#[test]
fn drawings_persist_across_response_drain_and_repeated_render() {
    let mut session = Session::new();
    let initialized = command(
        &mut session,
        json!({"op":"init","snapshot":empty_snapshot("drawings-fixture"),"memory_mb":16}),
    );
    assert_eq!(initialized["ok"], true, "{initialized}");
    let executed = command(
        &mut session,
        json!({"op":"execute","source":"mark = Drawing.new('Circle'); mark.Visible = true; mark.Position = Vector2.new(20,30); mark.Radius = 8"}),
    );
    assert_eq!(executed["ok"], true, "{executed}");
    for _ in 0..2 {
        let rendered = command(&mut session, json!({"op":"render"}));
        assert_eq!(rendered["ok"], true, "{rendered}");
        assert_eq!(
            rendered["frame"]["drawings"].as_array().unwrap().len(),
            1,
            "{rendered}"
        );
        let polled = command(&mut session, json!({"op":"poll"}));
        assert!(
            polled.get("drawing_scene").is_none(),
            "render data leaked into effects: {polled}"
        );
    }
    let removed = command(
        &mut session,
        json!({"op":"execute","source":"mark:Remove()"}),
    );
    assert_eq!(removed["ok"], true, "{removed}");
    let rendered = command(&mut session, json!({"op":"render"}));
    assert!(
        rendered["frame"]["drawings"].as_array().unwrap().is_empty(),
        "{rendered}"
    );
}

#[test]
fn text_service_reply_reaches_the_waiting_persistent_job() {
    let mut snapshot = empty_snapshot("text-fixture");
    snapshot["services"]["TextService"] = json!("text");
    snapshot["nodes"].as_array_mut().unwrap().push(json!({"id":"text","class_name":"TextService","name":"TextService","parent":"game","properties":{}}));
    let mut session = Session::new();
    let initialized = command(
        &mut session,
        json!({"op":"init","snapshot":snapshot,"memory_mb":16}),
    );
    assert_eq!(initialized["ok"], true, "{initialized}");
    let executed = command(
        &mut session,
        json!({"op":"execute","source":"local family = game:GetService('TextService'):GetFamilyInfoAsync('rbxasset://fonts/families/Test.json'); return family.Name, #family.Faces"}),
    );
    assert_eq!(executed["completed"], false, "{executed}");
    let request = &executed["text_service_requests"][0];
    let id = request["request_id"].as_u64().unwrap();
    assert_eq!(
        executed["active_text_service_requests"],
        json!([id]),
        "{executed}"
    );
    let completed = command(
        &mut session,
        json!({"op":"poll","text_service_responses":[{"request_id":id,"result":{"Name":"Fixture font","Faces":[{"Name":"Regular","Weight":400,"Style":0}]}}]}),
    );
    let jobs = completed["completed_jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1, "{completed}");
    assert_eq!(jobs[0]["job_id"], executed["job_id"], "{completed}");
    assert_eq!(
        jobs[0]["values"],
        json!(["Fixture font", 1.0]),
        "{completed}"
    );
    assert_eq!(
        completed["active_text_service_requests"],
        json!([]),
        "{completed}"
    );
}

#[test]
fn interaction_effects_require_received_signature_and_keep_local_signals() {
    let mut snapshot = empty_snapshot("interaction-fixture");
    snapshot["class_event_signatures"] =
        json!({"ProximityPrompt":{"TriggeredActionReplicated":[28]}});
    snapshot["nodes"].as_array_mut().unwrap().extend([
        json!({"id":"prompt","class_name":"ProximityPrompt","name":"Prompt","parent":"workspace","properties":{},"received_creation_complete":true,"server_owned":true}),
        json!({"id":"a","class_name":"Part","name":"A","parent":"workspace","properties":{},"server_owned":true}),
        json!({"id":"b","class_name":"Part","name":"B","parent":"workspace","properties":{},"server_owned":true}),
    ]);
    let mut session = Session::new();
    let initialized = command(
        &mut session,
        json!({"op":"init","snapshot":snapshot,"memory_mb":16}),
    );
    assert_eq!(initialized["ok"], true, "{initialized}");
    let fired = command(
        &mut session,
        json!({"op":"execute","source":"touches=0; prompt_events={}; workspace.Prompt.Triggered:Connect(function() table.insert(prompt_events,'triggered') end); workspace.Prompt.TriggerEnded:Connect(function() table.insert(prompt_events,'ended') end); workspace.A.Touched:Connect(function(other) assert(other==workspace.B); touches+=1 end); fireproximityprompt(workspace.Prompt); firetouchinterest(workspace.A,workspace.B,true)"}),
    );
    assert_eq!(fired["ok"], true, "{fired}");
    assert_eq!(
        fired["remote_server_events"],
        json!([
            {"node":"prompt","event":"TriggeredActionReplicated","argument_types":[28],"arguments":[{"$ref":null}]},
            {"touch":{"first":"a","second":"b","began":true}}
        ])
    );
    let checked = command(
        &mut session,
        json!({"op":"execute","source":"assert(touches==1); assert(#prompt_events==2 and prompt_events[1]=='triggered' and prompt_events[2]=='ended'); assert(not pcall(firetouchinterest,workspace.A,workspace.B,2)); local localPrompt=Instance.new('ProximityPrompt'); fireproximityprompt(localPrompt); return true"}),
    );
    assert_eq!(checked["ok"], true, "{checked}");
    assert_eq!(checked["remote_server_events"], json!([]));
    session.vm.as_mut().unwrap().world.borrow_mut().snapshot["class_event_signatures"] = json!({});
    let rejected = command(
        &mut session,
        json!({"op":"execute","source":"return pcall(fireproximityprompt,workspace.Prompt)"}),
    );
    assert_eq!(rejected["values"][0], false, "{rejected}");
    assert_eq!(rejected["remote_server_events"], json!([]));
}

#[test]
fn fireclickdetector_fires_local_mouse_signals_without_parenting_the_detector() {
    let mut session = Session::new();
    let mut snapshot = empty_snapshot("clickdetector-fixture");
    snapshot["local_player"] = json!("player");
    snapshot["services"]["Players"] = json!("players");
    snapshot["nodes"].as_array_mut().unwrap().extend([
        json!({"id":"players","class_name":"Players","name":"Players","parent":"game","properties":{"LocalPlayer":{"$ref":"player"}},"server_owned":true}),
        json!({"id":"player","class_name":"Player","name":"LocalPlayer","parent":"players","properties":{},"server_owned":true}),
    ]);
    let initialized = command(
        &mut session,
        json!({"op":"init","snapshot":snapshot,"memory_mb":16}),
    );
    assert_eq!(initialized["ok"], true, "{initialized}");
    let fired = command(&mut session, json!({"op":"execute","source":r#"
        local detector = Instance.new("ClickDetector")
        local clicked = false
        detector.MouseClick:Once(function(player)
            clicked = player == game.Players.LocalPlayer
        end)
        fireclickdetector(detector)
        task.wait()
        assert(detector.Parent == nil)
        assert(clicked, "MouseClick listener was not resumed")

        local events = {}
        detector.MouseClick:Connect(function() events.MouseClick = true end)
        detector.RightMouseClick:Connect(function() events.RightMouseClick = true end)
        detector.MouseHoverEnter:Connect(function() events.MouseHoverEnter = true end)
        detector.MouseHoverLeave:Connect(function() events.MouseHoverLeave = true end)
        for _, name in ipairs({"MouseClick", "RightMouseClick", "MouseHoverEnter", "MouseHoverLeave"}) do
            fireclickdetector(detector, 0, name)
            task.wait()
        end
        for _, name in ipairs({"MouseClick", "RightMouseClick", "MouseHoverEnter", "MouseHoverLeave"}) do
            assert(events[name], "listener did not fire for " .. name)
        end
        return true
    "#}));
    assert_eq!(fired["ok"], true, "{fired}");
    assert_eq!(fired["remote_server_events"], json!([]), "{fired}");
    let job_id = fired["job_id"].as_u64().unwrap();
    let completion = |reply: &Json| {
        reply["completed_jobs"].as_array().and_then(|rows| {
            rows.iter().find(|row| row["job_id"].as_u64() == Some(job_id)).cloned()
        })
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut finished = completion(&fired);
    while finished.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
        let reply = command(&mut session, json!({"op":"poll","timeout_ms":10}));
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(reply["remote_server_events"], json!([]), "{reply}");
        finished = completion(&reply);
    }
    let finished = finished.expect("click detector job did not finish its authored waits");
    assert_eq!(finished["ok"], true, "{finished}");
    assert_eq!(finished["values"], json!([true]), "{finished}");
}

#[test]
fn proximity_prompt_hold_emits_begin_wait_end_and_only_then_replicates_trigger() {
    let mut snapshot = empty_snapshot("prompt-hold-fixture");
    snapshot["class_event_signatures"] = json!({"ProximityPrompt":{"TriggeredActionReplicated":[28]}});
    snapshot["nodes"].as_array_mut().unwrap().push(json!({
        "id":"prompt","class_name":"ProximityPrompt","name":"Prompt","parent":"workspace",
        "received_creation_complete":true,"properties":{"HoldDuration":0.06},"server_owned":true
    }));
    let mut session = Session::new();
    let initialized = command(&mut session, json!({"op":"init","snapshot":snapshot,"memory_mb":16}));
    assert_eq!(initialized["ok"], true, "{initialized}");
    let started = command(&mut session, json!({"op":"execute","source":r#"
        prompt_events={}
        workspace.Prompt.PromptButtonHoldBegan:Connect(function() table.insert(prompt_events,'begin') end)
        workspace.Prompt.PromptButtonHoldEnded:Connect(function() table.insert(prompt_events,'hold-ended') end)
        workspace.Prompt.Triggered:Connect(function() table.insert(prompt_events,'triggered') end)
        workspace.Prompt.TriggerEnded:Connect(function() table.insert(prompt_events,'ended') end)
        task.spawn(function() fireproximityprompt(workspace.Prompt); prompt_done=true end)
    "#}));
    assert_eq!(started["ok"], true, "{started}");
    assert_eq!(started["remote_server_events"], json!([]), "must not report a trigger before its hold completes");
    std::thread::sleep(std::time::Duration::from_millis(25));
    let waiting = command(&mut session, json!({"op":"poll","timeout_ms":10}));
    assert_eq!(waiting["remote_server_events"], json!([]), "{waiting}");
    std::thread::sleep(std::time::Duration::from_millis(60));
    let completed = command(&mut session, json!({"op":"poll","timeout_ms":10}));
    assert_eq!(completed["remote_server_events"], json!([
        {"node":"prompt","event":"TriggeredActionReplicated","argument_types":[28],"arguments":[{"$ref":null}]}
    ]), "{completed}");
    let checked = command(&mut session, json!({"op":"execute","source":"assert(prompt_done); return table.concat(prompt_events, ',')"}));
    assert_eq!(checked["values"], json!(["begin,hold-ended,triggered,ended"]), "{checked}");
    assert_eq!(checked["remote_server_events"], json!([]), "the local signal result is not a server acknowledgement");
}

#[test]
fn compiler_owned_program_uses_bounded_vm_and_current_context() {
    let bytes = include_bytes!("../../../tests/fixtures/luau-v9/calculation.luac");
    let result =
        run_program_bytes(bytes, &[2., 5.], "00000000-0000-4000-8000-000000000001").unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["values"], json!([8 ^ 0x1234, "owned-v9"]));
    assert_eq!(result["runtime_protected_integrity"]["failures"], 0);
    assert!(
        result["runtime_protected_integrity"]["constructions"]
            .as_u64()
            .unwrap()
            > 0
    );
    let context = include_bytes!("../../../tests/fixtures/luau-session-context/source.luac");
    for suffix in [1, 2] {
        let job = format!("00000000-0000-4000-8000-{suffix:012}");
        let result = run_program_bytes(context, &[7., 11.], &job).unwrap();
        assert_eq!(result["values"], json!([job, false, 7, 11]), "{result}");
    }
}

#[test]
fn program_context_has_no_authored_io_capabilities() {
    let bytes=mlua::chunk::Compiler::new().compile(b"return request==nil, readfile==nil, loadstring==nil, debug==nil, setclipboard==nil, game:FindService('HttpService')==nil").unwrap();
    let result =
        run_program_bytes(&bytes, &[1., 2.], "00000000-0000-4000-8000-000000000001").unwrap();
    assert_eq!(
        result["values"],
        json!([true, true, true, true, true, true]),
        "{result}"
    );
    assert!(run_program_bytes(&bytes, &[1., 2.], "unissued-session").is_err());
    assert!(run_program_bytes(
        &bytes,
        &[f64::NAN, 2.],
        "00000000-0000-4000-8000-000000000001"
    )
    .is_err());
}

#[test]
fn verified_program_has_seeded_random_and_session_local_globals() {
    let bytes = mlua::chunk::Compiler::new()
        .compile(br#"
            local a, b = ...
            program_seed = a + b
            local random = Random.new(program_seed)
            local copy = random:Clone()
            local draw = random:NextInteger(1, 2147483647)
            assert(draw == copy:NextInteger(1, 2147483647))
            assert(not pcall(function() Random.new = function() end end))
            return draw, game.JobId, typeof(random),
                DateTime == nil, request == nil, loadstring == nil
        "#)
        .unwrap();
    let first = run_program_bytes(&bytes, &[2., 5.], "00000000-0000-4000-8000-000000000001").unwrap();
    let second = run_program_bytes(&bytes, &[7., 11.], "00000000-0000-4000-8000-000000000002").unwrap();
    for (result, job) in [(&first, "00000000-0000-4000-8000-000000000001"), (&second, "00000000-0000-4000-8000-000000000002")] {
        assert_eq!(result["ok"], true, "{result}");
        let values = result["values"].as_array().unwrap();
        assert!((1..=2_147_483_647).contains(&values[0].as_i64().unwrap()));
        assert_eq!(&values[1..], &[json!(job), json!("Random"), json!(true), json!(true), json!(true)]);
        assert_eq!(result["runtime_protected_integrity"]["failures"], 0);
    }
    assert_ne!(first["values"][0], second["values"][0]);
}

#[test]
fn request_requires_init_and_retains_a_single_vm() {
    let mut session = Session::new();
    assert!(session.request(br#"{"op":"poll"}"#).is_err());
    let init = serde_json::to_vec(
        &json!({"op":"init","snapshot":empty_snapshot("offline-fixture-session"),"memory_mb":16}),
    )
    .unwrap();
    let result: Json = serde_json::from_slice(&session.request(&init).unwrap()).unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert!(session.request(&init).is_err());
    let result: Json = serde_json::from_slice(
        &session
            .request(br#"{"op":"execute","source":"error('deadline is just text')"}"#)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["ok"], false, "{result}");
    assert_eq!(result["timed_out"], false, "{result}");
    let result: Json = serde_json::from_slice(
        &session
            .request(br#"{"op":"execute","source":"return 6 * 7"}"#)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["values"][0].as_f64(), Some(42.));
}
