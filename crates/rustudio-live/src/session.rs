//! A live session's packets, owned by rove's decoder and applied by the bridge.
//!
//! One `SessionCore` from `rove-client-core` is the whole wire side: a caller
//! feeds it the payloads a connection received (the schema first, then globals,
//! then replication) and it answers with what was committed. That is the same
//! object rove's own client drives, so the bridge cannot drift from rove's wire
//! format — there is no second decoder here to drift with.
//!
//! The order in this module is the order that matters:
//!
//! 1. [`Session::ingest`] hands a payload to rove and returns what it was;
//! 2. [`Session::pump`] reads the committed journal after the last applied
//!    sequence and applies each world action through [`crate::live::Live`];
//! 3. the caller draws the document that resulted.
//!
//! Nothing here reads the clock, sleeps or owns a socket: the transport (rove's
//! `transport` feature, or anything else that yields payloads) is the caller's
//! business, which is what makes the live path testable offline — the tests in
//! this file drive it with rove's own packet fixtures and no network at all.

use std::collections::VecDeque;

use rove_client_core::journal::ReaderId;
use rove_client_core::session::SessionCore;
use rustudio_core::doc::Document;
use serde_json::Value as J;

use crate::live::{ApplyStats, Live};
use crate::snapshot::ValueConverter;

/// The journal reader's window: how much a frame may apply. Chosen to match the
/// offline build's budget (2 ms/frame in docs/LIVE-JOIN.md) rather than the
/// journal's own limit, because a batch that takes longer than a frame is a
/// hitch, and the journal can wait.
pub const ACTIONS_PER_PUMP: usize = 512;

/// What a payload was, once rove has read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ingest {
    /// The schema: class and property ids the replication packets refer to.
    Schema { classes: usize, properties: usize },
    /// The globals packet: the client scope and the instance the player owns.
    Globals { world_sequence: u64 },
    /// A replication packet: creates, sets, parents, removes.
    Replication { world_sequence: u64 },
}

/// Nothing arrived, or rove refused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// rove could not read this payload (`SessionError`'s own text).
    Refused(String),
    /// The journal could not give us the records (`JournalError`'s own text).
    Journal(String),
    /// A reader lease was not available.
    Closed,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Refused(why) => write!(f, "rove refused the packet: {why}"),
            SessionError::Journal(why) => write!(f, "the journal: {why}"),
            SessionError::Closed => f.write_str("the session is closed"),
        }
    }
}

impl std::error::Error for SessionError {}

/// A live session's world: rove's decoder on one side, a `Document` on the
/// other, and the sequence the journal reader has reached between them.
pub struct Session {
    core: SessionCore,
    reader: Option<ReaderId>,
    /// The world sequence the reader has already applied.
    applied: u64,
    /// Packets rove accepted but could not commit, kept for the report.
    refused_packets: usize,
    /// World sequences that committed but carried no instance work.
    empty_batches: usize,
    /// What every pump has done so far.
    totals: ApplyStats,
}

impl Session {
    pub fn new() -> Result<Session, SessionError> {
        let core = SessionCore::new().map_err(|e| SessionError::Journal(e.to_string()))?;
        Ok(Session {
            core,
            reader: None,
            applied: 0,
            refused_packets: 0,
            empty_batches: 0,
            totals: ApplyStats::default(),
        })
    }

    /// The converter's refusals are counted here as well as by `Live`; the
    /// session keeps the run's totals so a report can print one line.
    pub fn totals(&self) -> &ApplyStats {
        &self.totals
    }

    pub fn refused_packets(&self) -> usize {
        self.refused_packets
    }

    pub fn empty_batches(&self) -> usize {
        self.empty_batches
    }

    /// The world sequence the document reflects.
    pub fn applied_sequence(&self) -> u64 {
        self.applied
    }

    pub fn journal_stats(&self) -> J {
        let s = self.core.journal_stats();
        serde_json::json!({
            "applied_sequence": self.applied,
            "refused_packets": self.refused_packets,
            "empty_batches": self.empty_batches,
            "committed_packets": self.core.committed_packets(),
            "journal": {
                "records": s.records,
                "bytes": s.accounted_bytes,
                "readers": s.readers,
                "oldest_sequence": s.oldest_sequence,
                "newest_sequence": s.newest_sequence,
                "lost_through": s.lost_through,
            },
        })
    }

