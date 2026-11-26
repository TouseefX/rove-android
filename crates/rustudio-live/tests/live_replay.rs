//! The offline half of M2: a recorded session, decoded by rove's own decoder,
//! applied to a rustudio document by the live bridge.
//!
//! Nothing here dials. `tests/fixtures/live_session.json` holds the packets a
//! join received (tools/ci/make_live_recording.py writes it), and the path they
//! take is the path a live connection takes: SessionCore ingests the bytes and
//! journals them; every committed packet's actions go to `Live`.
//!
//! The point of doing it this way is that it cannot agree with itself. The
//! decoder is rove's — `rove-client-core`, the same code a live session runs —
//! the bytes are a file, and what is asserted is rustudio's world afterwards.

// `Ref` lives under `types` in the vendored rbx_dom_weak 4.2 (live.rs imports it
// the same way); the crate root has no `Ref`.
use rbx_dom_weak::types::{Ref, Variant, Vector3};
use rbx_dom_weak::WeakDom;
use rove_client_core::session::{IngestOutcome, SessionCore};
use rustudio_live::live::{ApplyStats, Live};
use rustudio_live::snapshot::ValueConverter;
use serde_json::Value;

const RECORDING: &str = include_str!("fixtures/live_session.json");

fn recording() -> Value {
    serde_json::from_str(RECORDING).expect("the fixture is JSON")
}

fn id(recording: &Value, name: &str) -> String {
    recording["ids"][name]
        .as_str()
        .unwrap_or_else(|| panic!("the fixture names {name}"))
        .to_owned()
}

/// base64 without a dependency: the fixture's alphabet and padding only.
fn b64(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\n' | b'\r' | b' ' => continue,
            other => panic!("not base64: {:?}", other as char),
        } as u32;
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn packets(recording: &Value) -> Vec<(String, Vec<u8>)> {
    recording["packets"]
        .as_array()
        .expect("packets")
        .iter()
        .map(|entry| {
            let kind = entry["kind"].as_str().expect("kind").to_owned();
            (kind, b64(entry["base64"].as_str().expect("base64")))
        })
        .collect()
}

/// Replay the whole recording the way a live connection would: ingest, read the
/// receipt the journal just committed, apply its actions.
fn replay() -> (WeakDom, Live, Vec<(String, ApplyStats)>) {
    let recording = recording();
    let mut core = SessionCore::with_limits(256, 4 << 20).expect("a session");
    let mut doc = WeakDom::new();
    let mut conv = ValueConverter::default();
    let mut live = Live::new();
    let mut applied = Vec::new();

    for (kind, bytes) in packets(&recording) {
        let outcome = core
            .ingest(&bytes)
            .unwrap_or_else(|e| panic!("rove's decoder refused the {kind} packet: {e}"));
        // The journaled receipt is what a live reader replays; the actions are
        // taken from it rather than re-derived from the world afterwards.
        let reader = core.retain(0).expect("a reader");
        let rows = core.read_after(reader, 0, 64).expect("its rows");
        let record = rows
            .last()
            .map(|row| row.value.clone())
            .unwrap_or_else(|| {
                panic!("the {kind} packet was accepted but journaled nothing ({outcome:?})")
            });
        core.release(reader).expect("release");
        let actions = record.world_delta["actions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let stats = live.apply(&mut doc, &actions, &mut conv);
        applied.push((kind, stats));
    }
    (doc, live, applied)
}

fn named(doc: &WeakDom, name: &str) -> Option<Ref> {
    let mut stack: Vec<Ref> = doc.root().children().to_vec();
    while let Some(r) = stack.pop() {
        let instance = doc.get_by_ref(r)?;
        if instance.name == name {
            return Some(r);
        }
        stack.extend(instance.children().iter().copied());
    }
    None
}

#[test]
fn rove_decodes_the_recording_and_the_bridge_applies_it() {
    let (doc, live, applied) = replay();
    let kinds: Vec<&str> = applied.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "schema",
            "globals",
            "replication",
            "replication",
            "replication"
        ],
        "the fixture's packet order"
    );

    let recording = recording();
    let workspace = live
        .referent(&id(&recording, "workspace"))
        .expect("Workspace is bound");
    let players = live
        .referent(&id(&recording, "players"))
        .expect("Players is bound");
    let datamodel = live
        .referent(&id(&recording, "datamodel"))
        .expect("DataModel is bound");

    // The globals packet's containers are the world's roots; the replication
    // packet's Part hangs off Workspace.
    assert_eq!(doc.get_by_ref(workspace).name, "Workspace");
    assert_eq!(doc.get_by_ref(players).name, "Players");
    assert_eq!(doc.get_by_ref(datamodel).name, "DataModel");
    let part = live
        .referent(&id(&recording, "part"))
        .expect("the Part is bound");
    assert_eq!(doc.get_by_ref(part).parent(), Some(workspace));

    // The properties came through the converter with their real types: the
    // packet carries a Vector3 and a bool, not a JSON float array.
    let instance = doc.get_by_ref(part);
    assert_eq!(
        instance.properties.get(&"Size".into()),
        Some(&Variant::Vector3(Vector3::new(4.0, 1.0, 2.0))),
        "Size is a Vector3 with the packet's values"
    );
    assert_eq!(
        instance.properties.get(&"Anchored".into()),
        Some(&Variant::Bool(true)),
        "Anchored is the packet's boolean"
    );

    // Nothing is left waiting: Players' parent edge arrived in the packet after
    // the one that created it.
    assert_eq!(
        live.waiting(),
        0,
        "no create is still waiting for its parent"
    );
    assert_eq!(doc.get_by_ref(players).parent(), Some(workspace));
}

