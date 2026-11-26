//! Receive-only capture replay into the official Luau VM. Runs only the fixed
//! observer below, never captured scripts or authentication/integrity replies.
use headless_luau::{
    capture_replay,
    engine_world_projection::ProjectionContext,
    engine_world_view::SharedWorldView,
    live_runtime::{compact_delta_with_candidates, compact_snapshot},
    login,
    rust_host::{
        self,
        lua_api::Lua,
        remotes,
        scheduler::{self, Scheduler},
        world::{self, World},
    },
};
use rove_client_core::{
    runtime_delta::RuntimeDeltaNormalizer,
    runtime_snapshot::{build_runtime_snapshot, RuntimeSnapshotMetadata},
    session::{IngestOutcome, SessionCore},
};
use serde_json::json;
use std::{
    cell::RefCell,
    fs::File,
    io::BufReader,
    rc::Rc,
    time::{Duration, Instant},
};

const OBSERVER: &str = r#"
    local players=game:GetService('Players')
    counts={initialPlayers=0,initialCharacters=0,joined=0,left=0,spawned=0,removed=0,roots=0}
    local watched={}
    local function watch(player,initial)
        if watched[player] then return end
        watched[player]=true
        if initial then
            counts.initialPlayers+=1
            if player.Character then counts.initialCharacters+=1 end
        end
        player.CharacterAdded:Connect(function(character)
            assert(typeof(character)=='Instance' and character:IsA('Model'))
            counts.spawned+=1
            local root=character:WaitForChild('HumanoidRootPart',0)
            if root then counts.roots+=1 end
        end)
        player.CharacterRemoving:Connect(function(character)
            assert(typeof(character)=='Instance' and character:IsA('Model'))
            counts.removed+=1
        end)
    end
    players.PlayerAdded:Connect(function(player) counts.joined+=1; watch(player,false) end)
    players.PlayerRemoving:Connect(function(player,reason)
        assert(typeof(player)=='Instance' and player:IsA('Player'))
        assert(reason==Enum.PlayerExitReason.Unknown or typeof(reason)=='EnumItem')
        counts.left+=1; watched[player]=nil
    end)
    for _,player in ipairs(players:GetPlayers()) do watch(player,true) end
"#;

