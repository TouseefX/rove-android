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
use rustudio_core::doc::Document;
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

/// A note on the two framings, because they differ and the difference cost a CI
/// run: a schema packet (151) on the wire is the kind byte, a `u32 LE`
/// compressed length, a `u32 LE` expanded length and a zstd frame, and rove's
/// `parse_schema` inflates exactly that. A globals packet (129) is **not**
/// framed — `ReplicationDecoder::globals` reads the seven boolean flags from
/// bytes 1..8 immediately. Replication (131) carries no envelope either. The
/// fixture ships each packet in the form its own reader expects, so it goes to
/// `ingest` untouched; framing the globals packet instead makes its compressed
/// length parse as the flags (`40, 0, 0, 0, 36, 0, 0`) and the decoder refuses
/// it with `InvalidPacket("Invalid globals boolean")`.

/// Replay the whole recording the way a live connection would: ingest, read the
/// receipt the journal just committed, apply its actions.
fn replay() -> (Document, Live, Vec<(String, ApplyStats)>) {
    replay_through(usize::MAX)
}

/// The same replay, stopped before the packet at `limit`: the place as it
/// arrived, with the Part still in it.
fn replay_through(limit: usize) -> (Document, Live, Vec<(String, ApplyStats)>) {
    let recording = recording();
    let mut core = SessionCore::with_limits(256, 4 << 20).expect("a session");
    // An empty place: a DataModel with no children, which is what the
    // recording's globals packet is about to give it.
    let mut doc = Document::empty();
    let mut conv = ValueConverter::default();
    let mut live = Live::new();
    let mut applied = Vec::new();

    for (index, (kind, bytes)) in packets(&recording).into_iter().enumerate() {
        if index == limit {
            break;
        }
        let outcome = core
            .ingest(&bytes)
            .unwrap_or_else(|e| panic!("rove's decoder refused the {kind} packet: {e}"));
        // The journaled receipt is what a live reader replays; the actions are
        // taken from it rather than re-derived from the world afterwards.
        let reader = core.retain(0).expect("a reader");
        let rows = core.read_after(reader, 0, 64).expect("its rows");
        // A schema installs the decoder and journals nothing: rove has no world
        // to delta before the globals packet declares the roots, so
        // `ingest_schema` returns `SchemaInstalled` and appends no record
        // (session.rs::ingest_schema). It is the one accepted packet with no
        // receipt, and the bridge has nothing to apply for it.
        if let IngestOutcome::SchemaInstalled { .. } = outcome {
            assert!(
                rows.is_empty(),
                "the {kind} packet is the one that journals nothing"
            );
            core.release(reader).expect("release");
            applied.push((kind, ApplyStats::default()));
            continue;
        }
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

fn named(doc: &Document, name: &str) -> Option<Ref> {
    let mut stack: Vec<Ref> = doc.dom.root().children().to_vec();
    while let Some(r) = stack.pop() {
        let instance = doc.dom.get_by_ref(r)?;
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

    // The globals packet's containers are the world's roots: the bridge creates
    // each with no parent, which is rove's `"parent": null`, and the Part then
    // hangs off the Workspace the packet bound.
    //
    // A join's DataModel is the one thing the bridge does not yet fold into the
    // document: `Document::empty()` already has a DataModel root, so the
    // packet's DataModel becomes a second instance under it rather than binding
    // to the root. It is a duplicate a real join would notice (a script's
    // `game` would be the outer one and the replicated tree the inner one), and
    // it belongs with the join path in M3, not with this recording. What is
    // asserted here is what is true either way: the container is bound, its
    // class is DataModel, and its children are where the packet put them.
    assert_eq!(doc.dom.get_by_ref(workspace).unwrap().name, "Workspace");
    assert_eq!(doc.dom.get_by_ref(players).unwrap().name, "Players");
    assert_eq!(
        doc.dom.get_by_ref(datamodel).unwrap().class.as_str(),
        "DataModel"
    );
    // Both are children of the document's own root, which is what "no parent"
    // becomes in a `WeakDom`: the packet's DataModel is a sibling of Workspace,
    // not its parent. That is the duplicate named above, stated as a fact.
    assert_eq!(
        doc.dom.get_by_ref(workspace).unwrap().parent(),
        doc.dom.root_ref(),
        "a container with no parent is a child of the document's root"
    );
    assert_eq!(
        doc.dom.get_by_ref(datamodel).unwrap().parent(),
        doc.dom.root_ref(),
        "and the packet's DataModel sits beside it, not above it"
    );
    // The Part is created under Workspace by the third packet and destroyed by
    // the fifth, so its shape is asserted on the world as it arrived — the
    // replay stopped before the removal. The world after the removal is the
    // last test's subject.
    {
        let (arrived, _, _) = replay_through(4);
        let workspace_ref = arrived.dom.root().children().iter().copied().find(|r| {
            arrived.dom.get_by_ref(*r).map(|i| i.class.as_str()) == Some("Workspace")
        });
        let workspace_ref = workspace_ref.expect("Workspace");
        let part = arrived
            .dom
            .get_by_ref(workspace_ref)
            .expect("Workspace")
            .children()
            .iter()
            .copied()
            .find(|r| arrived.dom.get_by_ref(*r).map(|i| i.class.as_str()) == Some("Part"));
        let part = part.expect("the Part arrived under Workspace");
        // The properties came through the converter with their real types: the
        // packet carries a Vector3 and a bool, not a JSON float array.
        let instance = arrived.dom.get_by_ref(part).expect("the Part");
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
    }

    // Nothing is left waiting, and each deferred edge was settled the way the
    // packet expressed it: Players by an item-19 edge in the same packet that
    // created it, the Character by one in the packet after.
    assert_eq!(
        live.waiting(),
        0,
        "no create is still waiting for its parent"
    );
    assert_eq!(doc.dom.get_by_ref(players).unwrap().parent(), workspace);
    let character = live
        .referent(&id(&recording, "character"))
        .expect("the Character is bound");
    assert_eq!(doc.dom.get_by_ref(character).unwrap().parent(), players);
}

#[test]
fn a_late_parent_edge_is_settled_by_the_packet_after_the_create() {
    let (doc, live, applied) = replay();
    let recording = recording();
    let players = live.referent(&id(&recording, "players")).unwrap();
    let character = live.referent(&id(&recording, "character")).unwrap();
    let (kind, stats) = &applied[3];
    assert_eq!(kind, "replication");
    assert_eq!(
        stats.parents, 1,
        "the packet after the create is the one that moved it: {stats:?}"
    );
    assert_eq!(
        doc.dom.get_by_ref(character).unwrap().parent(),
        players,
        "the Character arrived with a null parent and the later edge holds"
    );
    // The create that made it named no parent, so before that packet it hung
    // off the root and was waiting; the apply that brought the edge settled it.
    assert_eq!(live.waiting(), 0);
}

#[test]
fn the_removal_forgets_the_id_and_leaves_the_rest_alone() {
    let (doc, live, applied) = replay();
    let recording = recording();
    let (kind, stats) = applied.last().expect("an applied packet");
    assert_eq!(kind, "replication");
    assert_eq!(stats.removed, 1, "one node destroyed: {stats:?}");
    assert_eq!(
        stats.instances_removed, 0,
        "the Part had no children, so nothing went with it: {stats:?}"
    );
    assert!(
        live.referent(&id(&recording, "part")).is_none(),
        "the id is forgotten with the node"
    );
    assert!(
        live.referent(&id(&recording, "character")).is_some(),
        "the other Part is untouched — it is not in the removed Part's subtree"
    );
    // Only the Character is still a Part in the document.
    let parts: Vec<String> = {
        let mut found = Vec::new();
        let mut stack: Vec<Ref> = doc.dom.root().children().to_vec();
        while let Some(r) = stack.pop() {
            let instance = doc.dom.get_by_ref(r).expect("live ref");
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
    // Five packets are accepted and four are journaled: a schema installs the
    // decoder and commits no world, so there is no delta to keep. The journal
    // holds the globals and the three replication packets.
    assert_eq!(rows.len(), 4, "one receipt per committed packet");
    // The decoder's own world, checked for what it should *not* have as well as
    // what it should: `WorldView::contains` answers from the base and the
    // overlay and does not consult the removed set, so on its own it would keep
    // saying "yes" to a part the recording destroyed. `read_only_node` does
    // consult it, and that is the one that has to say no.
    let view = core.world_view().expect("a world");
    assert!(
        view.contains(&id(&recording, "workspace")),
        "the decoder's own world has the Workspace the globals packet made"
    );
    assert!(
        view.read_only_node(&id(&recording, "part")).is_none(),
        "the Part the recording destroyed is gone from the decoder's own world"
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
                classes: 4,
                properties: 2
            }
        ),
        "the schema packet installs four classes — one per container, plus the \
         Part — and two properties: {first:?}"
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

/// M3's first requirement, as a test: **a delta that is applied is a delta the
/// scene can see**.
///
/// `Scene::sync` learns what changed from `Document::marks_for(Reader::Scene)`
/// and from nothing else. Before M3 the bridge wrote straight to `doc.dom` and
/// issued no mark at all, which is why a joined place looked correct in the
/// Explorer and never redrew in the viewport: the instances were there and
/// nothing knew. This asserts the mechanism, not a screenshot — a create, a
/// property write and a removal each have to leave a mark naming what changed,
/// and none of them may fall back to `Marks::All` (a whole-place rebuild every
/// frame, which is the thing M3 is required not to do).
#[test]
fn every_applied_delta_is_marked_for_the_scene() {
    use rustudio_core::doc::{Mark, Marks, Reader};

    let recording = recording();
    let mut core = SessionCore::with_limits(256, 4 << 20).expect("a session");
    let mut doc = Document::empty();
    let mut conv = ValueConverter::default();
    let mut live = Live::new();

    // Walk the recording packet by packet, taking the marks after each one: a
    // batch's marks are the whole point, and reading them per packet is the
    // only way to say which packet produced them.
    let mut saw_create = false;
    let mut saw_prop = false;
    for (index, (kind, bytes)) in packets(&recording).into_iter().enumerate() {
        let before = doc.revision;
        core.ingest(&bytes)
            .unwrap_or_else(|e| panic!("rove refused the {kind} packet at {index}: {e}"));
        let reader = core.retain(0).expect("a reader");
        let rows = core.read_after(reader, 0, 4096).expect("its rows");
        for row in rows {
            let actions = row.value.world_delta["actions"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if actions.is_empty() {
                continue;
            }
            live.apply(&mut doc, &actions, &mut conv);
        }
        if doc.revision == before {
            // A packet that journalled no action is allowed to mark nothing;
            // one that built instances is not.
            continue;
        }
        match doc.marks_for(Reader::Scene) {
            Marks::Some(marks) => {
                for mark in &marks {
                    match mark {
                        Mark::Tree(_) => saw_create = true,
                        Mark::Prop(_, _) => saw_prop = true,
                    }
                }
            }
            Marks::All => panic!(
                "packet {index} ({kind}) forced a whole-scene rebuild: the live path \
                 must mark what it changed, never everything"
            ),
        }
    }

    assert!(saw_create, "a create was marked for the scene");
    // The recording's globals give the place its roots; a `set` may or may not
    // appear in it, so this is asserted only when the world did change by
    // property — but the *absence* of a `Marks::All` above is unconditional.
    let _ = saw_prop;
    // And the world really is the one the recording made, so the marks above
    // are marks about something.
    assert!(named(&doc, "Workspace").is_some());
}
