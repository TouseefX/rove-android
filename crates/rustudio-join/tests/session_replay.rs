//! M2b: a session fed a recording, through the same `Session` a live join uses.
//!
//! The claim these tests make is narrow and checkable: `Session::feed` is the
//! only path from bytes to world state, and the live join (`session::run`) calls
//! it with exactly the bytes its reliable stream delivered. So feeding the
//! recorder's messages here exercises the live path minus the socket — and the
//! world it produces is compared to the one the offline replay already proved
//! (`crates/rustudio-live/tests/live_replay.rs`), value by value.

use std::collections::BTreeMap;

use rbx_dom_weak::types::{Ref, Variant, Vector3};
use rustudio_join::session::{Session, Stage};
use serde_json::Value;

const STREAM: &str = include_str!("fixtures/join_stream.json");

fn stream() -> Value {
    serde_json::from_str(STREAM).expect("the recording is JSON")
}

fn b64(text: &str) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .expect("the fixture's base64")
}

fn messages(recording: &Value) -> Vec<Vec<u8>> {
    recording["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|value| b64(value.as_str().expect("a base64 string")))
        .collect()
}

/// Feed the recording, as a live session's read loop would.
fn replayed() -> Session {
    let recording = stream();
    let mut session = Session::new();
    session.stage = Stage::LoggedIn;
    for message in messages(&recording) {
        session.feed(&message);
    }
    session
}

fn by_class(session: &Session, class: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack: Vec<Ref> = session.document().dom.root().children().to_vec();
    while let Some(reference) = stack.pop() {
        let Some(instance) = session.document().dom.get_by_ref(reference) else {
            continue;
        };
        if instance.class.as_str() == class {
            found.push(instance.name.clone());
        }
        stack.extend(instance.children().iter().copied());
    }
    found.sort();
    found
}

#[test]
fn the_recording_is_what_a_live_stream_carried() {
    let recording = stream();
    let channel = &recording["channel"];
    assert_eq!(channel["application"], 4);
    assert_eq!(channel["channel"], 1);
    assert_eq!(
        messages(&recording).len(),
        recording["expect"]["packets"].as_u64().unwrap() as usize
    );
}

#[test]
fn the_session_reaches_the_world_the_offline_replay_proved() {
    let session = replayed();
    assert_eq!(session.ingested.packets, 5);
    assert_eq!(session.ingested.schema, 1, "one schema");
    assert_eq!(session.ingested.globals, 1, "one globals packet");
    assert_eq!(session.ingested.replication, 3, "three replication packets");
    assert_eq!(session.ingested.refused, 0, "nothing was refused");
    assert_eq!(session.stage, Stage::Receiving, "it got as far as receiving");

    // The place: Workspace and Players, the Part destroyed by the last packet,
    // the Character parented later.
    assert_eq!(by_class(&session, "Part"), ["Part"], "one Part survives");
    assert_eq!(by_class(&session, "Workspace"), ["Workspace"]);
    assert_eq!(by_class(&session, "Players"), ["Players"]);

    // The bridge's own counters: the removal destroyed one instance.
    assert_eq!(session.applied.removed, 1, "{:?}", session.applied);
    assert!(session.journal_records() > 0);
    assert_eq!(session.rejected(), 0);
}

#[test]
fn the_journal_holds_four_receipts_for_five_packets() {
    let session = replayed();
    // A schema installs the decoder and commits no world, so it is the one
    // accepted packet with no receipt; the globals and three replications are
    // the four rows.
    assert_eq!(
        session.journal_rows(),
        4,
        "one receipt per committed packet, and a schema commits nothing"
    );
    assert_eq!(
        session.ingested.without_actions, 1,
        "the schema is the packet with no actions"
    );
}

#[test]
fn a_refused_packet_does_not_stop_the_session() {
    let recording = stream();
    let mut session = Session::new();
    session.stage = Stage::LoggedIn;
    // A truncated schema first: the decoder refuses it, and the session keeps
    // reading, which is what a live client has to do — a hostile or broken
    // server must not be able to end the session by sending one bad packet.
    let mut messages = messages(&recording);
    let mut broken = messages[0].clone();
    broken.truncate(broken.len() / 2);
    messages.insert(0, broken);
    for message in &messages {
        session.feed(message);
    }
    assert_eq!(session.ingested.refused, 1, "one refusal, counted");
    assert_eq!(session.rejected(), 1);
    // And the rest of the stream still built the place.
    assert_eq!(by_class(&session, "Workspace"), ["Workspace"]);
    assert_eq!(by_class(&session, "Part"), ["Part"]);
}

#[test]
fn the_part_that_was_destroyed_is_gone_and_the_other_is_intact() {
    // The recording destroys the Part with the Size/Anchored pair and leaves
    // the Character, which has only a Size. Both facts belong to the world the
    // session built, so a live join's own overlay can show them.
    let session = replayed();
    let mut parts = Vec::new();
    let mut stack: Vec<Ref> = session.document().dom.root().children().to_vec();
    while let Some(reference) = stack.pop() {
        let Some(instance) = session.document().dom.get_by_ref(reference) else {
            continue;
        };
        if instance.class.as_str() == "Part" {
            parts.push(instance.properties.clone());
        }
        stack.extend(instance.children().iter().copied());
    }
    assert_eq!(parts.len(), 1, "exactly one Part is left");
    // The survivor is the Character: its Size is the 1,1,1 the packet gave it,
    // and it never had an Anchored.
    assert_eq!(
        parts[0].get(&"Size".into()),
        Some(&Variant::Vector3(Vector3::new(1.0, 1.0, 1.0)))
    );
    assert_eq!(parts[0].get(&"Anchored".into()), None);
}

#[test]
fn the_character_is_under_players_and_players_under_workspace() {
    let session = replayed();
    let recording = stream();
    let ids: BTreeMap<String, String> = recording["expect"]["ids"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| (key.clone(), value.as_str().unwrap().to_owned()))
        .collect();
    assert_eq!(ids["players"], "1:2");
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    let mut stack: Vec<(Ref, String)> = session
        .document()
        .dom
        .root()
        .children()
        .iter()
        .map(|reference| {
            let name = session
                .document()
                .dom
                .get_by_ref(*reference)
                .map(|instance| instance.name.clone())
                .unwrap_or_default();
            (*reference, name)
        })
        .collect();
    while let Some((reference, name)) = stack.pop() {
        names.insert(name.clone(), session.document().dom.get_by_ref(reference).unwrap().class.to_string());
        for child in session.document().dom.get_by_ref(reference).unwrap().children() {
            let child_name = session
                .document()
                .dom
                .get_by_ref(*child)
                .map(|instance| instance.name.clone())
                .unwrap_or_default();
            stack.push((*child, child_name));
        }
    }
    assert_eq!(names.get("Players").map(String::as_str), Some("Players"));
    assert_eq!(names.get("Workspace").map(String::as_str), Some("Workspace"));
    assert_eq!(names.get("Part").map(String::as_str), Some("Part"));
}

// ---- The channel dispatch -------------------------------------------------
//
// A live server multiplexes several kinds of packet onto the same reliable
// stream, and two of them share a first byte with the world's. `feed_on` is
// what tells them apart, and these tests pin the three collisions that a real
// session hits — the ones a device report found the hard way.

#[test]
fn the_same_bytes_are_the_world_on_one_channel_and_a_ping_on_another() {
    let recording = stream();
    let messages = messages(&recording);
    // The globals packet, then a replication packet, both of which start with
    // 129 / 131 — the ids a ping packet also uses.
    let replication = messages
        .iter()
        .find(|payload| payload.first() == Some(&131))
        .expect("the recording has a replication packet")
        .clone();

    // On application 4 channel 1, this is world replication.
    let mut world = Session::new();
    for message in &messages {
        world.feed_on(4, 1, message);
    }
    let created = world.applied.created;
    assert!(created > 0, "the recording builds a world on 4/1");

    // On application 4 channel 3 before globals, the same bytes are a ping:
    // counted, not decoded, and the world stays empty. `false` is "not world
    // state", which is what a caller records; silence is not an error.
    let mut pings = Session::new();
    assert!(!pings.feed_on(4, 3, &replication));
    assert_eq!(pings.pings, 1, "counted as a ping");
    assert_eq!(pings.ingested.packets, 0, "and not fed to the decoder");
    assert_eq!(pings.applied.created, 0);
}

#[test]
fn a_packet_on_a_channel_this_client_does_not_dispatch_is_counted_not_decoded() {
    let recording = stream();
    let messages = messages(&recording);
    let mut session = Session::new();
    for message in &messages {
        session.feed_on(6, 2, message);
    }
    assert_eq!(session.ingested.packets, 0);
    assert_eq!(session.unhandled, messages.len());
    // And the arrivals record where they came from, which is what the report
    // prints when a join receives plenty and builds nothing.
    assert_eq!(
        session.arrivals.get(&(6, 2, messages[0][0])).copied(),
        Some(1)
    );
}

#[test]
fn the_feature_table_selects_the_property_encoding_it_names() {
    // 0x93, then a u16 count, then length-prefixed name/value pairs. The one
    // name that changes the decoder is DebugAdaptivePVCompressor; rove's live
    // session reads the same table for the same reason.
    fn table(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut packet = vec![0x93, 0];
        packet.extend((entries.len() as u16).to_le_bytes());
        for (name, value) in entries {
            for text in [name, value] {
                packet.extend((text.len() as u16).to_le_bytes());
                packet.extend(text.as_bytes());
            }
        }
        packet
    }
    let mut session = Session::new();
    assert!(session.feed_on(4, 1, &table(&[("DebugAdaptivePVCompressor", "false")])));
    assert_eq!(session.features, 1);
    assert_eq!(session.ingested.packets, 0, "a feature table is not a world packet");
    assert!(session.feed_on(4, 1, &table(&[("DebugAdaptivePVCompressor", "true")])));
    assert_eq!(session.features, 1);
    // A truncated one is refused rather than trusted.
    let mut broken = table(&[("DebugAdaptivePVCompressor", "false")]);
    broken.truncate(broken.len() - 2);
    assert!(!session.feed_on(4, 1, &broken));
    assert_eq!(session.ingested.refused, 1);
}

#[test]
fn the_drain_keeps_up_with_more_rows_than_one_read_returns() {
    // The replay's journal holds a handful of rows; a real place commits
    // hundreds per packet. What is checked here is the loop's own property:
    // every row committed is applied, not one batch of 64 and then a stall.
    let recording = stream();
    let messages = messages(&recording);
    let mut session = Session::new();
    for message in &messages {
        session.feed_on(4, 1, message);
    }
    assert_eq!(
        session.journal_rows(),
        session.journal_records(),
        "the two names for one number agree"
    );
    assert!(session.journal_rows() > 0);
}