fn main() -> Result<(), String> {
    let args = std::env::args_os().collect::<Vec<_>>();
    if args.len() != 3 {
        return Err("usage: replay_player_lifecycle_capture CAPTURE REPORT".into());
    }
    let mut core = SessionCore::with_limits(8, 32 * 1024 * 1024).map_err(|e| e.to_string())?;
    let reader = core.retain(0).map_err(|e| e.to_string())?;
    let mut acknowledged = 0;
    let mut runtime = None;
    let mut observed = false;
    let mut applied = 0;
    let mut task_errors = 0;
    let mut explicit_events = 0;
    let started = Instant::now();
    let rows = capture_replay::read_instance_state(
        BufReader::new(File::open(&args[1]).map_err(|e| e.to_string())?),
        |payload, _| {
            let before = core.world_view();
            let outcome = match payload[0] {
                147 => {
                    let config =
                        login::feature_configuration(payload).map_err(|e| e.to_string())?;
                    core.set_instance_state_legacy_pv(config.iter().any(|(key, value)| {
                        key == "DebugAdaptivePVCompressor" && value == "false"
                    }));
                    None
                }
                161 => core
                    .ingest_instance_state(payload)
                    .map_err(|e| e.to_string())?,
                131 if core.scope().is_none() => {
                    headless_luau::ping_responder::decode_ping_packet(payload)
                        .map_err(|e| format!("pre-world ping: {e:?}"))?;
                    None
                }
                _ => Some(core.ingest(payload).map_err(|e| e.to_string())?),
            };
            let sequence = match outcome {
                Some(
                    IngestOutcome::GlobalsCommitted {
                        packet_sequence, ..
                    }
                    | IngestOutcome::ReplicationCommitted {
                        packet_sequence, ..
                    },
                ) => packet_sequence,
                _ => return Ok(()),
            };
            let entries = core
                .read_after(reader, acknowledged, 1)
                .map_err(|e| e.to_string())?;
            let entry = entries.first().ok_or("missing receipt")?;
            if runtime.is_none() {
                let schema = core.schema().ok_or("missing schema")?;
                let mut normalizer = RuntimeDeltaNormalizer::new(schema)?;
                let mut snapshot = build_runtime_snapshot(
                    schema,
                    &core.snapshot().ok_or("missing world")?,
                    &RuntimeSnapshotMetadata::default(),
                )?;
                normalizer.bind_snapshot_services(&snapshot)?;
                let projection = ProjectionContext::from_normalizer(&normalizer);
                let view = SharedWorldView::projected(core.world_view().unwrap(), projection);
                compact_snapshot(&mut snapshot, &view)?;
                let ctx = Rc::new(RefCell::new(World::from_snapshot(&snapshot, Some(&view))?));
                let lua = Lua::new();
                let sched = Rc::new(RefCell::new(Scheduler::new()));
                scheduler::install(&lua, &sched).map_err(|e| e.to_string())?;
                rust_host::install_bindings(&lua, &ctx).map_err(|e| e.to_string())?;
                runtime = Some((lua, ctx, sched, normalizer));
            } else {
                let (lua, ctx, _, normalizer) = runtime.as_mut().unwrap();
                let post = core.world_view().unwrap();
                let mut normalized = normalizer.normalize_for_shared_view_worker(
                    &before.ok_or("missing previous world")?,
                    &post,
                    &entry.value,
                    false,
                )?;
                let view = SharedWorldView::projected(
                    post,
                    ProjectionContext::from_normalizer(normalizer),
                );
                compact_delta_with_candidates(
                    &mut normalized.worker_delta,
                    &view,
                    &normalized.compact_create_ids,
                )?;
                let previous = ctx.borrow().sequence;
                world::apply_delta_with_lua(lua, ctx, &normalized.worker_delta, Some(&view))
                    .map_err(|e| e.to_string())?;
                explicit_events += normalized.worker_delta["actions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|row| {
                        row["kind"] == "event"
                            && matches!(
                                row["name"].as_str(),
                                Some(
                                    "CharacterAdded"
                                        | "CharacterRemoving"
                                        | "PlayerAdded"
                                        | "PlayerRemoving"
                                )
                            )
                    })
                    .count();
                remotes::process_delta_events(lua, ctx, &normalized.worker_delta, previous)
                    .map_err(|e| e.to_string())?;
                applied += 1;
            }
            let (lua, ctx, sched, _) = runtime.as_ref().unwrap();
            if !observed && ctx.borrow().services.contains_key("Players") {
                lua.load(OBSERVER).exec().map_err(|e| e.to_string())?;
                observed = true;
            }
            let report = scheduler::step(lua, sched, Instant::now(), Duration::from_millis(20));
            task_errors += report.errors.len();
            if let Some(error) = report.errors.first() {
                return Err(format!("observer failed after receipt {applied}: {error}"));
            }
            core.acknowledge(reader, sequence)
                .map_err(|e| e.to_string())?;
            acknowledged = sequence;
            if applied > 0 && applied % 2000 == 0 {
                eprintln!("replayed {applied} world receipts");
            }
            Ok(())
        },
    )?;
    let (lua, _, sched, _) = runtime.as_ref().ok_or("capture had no world")?;
    if !observed {
        return Err("capture had no Players service".into());
    }
    let counts: Vec<u64>=lua.load("return {counts.initialPlayers,counts.initialCharacters,counts.joined,counts.left,counts.spawned,counts.removed,counts.roots}").eval().map_err(|e|e.to_string())?;
    let remaining = scheduler::step(lua, sched, Instant::now(), Duration::from_millis(100));
    task_errors += remaining.errors.len();
    let report = json!({"source":"offline_capture_replay","live_connection_tested":false,
        "selected_rows":rows,"world_receipts_applied_to_luau":applied,"replay_elapsed_seconds":started.elapsed().as_secs_f64(),
        "initial_players":counts[0],"initial_characters":counts[1],"player_added_callbacks":counts[2],
        "player_removing_callbacks":counts[3],"character_added_callbacks":counts[4],"character_removing_callbacks":counts[5],
        "characters_with_root_at_callback":counts[6],"explicit_lifecycle_event_records":explicit_events,
        "observer_task_errors":task_errors,"pending_observer_waits":remaining.waiting,
        "packets_sent":0,"captured_scripts_executed":0});
    std::fs::write(&args[2], serde_json::to_vec_pretty(&report).unwrap())
        .map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    if task_errors != 0 {
        return Err("observer task failed".into());
    }
    Ok(())
}
