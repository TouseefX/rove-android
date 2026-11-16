//! Offline, receive-only validation against an existing private capture.
use headless_luau::{capture_replay, login};
use rove_client_core::session::{IngestOutcome, SessionCore};
use serde_json::{json, Value};
use std::{collections::HashMap, fs::File, io::BufReader};

fn point(value: &Value) -> Option<[f64; 3]> {
    let v = value.get("position")?.as_array()?;
    if v.len() != 3 {
        return None;
    }
    Some([v[0].as_f64()?, v[1].as_f64()?, v[2].as_f64()?])
}
fn main() -> Result<(), String> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 3 {
        return Err("usage: replay_movement_capture CAPTURE REPORT".into());
    }
    let mut core = SessionCore::with_limits(8, 32 * 1024 * 1024).map_err(|e| e.to_string())?;
    let reader = core.retain(0).map_err(|e| e.to_string())?;
    let mut acknowledged = 0;
    let mut pre_world_ping_packets = 0;
    let mut samples: HashMap<String, (u64, u64, [f64; 3], [f64; 3])> = HashMap::new();
    let rows = capture_replay::read_instance_state(
        BufReader::new(File::open(&args[1]).map_err(|e| e.to_string())?),
        |payload, _| {
            let outcome = match payload[0] {
                147 => {
                    let config =
                        login::feature_configuration(payload).map_err(|e| e.to_string())?;
                    core.set_instance_state_legacy_pv(
                        config
                            .iter()
                            .find(|(key, _)| key == "DebugAdaptivePVCompressor")
                            .is_some_and(|(_, v)| v == "false"),
                    );
                    None
                }
                161 => core
                    .ingest_instance_state(payload)
                    .map_err(|e| e.to_string())?,
                131 if core.scope().is_none() => {
                    headless_luau::ping_responder::decode_ping_packet(payload)
                        .map_err(|e| format!("pre-world ping validation failed: {e:?}"))?;
                    pre_world_ping_packets += 1;
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
            if entry.value.packet_kind == 161 {
                for action in entry.value.world_delta["actions"]
                    .as_array()
                    .ok_or("missing actions")?
                {
                    if let (Some(id), Some(p)) = (
                        action["id"].as_str(),
                        point(&action["fields"]["ReplicationPV"]),
                    ) {
                        let row = samples.entry(id.into()).or_insert((0, 0, p, p));
                        row.0 += 1;
                        if row.3 != p {
                            row.1 += 1;
                        }
                        row.3 = p;
                    }
                }
            }
            core.acknowledge(reader, sequence)
                .map_err(|e| e.to_string())?;
            acknowledged = sequence;
            Ok(())
        },
    )?;
    let world = core.world_view().ok_or("capture has no world")?;
    let mut players = Vec::new();
    let mut characters = std::collections::HashSet::new();
    let mut roots = std::collections::HashSet::new();
    world.for_each_node(|node| {
        if node.class_name() != "Player" {
            return;
        }
        let Some(character) = node
            .property("Character")
            .and_then(|v| v["$ref"].as_str().map(str::to_owned))
        else {
            return;
        };
        let Some(root) = world
            .get_property(&character, "PrimaryPart")
            .and_then(|v| v["$ref"].as_str().map(str::to_owned))
        else {
            return;
        };
        if world
            .read_only_node(&root)
            .is_none_or(|n| n.record_field("destroyed") == Some(json!(true)))
        {
            return;
        }
        characters.insert(character);
        roots.insert(root.clone());
        let observed = samples.get(&root);
        players.push(
            json!({"samples":observed.map_or(0,|r|r.0),"position_changes":observed.map_or(0,|r|r.1),
            "first_position":observed.map(|r|r.2),"last_position":observed.map(|r|r.3)}),
        );
    });
    let report = json!({"source":"offline_capture_replay","selected_rows":rows,
        "instance_state_replication":core.instance_state_report(),"world_packets_committed":core.committed_packets(),
        "pre_world_ping_packets_validated_without_replies":pre_world_ping_packets,
        "instances_with_pose_samples":samples.len(),"instances_with_position_changes":samples.values().filter(|r|r.1>0).count(),
        "players_with_roots":players.len(),"distinct_characters":characters.len(),"distinct_roots":roots.len(),
        "player_roots_with_position_changes":players.iter().filter(|p|p["position_changes"].as_u64().unwrap_or(0)>0).count(),"players":players,
        "live_connection_tested":false});
    std::fs::write(&args[2], serde_json::to_vec_pretty(&report).unwrap())
        .map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    Ok(())
}
