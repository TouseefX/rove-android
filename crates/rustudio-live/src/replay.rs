//! A recorded live session, replayed offline through rove's own decoder.
//!
//! The live path cannot be exercised against Roblox in CI, and a test transport
//! would test the test. So the unit of evidence is a **recording**: the payloads
//! one connection received, in the order they arrived, base64-encoded. Replaying
//! them drives exactly the code a live connection drives — rove's
//! `SessionCore::ingest` for every packet, rove's journal, then the bridge's
//! apply — with no socket, no clock and no network.
//!
//! The format is deliberately boring, because it is also the file a person
//! writes when they have a capture to reproduce a bug with:
//!
//! ```json
//! {
//!   "note": "what this session was (a place, a server, a date)",
//!   "packets": [
//!     {"at_ms": 0,    "kind": "schema",      "base64": "P4AAAA..."},
//!     {"at_ms": 0,    "kind": "globals",     "base64": "gQ..."},
//!     {"at_ms": 120,  "kind": "replication", "base64": "gw..."},
//!   ]
//! }
//! ```
//!
//! `kind` is documentation, not dispatch: rove's decoder reads the kind off the
//! payload's first byte (`Session::ingest`), and a `kind` a payload disagrees
//! with is refused by rove rather than trusted here. `at_ms` is the receipt time
//! the recorder saw; replay uses it only to split the recording into the batches
//! a live session would have pumped, so a run's action counts match the live
//! case instead of one giant batch.

use base64::Engine;
use rustudio_core::doc::Document;
use serde_json::Value as J;

use crate::live::{ApplyStats, Live};
use crate::session::{Ingest, Session};
use crate::snapshot::ValueConverter;

/// One recorded payload.
#[derive(Debug, Clone)]
pub struct Packet {
    /// Receipt time on the recording machine, in milliseconds.
    pub at_ms: u64,
    /// The recorder's label (`schema`, `globals`, `replication`, …).
    pub kind: String,
    pub payload: Vec<u8>,
}

/// A parsed recording.
#[derive(Debug, Clone)]
pub struct Recording {
    pub note: String,
    pub packets: Vec<Packet>,
}

impl Recording {
    /// The number of distinct arrival times: the batches a live session would
    /// have pumped.
    pub fn batches(&self) -> usize {
        let mut times: Vec<u64> = self.packets.iter().map(|p| p.at_ms).collect();
        times.dedup();
        times.len()
    }

    pub fn bytes(&self) -> usize {
        self.packets.iter().map(|p| p.payload.len()).sum()
    }
}

/// What a replay did, for the harness to print.
#[derive(Debug, Clone, Default)]
pub struct Replay {
    pub stats: ApplyStats,
    /// Packets rove accepted, by what it read them as.
    pub schema_packets: usize,
    pub globals_packets: usize,
    pub replication_packets: usize,
    /// Packets rove refused (a kind it does not read, or a malformed one).
    pub refused_packets: usize,
    /// The first refusal's text, for the report (the rest are counted).
    pub first_refusal: Option<String>,
    /// Journal records that committed but carried no instance work.
    pub empty_batches: usize,
    /// Instances in the world after the last batch.
    pub instances: usize,
    /// rove ids the world knows after the last batch.
    pub ids: usize,
    /// Creates still waiting for a parent that never arrived.
    pub waiting_for_parent: usize,
    /// The world sequence the document reflects.
    pub applied_sequence: u64,
}

impl Replay {
    /// The one line the harness prints and `--result` carries.
    pub fn line(&self) -> String {
        format!(
            "{} packets ({} schema, {} globals, {} replication, {} refused) -> \
             {} instances, {} ids, {} waiting; {}",
            self.schema_packets + self.globals_packets + self.replication_packets,
            self.schema_packets,
            self.globals_packets,
            self.replication_packets,
            self.refused_packets,
            self.instances,
            self.ids,
            self.waiting_for_parent,
            self.stats.line(),
        )
    }

    pub fn json(&self) -> J {
        let mut stats = self.stats.json();
        if let Some(object) = stats.as_object_mut() {
            object.insert("schema_packets".into(), self.schema_packets.into());
            object.insert("globals_packets".into(), self.globals_packets.into());
            object.insert("replication_packets".into(), self.replication_packets.into());
            object.insert("refused_packets".into(), self.refused_packets.into());
            object.insert("empty_batches".into(), self.empty_batches.into());
            object.insert("instances".into(), self.instances.into());
            object.insert("ids".into(), self.ids.into());
            object.insert("waiting_for_parent".into(), self.waiting_for_parent.into());
            object.insert("applied_sequence".into(), self.applied_sequence.into());
        }
        stats
    }
}