#[test]
fn the_character_is_bound_to_players_even_though_it_arrived_first() {
    let (doc, live, _) = replay();
    let recording = recording();
    let players = live.referent(&id(&recording, "players")).unwrap();
    let character = live.referent(&id(&recording, "character")).unwrap();
    assert_eq!(
        doc.get_by_ref(character).parent(),
        Some(players),
        "the child arrived with no parent and was given its edge later"
    );
}

#[test]
fn the_removal_takes_the_subtree_and_forgets_the_id() {
    let (doc, live, applied) = replay();
    let recording = recording();
    let (kind, stats) = applied.last().expect("an applied packet");
    assert_eq!(kind, "replication");
    assert_eq!(stats.removed, 1, "one node destroyed: {stats:?}");
    assert!(
        live.referent(&id(&recording, "part")).is_none(),
        "the id is forgotten with the node"
    );
    assert!(
        live.referent(&id(&recording, "character")).is_some(),
        "the other Part is untouched"
    );
    // Only the Character is still a Part in the document.
    let parts: Vec<String> = {
        let mut found = Vec::new();
        let mut stack: Vec<Ref> = doc.root().children().to_vec();
        while let Some(r) = stack.pop() {
            let instance = doc.get_by_ref(r).expect("live ref");
            if instance.class.as_str() == "Part" {
                found.push(instance.name.clone());
            }
            stack.extend(instance.children().iter().copied());
        }
        found
    };
    assert_eq!(parts, ["Part"], "the removed Part is gone from the tree");
}

#[test]
fn the_journal_holds_what_a_live_reader_would_replay() {
    let recording = recording();
    let mut core = SessionCore::with_limits(256, 4 << 20).unwrap();
    for (_, bytes) in packets(&recording) {
        core.ingest(&bytes).expect("accepted");
    }
    let reader = core.retain(0).unwrap();
    let rows = core.read_after(reader, 0, 64).unwrap();
    assert_eq!(rows.len(), 5, "one receipt per committed packet");
    let view = core.world_view().expect("a world");
    assert!(
        view.contains(&id(&recording, "part")),
        "the decoder's own world has the Part the packets created"
    );
    assert!(
        core.journal_stats().records > 0,
        "the journal kept {:?}",
        core.journal_stats()
    );
    assert_eq!(core.rejected_packets(), 0, "nothing was refused");
}

#[test]
fn ingest_outcomes_name_what_each_packet_did() {
    let recording = recording();
    let mut core = SessionCore::with_limits(256, 4 << 20).unwrap();
    let mut packets = packets(&recording).into_iter();
    let first = core.ingest(&packets.next().unwrap().1).unwrap();
    assert!(
        matches!(
            first,
            IngestOutcome::SchemaInstalled {
                classes: 3,
                properties: 2
            }
        ),
        "the schema packet installs three classes and two properties: {first:?}"
    );
    let second = core.ingest(&packets.next().unwrap().1).unwrap();
    assert!(
        matches!(second, IngestOutcome::GlobalsCommitted { .. }),
        "the globals packet commits the top containers: {second:?}"
    );
    let third = core.ingest(&packets.next().unwrap().1).unwrap();
    assert!(
        matches!(third, IngestOutcome::ReplicationCommitted { .. }),
        "the first replication packet commits the place: {third:?}"
    );
}

#[test]
fn a_truncated_packet_is_refused_by_the_decoder_and_never_half_applied() {
    let recording = recording();
    let mut core = SessionCore::with_limits(256, 4 << 20).unwrap();
    let mut refused = 0;
    for (kind, bytes) in packets(&recording) {
        if kind != "replication" {
            core.ingest(&bytes).unwrap();
            continue;
        }
        let mut broken = bytes.clone();
        broken.truncate(broken.len() / 2);
        assert!(
            core.ingest(&broken).is_err(),
            "a truncated replication packet is refused"
        );
        refused += 1;
        break;
    }
    assert_eq!(refused, 1);
    assert_eq!(core.rejected_packets(), 1, "the refusal is counted, not hidden");
    // And the world is the one the intact packets made.
    let (doc, _, _) = replay();
    assert!(named(&doc, "Workspace").is_some());
}
