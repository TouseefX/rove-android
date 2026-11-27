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