    /// One payload from the connection. The order the server sends them in is
    /// the order they must arrive in: schema, globals, then replication.
    pub fn ingest(&mut self, payload: &[u8]) -> Result<Ingest, SessionError> {
        match self.core.ingest(payload) {
            Ok(outcome) => Ok(match outcome {
                rove_client_core::session::IngestOutcome::SchemaInstalled { classes, properties } => {
                    Ingest::Schema { classes, properties }
                }
                rove_client_core::session::IngestOutcome::GlobalsCommitted { world_sequence, .. } => {
                    Ingest::Globals { world_sequence }
                }
                rove_client_core::session::IngestOutcome::ReplicationCommitted {
                    world_sequence, ..
                } => Ingest::Replication { world_sequence },
            }),
            Err(e) => {
                self.refused_packets += 1;
                Err(SessionError::Refused(e.to_string()))
            }
        }
    }

    /// The packed packet kinds a live session receives, for a caller that
    /// forwards whole frames: `SessionCore` wants one payload at a time and
    /// refuses the kinds it does not read, so unwrapping here keeps the
    /// refusal counted rather than fatal.
    pub fn ingest_all(&mut self, payloads: &[Vec<u8>]) -> (usize, usize) {
        let mut ok = 0;
        let mut refused = 0;
        for payload in payloads {
            match self.ingest(payload) {
                Ok(_) => ok += 1,
                Err(_) => refused += 1,
            }
        }
        (ok, refused)
    }

    /// Apply whatever rove has committed since the last pump to `doc`.
    ///
    /// `limit` bounds the actions applied in one call; the remainder waits in
    /// the journal for the next frame, which is how a burst becomes a hitch
    /// instead of a freeze.
    pub fn pump(
        &mut self,
        doc: &mut Document,
        live: &mut Live,
        conv: &mut ValueConverter,
        limit: usize,
    ) -> Result<ApplyStats, SessionError> {
        let mut totals = ApplyStats::default();
        let reader = match self.reader {
            Some(reader) => reader,
            None => {
                // First pump: take the lease at the sequence already applied
                // (0 for a fresh session, or a snapshot's sequence for a world
                // that was built offline first).
                let reader = self
                    .core
                    .retain(self.applied)
                    .map_err(|e| SessionError::Journal(e.to_string()))?;
                self.reader = Some(reader);
                reader
            }
        };

        loop {
            let entries = self
                .core
                .read_after(reader, self.applied, limit)
                .map_err(|e| SessionError::Journal(e.to_string()))?;
            if entries.is_empty() {
                break;
            }
            let mut applied_this_round = 0usize;
            for entry in &entries {
                if entry.sequence <= self.applied {
                    continue;
                }
                let record = &entry.value;
                let actions: Vec<J> = record
                    .world_delta
                    .get("actions")
                    .and_then(J::as_array)
                    .cloned()
                    .unwrap_or_default();
                if actions.is_empty() {
                    self.empty_batches += 1;
                }
                let stats = live.apply(doc, &actions, conv);
                totals.add(&stats);
                applied_this_round += stats.actions.max(1);
                self.applied = entry.sequence;
                if applied_this_round >= limit {
                    break;
                }
            }
            // Acknowledging lets the journal drop what every reader has passed;
            // without it the window fills and `read_after` stops answering.
            self.core
                .acknowledge(reader, self.applied)
                .map_err(|e| SessionError::Journal(e.to_string()))?;
            if applied_this_round == 0 || applied_this_round < limit {
                break;
            }
        }

        self.totals.add(&totals);
        Ok(totals)
    }

    /// The events in the journal that the pump has already applied, for the
    /// engine to dispatch to `LocalScript`s. Consumed from the front so a
    /// caller that drains once per frame cannot dispatch one twice.
    pub fn take_events(&mut self, into: &mut VecDeque<J>) -> usize {
        let reader = match self.reader {
            Some(reader) => reader,
            None => return 0,
        };
        let Ok(entries) = self
            .core
            .read_after(reader, self.applied.saturating_sub(1), ACTIONS_PER_PUMP)
        else {
            return 0;
        };
        let mut taken = 0;
        for entry in entries.iter().filter(|e| e.sequence <= self.applied) {
            for event in &entry.value.events {
                into.push_back(event.clone());
                taken += 1;
            }
        }
        taken
    }
}

impl Default for Session {
    fn default() -> Self {
        // `SessionCore::new` only fails on limits, which are our constants.
        Session::new().expect("a session with the default limits")
    }
}
