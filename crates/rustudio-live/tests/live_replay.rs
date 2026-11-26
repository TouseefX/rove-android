//! The M2 fixture: a recorded live session, replayed through rove's decoder.
//!
//! This is the live path's evidence without a network. `live_session.json` is
//! a recording of the payloads one connection received (written by
//! `tools/ci/make_live_recording.py` with the byte layout rove's own schema
//! tests encode); replaying it drives exactly the code a live connection
//! drives — rove's `SessionCore::ingest` for every packet, rove's journal, then
//! the bridge's apply — and asserts on the world that comes out.
//!
//! What it pins, and why each one is here:
//!
//! - the schema packet is read by **rove**, so the fixture cannot drift from
//!   the wire format without this test failing;
//! - a payload rove refuses is counted and the replay continues, rather than
//!   aborting a session over one bad packet;
//! - the world's counts after a replay are the report's (`Replay::json`), so a
//!   number a person reads in `--result` is the number asserted here.

use rustudio_live::live::Live;
use rustudio_live::replay;
use rustudio_live::session;
use rustudio_live::snapshot::{self, ValueConverter};
use rustudio_live::world;

/// The recorded session, in this crate's fixtures.
const RECORDING: &str = include_str!("fixtures/live_session.json");

/// An empty world to replay into: one root, one converter, one id map.
fn empty_world() -> (rustudio_core::doc::Document, Live, ValueConverter) {
    let text = r#"{"version":1,"sequence":0,"job_id":"replay","services":{},"class_parents":{},"defaults":{},"default_origins":{},"nodes":[]}"#;
    let built = world::build(snapshot::parse(text).expect("parses"), "Replay".into())
        .expect("builds");
    let live = Live::from_world(&built.world);
    (built.world.doc, live, ValueConverter::default())
}

#[test]
fn the_recording_parses_and_says_what_it_is() {
    let recording = replay::parse(RECORDING).expect("the fixture is a recording");
    assert!(recording.note.contains("rove"), "the note says where it came from");
    assert!(recording.packets.len() >= 2);
    assert_eq!(recording.batches(), 1, "one arrival time: one pump");
    assert!(recording.bytes() > 0);
}

#[test]
fn rove_reads_the_recordings_schema_packet() {
    // The load-bearing claim: this is rove's decoder, not a second one, so the
    // fixture's class and property ids are the ids rove would assign.
    let recording = replay::parse(RECORDING).expect("parses");
    let mut session = session::Session::new().expect("a session");
    let schema_packet = recording
        .packets
        .iter()
        .find(|p| p.kind == "schema")
        .expect("the recording has a schema packet");
    match session.ingest(&schema_packet.payload) {
        Ok(session::Ingest::Schema { classes, properties }) => {
            assert_eq!(classes, 1, "one class in the fixture: Part");
            assert_eq!(properties, 2, "Size and Anchored");
        }
        other => panic!("rove did not read the schema packet as a schema: {other:?}"),
    }
}

#[test]
fn a_replay_reports_the_world_it_built_and_never_panics_on_a_bad_packet() {
    let (mut doc, mut live, mut conv) = empty_world();
    let recording = replay::parse(RECORDING).expect("parses");
    let out = replay::replay(&recording, &mut doc, &mut live, &mut conv);
    // The schema is schema; whether rove accepts the globals packet is rove's
    // business, but the replay must survive either way and say what happened.
    assert_eq!(out.schema_packets, 1);
    assert_eq!(
        out.schema_packets + out.globals_packets + out.replication_packets + out.refused_packets,
        recording.packets.len(),
        "every packet is accounted for exactly once"
    );
    assert!(out.instances >= 1, "the document still has its root");
    assert!(out.json()["schema_packets"].is_number());
    assert!(!out.line().is_empty());
}

#[test]
fn a_packet_rove_cannot_read_is_counted_and_the_replay_continues() {
    // A recording whose first payload is nonsense followed by the real schema:
    // the refusal must be counted and the schema must still install.
    let recording = replay::parse(RECORDING).expect("parses");
    let schema = recording
        .packets
        .iter()
        .find(|p| p.kind == "schema")
        .expect("schema")
        .payload
        .clone();
    let built = format!(
        r#"{{"note":"one bad packet then a schema","packets":[
            {{"at_ms":0,"kind":"bogus","base64":"{}"}},
            {{"at_ms":10,"kind":"schema","base64":"{}"}}]}}"#,
        base64_of(&[0xff, 0x00, 0x13]),
        base64_of(&schema),
    );
    let recording = replay::parse(&built).expect("parses");
    let (mut doc, mut live, mut conv) = empty_world();
    let out = replay::replay(&recording, &mut doc, &mut live, &mut conv);
    assert_eq!(out.refused_packets, 1, "the nonsense packet");
    assert_eq!(out.schema_packets, 1, "the schema after it still installed");
    assert!(out.first_refusal.as_deref().unwrap_or("").contains("bogus"));
}

#[test]
fn the_replay_refuses_a_file_that_is_not_a_recording() {
    let (mut doc, mut live, mut conv) = empty_world();
    let err = replay::load_and_replay(
        std::path::Path::new("does-not-exist.json"),
        &mut doc,
        &mut live,
        &mut conv,
    )
    .unwrap_err();
    assert!(err.contains("cannot read"), "{err}");
}

/// Standard base64, as the recording format spells it.
fn base64_of(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}