/// Read a recording. A file that is not one is refused by name rather than
/// half-read: a partial recording would replay into a half world.
pub fn parse(text: &str) -> Result<Recording, String> {
    let root: J = serde_json::from_str(text).map_err(|e| format!("recording is not JSON: {e}"))?;
    let object = root.as_object().ok_or("recording must be an object")?;
    let note = object
        .get("note")
        .and_then(J::as_str)
        .unwrap_or("(no note)")
        .to_string();
    let packets = object
        .get("packets")
        .and_then(J::as_array)
        .ok_or("recording has no packets array")?;
    let engine = base64::engine::general_purpose::STANDARD;
    let mut out = Vec::with_capacity(packets.len());
    for (i, packet) in packets.iter().enumerate() {
        let packet = packet
            .as_object()
            .ok_or_else(|| format!("packet {i} is not an object"))?;
        let encoded = packet
            .get("base64")
            .and_then(J::as_str)
            .ok_or_else(|| format!("packet {i} has no base64 payload"))?;
        let payload = engine
            .decode(encoded)
            .map_err(|e| format!("packet {i}: not base64 ({e})"))?;
        out.push(Packet {
            at_ms: packet.get("at_ms").and_then(J::as_u64).unwrap_or(0),
            kind: packet
                .get("kind")
                .and_then(J::as_str)
                .unwrap_or("?")
                .to_string(),
            payload,
        });
    }
    if out.is_empty() {
        return Err("recording has no packets".into());
    }
    Ok(Recording { note, packets: out })
}

/// Replay a recording into a document, one pump per arrival time.
///
/// `doc` may be an empty document (a session from nothing) or one a snapshot
/// build produced (a join that began with a snapshot); `live` must belong to
/// the same document.
pub fn replay(
    recording: &Recording,
    doc: &mut Document,
    live: &mut Live,
    conv: &mut ValueConverter,
) -> Replay {
    let mut session = match Session::new() {
        Ok(session) => session,
        Err(e) => {
            let mut failed = Replay::default();
            failed.refused_packets = recording.packets.len();
            failed.first_refusal = Some(e.to_string());
            return failed;
        }
    };
    let mut out = Replay::default();
    let mut index = 0;
    while index < recording.packets.len() {
        // One batch: every packet with this arrival time.
        let at = recording.packets[index].at_ms;
        while index < recording.packets.len() && recording.packets[index].at_ms == at {
            let packet = &recording.packets[index];
            match session.ingest(&packet.payload) {
                Ok(Ingest::Schema { .. }) => out.schema_packets += 1,
                Ok(Ingest::Globals { .. }) => out.globals_packets += 1,
                Ok(Ingest::Replication { .. }) => out.replication_packets += 1,
                Err(e) => {
                    out.refused_packets += 1;
                    if out.first_refusal.is_none() {
                        out.first_refusal = Some(format!("{}: {e}", packet.kind));
                    }
                }
            }
            index += 1;
        }
        // Then the pump that batch would have caused.
        match session.pump(doc, live, conv, crate::session::ACTIONS_PER_PUMP) {
            Ok(stats) => out.stats.add(&stats),
            Err(e) => {
                if out.first_refusal.is_none() {
                    out.first_refusal = Some(e.to_string());
                }
            }
        }
    }
    out.empty_batches = session.empty_batches();
    out.applied_sequence = session.applied_sequence();
    out.instances = doc.dom.descendants().count();
    out.ids = live.len();
    out.waiting_for_parent = live.waiting();
    out
}

/// A recording read from a file and replayed: the harness's one call.
pub fn load_and_replay(
    path: &std::path::Path,
    doc: &mut Document,
    live: &mut Live,
    conv: &mut ValueConverter,
) -> Result<(Recording, Replay), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let recording = parse(&text)?;
    let replay = replay(&recording, doc, live, conv);
    Ok((recording, replay))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recording_that_is_not_json_is_refused_by_name() {
        let err = parse("{").unwrap_err();
        assert!(err.contains("not JSON"), "{err}");
    }

    #[test]
    fn a_recording_without_packets_is_refused() {
        let err = parse(r#"{"note":"x","packets":[]}"#).unwrap_err();
        assert!(err.contains("no packets"), "{err}");
    }

    #[test]
    fn a_packet_that_is_not_base64_is_refused_with_its_index() {
        let err = parse(r#"{"packets":[{"base64":"!!!"}]}"#).unwrap_err();
        assert!(err.starts_with("packet 0"), "{err}");
    }

    #[test]
    fn arrival_times_are_the_batches() {
        let text = r#"{"packets":[
            {"at_ms":0,"kind":"schema","base64":"AA=="},
            {"at_ms":0,"kind":"globals","base64":"AA=="},
            {"at_ms":120,"kind":"replication","base64":"AA=="}]}"#;
        let recording = parse(text).expect("parses");
        assert_eq!(recording.packets.len(), 3);
        assert_eq!(recording.batches(), 2);
        assert_eq!(recording.bytes(), 3);
    }

    #[test]
    fn an_empty_payload_is_refused_by_rove_not_by_us() {
        // A recording can carry a payload rove will not read; the replay must
        // count it and keep going rather than abort the session.
        let doc_snapshot = r#"{"version":1,"sequence":0,"job_id":"r","services":{},"class_parents":{},"defaults":{},"default_origins":{},"nodes":[]}"#;
        let built = crate::world::build(
            crate::snapshot::parse(doc_snapshot).expect("parses"),
            "Replay".into(),
        )
        .expect("builds");
        let mut doc = built.world.doc;
        let mut live = Live::from_world(&built.world);
        let mut conv = ValueConverter::default();
        let recording = parse(r#"{"packets":[{"at_ms":0,"kind":"bogus","base64":"AA=="}]}"#)
            .expect("parses");
        let out = replay(&recording, &mut doc, &mut live, &mut conv);
        assert_eq!(out.refused_packets, 1, "rove refused the packet");
        assert!(out.first_refusal.is_some());
        assert_eq!(out.stats.actions, 0);
    }
}
