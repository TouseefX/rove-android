//! The session: a real WebTransport connection, rove's decoder, and the bridge.
//!
//! This is the skeleton the live join runs, and the same code the offline
//! evidence exercises through a channel instead of a socket:
//!
//! ```text
//!  dial ──► login packets (app 4, channel 1) ──► server
//!                     │
//!                     └── receives ──► SessionCore::ingest ──► journal ──► Live::apply
//! ```
//!
//! Two seams make that literal rather than aspirational:
//!
//! - [`Lane`] is one reliable channel: a real `wtransport` stream in a live
//!   session, a channel fed by a recording in a test. [`run`] is written against
//!   it and does not know which it has.
//! - [`Session`] owns the decoder, the journal reader and the bridge, and
//!   [`Session::feed`] is the one place bytes become world state. A live join
//!   and a replay of a recording therefore take the same path.
//!
//! What a live join adds on top of a replay is the five login packets and the
//! control channels the server expects opened first; both are here, and both
//! are in the log so a failed join says which stage it reached.

use std::fmt;

use rove_client_core::journal::ReaderId;
use rove_client_core::session::{IngestOutcome, SessionCore};
use rustudio_core::doc::Document;
use rustudio_live::live::{ApplyStats, Live};
use rustudio_live::snapshot::ValueConverter;

use crate::allocation::IssuedJoinAllocation;
use crate::login;

#[cfg(feature = "transport")]
use rove_client_core::timing::{SessionWireClock, TimeSyncProbe};
#[cfg(feature = "transport")]
use rove_client_core::transport::session::{
    receive_unreliable, ReliableReceiver, ReliableSender,
};
#[cfg(feature = "transport")]
use rove_client_core::transport::{connect_webtransport, IssuedAllocation};

/// The application and channel a join's login packets and replication stream
/// travel on. Verified against rove's own live session (`live_session.rs`):
/// control opens on applications 0, 4 and 6, and the five login packets go out
/// on application 4, channel 1 — the same channel the server answers on.
pub const REPLICATION_APPLICATION: u8 = 4;
pub const REPLICATION_CHANNEL: u32 = 1;

/// Replication pings, on the same application as the world. Upstream's
/// `ping_responder::PING_CHANNEL`; before globals, byte 131 here is a ping
/// rather than replication, which is why the dispatch has to look at both.
pub const PING_CHANNEL: u32 = 3;

/// Compact instance-state packets (161), where most of a real place's world
/// arrives. Upstream routes them on application 4 channel 5.
pub const INSTANCE_STATE_CHANNEL: u32 = 5;

/// The bytecode range `protocol_sync` offers, and the capability bit that says
/// this client asked for it. Upstream's `BYTECODE_RANGE_CAPABILITY`, and the
/// pair has to agree: offering the range without the capability (or the other
/// way round) asks the server for a feature the handshake says we cannot use.
pub const BYTECODE_RANGE: (u8, u8) = (13, 14);
pub const BYTECODE_RANGE_CAPABILITY: u64 = 0x10_0000;

/// World packets that may wait for globals. A real join sent 793 replication
/// packets before globals arrived; 4096 leaves room for a much larger place
/// while keeping a server that never sends globals from growing the buffer
/// without limit.
const HELD_PACKETS: usize = 4096;

/// How many journal rows one read takes. The journal is read in batches and
/// drained fully, so this is a page size and not a limit.
const DRAIN_BATCH: usize = 64;

/// Where a session got to. A `stage` is what a diagnostics overlay shows when a
/// join fails: the difference between "we never dialled" and "we dialled and the
/// server hung up" is the whole diagnosis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Nothing has happened yet.
    Idle,
    /// The allocation parsed and gave us somewhere to dial.
    Allocated,
    /// The WebTransport CONNECT was accepted.
    Connected,
    /// The five login packets were written.
    LoggedIn,
    /// The first packet from the server arrived and was fed to the decoder.
    Receiving,
    /// The connection ended, cleanly or not.
    Closed(&'static str),
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stage::Idle => f.write_str("idle"),
            Stage::Allocated => f.write_str("allocated"),
            Stage::Connected => f.write_str("connected"),
            Stage::LoggedIn => f.write_str("logged in"),
            Stage::Receiving => f.write_str("receiving"),
            Stage::Closed(reason) => write!(f, "closed: {reason}"),
        }
    }
}

/// One reliable channel: what a session sends and receives on.
///
/// Implemented for rove's `wtransport` streams (live) and for in-memory
/// channels (tests). `next` returning `None` is the peer finishing the stream,
/// which is a normal end, not an error.
#[cfg(feature = "transport")]
#[allow(async_fn_in_trait)]
pub trait Lane {
    async fn send(&mut self, payload: &[u8]) -> Result<(), &'static str>;
    async fn next(&mut self) -> Result<Option<Vec<u8>>, &'static str>;
}

/// What a session did with the bytes it was given.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ingested {
    pub packets: usize,
    pub schema: usize,
    pub globals: usize,
    pub replication: usize,
    /// Packets the decoder refused. A live session keeps going: a refusal is
    /// data about the server, not a reason to drop the connection.
    pub refused: usize,
    /// Packets accepted but journaled nothing (a schema).
    pub without_actions: usize,
}

/// The decoder, the journal reader and the bridge, together.
///
/// Created once per session and fed for its whole life. The journal is read
/// with a cursor rather than drained, so an acknowledgement can be added later
/// without changing this shape.
pub struct Session {
    core: SessionCore,
    document: Document,
    /// rustudio's bridge: rove's journal actions become `Document` instances.
    /// The same `Live` the offline replay uses, so a live join and a recording
    /// cannot diverge in how they read the decoder's output.
    live: Live,
    converter: ValueConverter,
    /// Opaque journal handle, as rove hands it out. The read cursor is the
    /// sequence number, which is a `u64`.
    reader: ReaderId,
    cursor: u64,
    pub ingested: Ingested,
    pub applied: ApplyStats,
    pub stage: Stage,
    /// What arrived, by `(application, channel, first byte)`, counted: a
    /// session that received 900 packets the decoder does not want reads the
    /// same as a silent one without this.
    pub arrivals: std::collections::BTreeMap<(u8, u32, u8), usize>,
    /// Streams the server opened that were not the world's, read anyway.
    pub other_streams: usize,
    /// Replication pings the server asked for. Unanswered for now, and said so
    /// in the report rather than padded with an invented reply.
    pub pings: usize,
    /// Features the server's table offered, when one arrived.
    pub features: usize,
    /// Markers the server acknowledged.
    pub markers: usize,
    /// Packets on a channel this client does not dispatch: counted so the
    /// report can name a stream the session saw and did nothing with.
    pub unhandled: usize,
    /// World packets held because globals had not arrived yet, and how many of
    /// those were later fed. A server sends replication on more than one
    /// stream, so arrival order is not protocol order: on a real join 793
    /// replication packets arrived before globals, and refusing them (which is
    /// what the decoder does without a world to apply them to) lost that part
    /// of the place.
    pub held: usize,
    pub released: usize,
    /// Held packets dropped because the bound was reached.
    pub dropped: usize,
    /// World packets waiting for globals, in arrival order.
    held_packets: std::collections::VecDeque<(u8, u32, Vec<u8>)>,
    /// Reliable streams the server opened and this session accepted. A report
    /// that says "no packet arrived" has to say how many streams it was
    /// reading, or it cannot tell silence from a stream that never opened.
    pub streams: usize,
    /// Unreliable datagrams read, and the time-sync probe's requests and
    /// replies. The probe is part of the pre-login handshake, so its silence is
    /// a diagnosis: a request with no reply is a server that is not answering.
    pub datagrams: usize,
    pub sync_requests: usize,
    pub sync_replies: usize,
    /// Where the world goes while the session runs (M3). `None` in the offline
    /// replay and the harness, which read the document when the session ends.
    ///
    /// The editor cannot be given `&Document` across threads and cannot be given
    /// a `Document` it must give back later, so the handover is a *swap*: this
    /// field is the editor's side of it (`live_world::LiveWorld`), and after
    /// every drain the world here changes hands with the one the renderer is
    /// drawing.
    pub handover: Option<crate::live_world::LiveWorld>,
}

impl Session {
    /// A session over an empty place. The document is a bare DataModel, which
    /// is what a join's globals packet fills with the place's roots.
    pub fn new() -> Self {
        // `with_limits_live`: a join's packets come from a real server, and a
        // record this client cannot fully decode must cost that record, not
        // the packet (upstream's lenient ISR policy; see rove's session.rs).
        let mut core = SessionCore::with_limits_live(4096, 8 << 20)
            .expect("a session with sane limits");
        let reader = core.retain(0).expect("a journal reader");
        Self {
            core,
            document: Document::empty(),
            live: Live::new(),
            converter: ValueConverter::default(),
            reader,
            cursor: 0,
            ingested: Ingested::default(),
            applied: ApplyStats::default(),
            stage: Stage::Idle,
            arrivals: std::collections::BTreeMap::new(),
            other_streams: 0,
            pings: 0,
            features: 0,
            markers: 0,
            unhandled: 0,
            streams: 0,
            held: 0,
            released: 0,
            dropped: 0,
            held_packets: std::collections::VecDeque::new(),
            datagrams: 0,
            sync_requests: 0,
            sync_replies: 0,
            handover: None,
        }
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    /// The world, moved out and replaced with an empty one.
    ///
    /// For the end of a session, when a caller on another thread (the join
    /// worker) hands the place it built to the editor. A `Document` is not
    /// `Clone` and is not made so here: it owns a DOM and a journal, and two
    /// copies of a live place would be two different worlds.
    pub fn take_document(&mut self) -> Document {
        std::mem::replace(&mut self.document, Document::empty())
    }

    pub fn document_mut(&mut self) -> &mut Document {
        &mut self.document
    }

    /// Feed one packet. Bytes are handed to the decoder as they are: rove's
    /// readers know their own framing (the schema carries a zstd envelope, the
    /// globals packet does not), so a session must not re-frame anything.
    ///
    /// A refusal is counted and returned as `false`; it never aborts the
    /// session.
    pub fn feed(&mut self, payload: &[u8]) -> bool {
        // A replay has no channel of its own; the world's is the one it stands
        // for. Routed through the dispatch so a recording takes the same path
        // a live join does, holding included.
        self.feed_on(REPLICATION_APPLICATION, REPLICATION_CHANNEL, payload)
    }

    /// A packet that arrived on a known channel: dispatch on where it came
    /// from, then feed the world's packets to the decoder.
    ///
    /// The dispatch is not decoration. Upstream routes on
    /// `(application, channel, first byte)` because a server sends several
    /// kinds of packet on the same reliable stream, and two of them collide
    /// with the world's ids:
    ///
    /// - a **replication ping** is `0x83` — the same first byte as replication
    ///   (131) — but arrives on application 4 channel 3, and decoding it as
    ///   replication would corrupt the world;
    /// - the **feature table** is `0x93` (147) and the **marker** reply is
    ///   `0x84` (132), both on channel 1, and both are handshake steps rather
    ///   than world state.
    ///
    /// Returns whether the packet was world state the decoder accepted.
    pub fn feed_on(&mut self, application: u8, channel: u32, payload: &[u8]) -> bool {
        self.note_arrival(application, channel, payload);
        let kind = payload.first().copied().unwrap_or(0);
        match (application, channel, kind) {
            // The schema and the globals: these are what make a world, and
            // they are fed in whichever order they arrive.
            (REPLICATION_APPLICATION, REPLICATION_CHANNEL, 129 | 151) => {
                let globals = kind == 129;
                let fed = self.feed_inner_on(application, channel, payload);
                // Globals are what the held packets were waiting for.
                if globals && fed {
                    self.release_held();
                }
                fed
            }
            // Replication needs globals. They arrive on a *different* stream
            // (4/2) from the replication stream, so a server's packets are in
            // protocol order on the wire and not in arrival order here: on a
            // real join 793 replication packets arrived before globals, and the
            // decoder refuses each of them without a world. Holding them and
            // feeding them once globals commit is what makes the whole place
            // arrive instead of most of it.
            (REPLICATION_APPLICATION, REPLICATION_CHANNEL, 131) => {
                if self.core.scope().is_none() {
                    self.hold(application, channel, payload);
                    false
                } else {
                    self.feed_inner_on(application, channel, payload)
                }
            }
            // Instance state (161) rides application 4 channel 5, and is the
            // packet most of a real place's world arrives in. It needs a world
            // for the same reason replication does.
            (REPLICATION_APPLICATION, INSTANCE_STATE_CHANNEL, 161) => {
                if self.core.scope().is_none() {
                    self.hold(application, channel, payload);
                    false
                } else {
                    self.feed_instance_state(payload)
                }
            }
            // Replication on channel 3 is the world *after* globals; before
            // them the same kind is a ping (upstream's `if scope.is_none()`).
            (REPLICATION_APPLICATION, PING_CHANNEL, 131) => {
                if self.core.scope().is_none() {
                    self.ping_requested(payload)
                } else {
                    self.feed_inner_on(application, channel, payload)
                }
            }
            // The server's feature-name/value table: the one packet that says
            // which encoding the server will use for the session.
            (REPLICATION_APPLICATION, REPLICATION_CHANNEL, 147) => self.features_offered(payload),
            // A marker: the server telling the client its initial world tag
            // round-trip succeeded.
            (REPLICATION_APPLICATION, REPLICATION_CHANNEL, 132) => {
                self.markers += 1;
                true
            }
            _ => {
                self.unhandled += 1;
                false
            }
        }
    }

    /// The feature table: names and values, in wire order.
    fn features_offered(&mut self, payload: &[u8]) -> bool {
        match crate::login::feature_configuration(payload) {
            Ok(entries) => {
                // The same switch rove's client makes from this packet: the
                // adaptive property compressor decides the property encoding.
                let legacy_pv = entries
                    .iter()
                    .find(|(name, _)| name == "DebugAdaptivePVCompressor")
                    .is_some_and(|(_, value)| value == "false");
                self.core.set_instance_state_legacy_pv(legacy_pv);
                self.features = entries.len();
                true
            }
            Err(error) => {
                log::warn!("session: feature table refused: {error}");
                self.ingested.refused += 1;
                false
            }
        }
    }

    /// A replication ping. Counted, not answered.
    ///
    /// A reply must carry measured telemetry — this session's replication
    /// statistics and integrity inventory — and upstream blocks a reply rather
    /// than invent one (`PingTelemetryProvider` has no defaults for exactly
    /// this reason). Until that measurement is ported, a fabricated reply would
    /// be a lie told to the server in its own protocol, so the count is what
    /// the report carries and the answer is left out.
    fn ping_requested(&mut self, payload: &[u8]) -> bool {
        self.pings += 1;
        let _ = payload;
        false
    }

    /// A compact instance-state packet (161), which upstream reads through
    /// `ingest_instance_state` rather than `ingest`.
    fn feed_instance_state(&mut self, payload: &[u8]) -> bool {
        self.ingested.packets += 1;
        match self.core.ingest_instance_state(payload) {
            Ok(Some(outcome)) => {
                match outcome {
                    IngestOutcome::SchemaInstalled { .. } => self.ingested.schema += 1,
                    IngestOutcome::GlobalsCommitted { .. } => self.ingested.globals += 1,
                    IngestOutcome::ReplicationCommitted { .. } => self.ingested.replication += 1,
                }
                self.drain();
                if self.stage == Stage::LoggedIn {
                    self.stage = Stage::Receiving;
                }
                true
            }
            Ok(None) => {
                // Every record in it was dropped, or the decoder is not ready
                // yet. Not an error: upstream returns an empty packet list.
                self.ingested.without_actions += 1;
                false
            }
            Err(error) => {
                self.ingested.refused += 1;
                log::warn!("session: instance-state packet refused: {error}");
                false
            }
        }
    }

    fn note_arrival(&mut self, application: u8, channel: u32, payload: &[u8]) {
        let kind = payload.first().copied().unwrap_or(0);
        *self.arrivals.entry((application, channel, kind)).or_default() += 1;
    }

    /// Hold a world packet until the session has a world to apply it to.
    ///
    /// Bounded: a server that never sends globals must not turn a join into an
    /// unbounded buffer, and the count is reported either way.
    fn hold(&mut self, application: u8, channel: u32, payload: &[u8]) {
        self.held_packets
            .push_back((application, channel, payload.to_vec()));
        self.held += 1;
        while self.held_packets.len() > HELD_PACKETS {
            // Oldest first: the world's own order matters more than any single
            // packet, and the report says how many were dropped this way.
            self.held_packets.pop_front();
            self.dropped += 1;
        }
    }

    /// Feed what was held, in arrival order, once globals have committed.
    ///
    /// Each held packet keeps the channel it arrived on, so it goes back
    /// through the same choice it would have taken had globals been first:
    /// instance state (161) to `ingest_instance_state`, everything else to the
    /// replication decoder. Feeding a 161 as replication is a corrupt world.
    fn release_held(&mut self) {
        while let Some((application, channel, payload)) = self.held_packets.pop_front() {
            self.released += 1;
            match (application, channel, payload.first().copied().unwrap_or(0)) {
                (REPLICATION_APPLICATION, INSTANCE_STATE_CHANNEL, 161) => {
                    self.feed_instance_state(&payload);
                }
                _ => {
                    self.feed_inner_on(application, channel, &payload);
                }
            }
        }
    }

    fn feed_inner(&mut self, payload: &[u8]) -> bool {
        self.feed_inner_on(REPLICATION_APPLICATION, REPLICATION_CHANNEL, payload)
    }

    fn feed_inner_on(&mut self, application: u8, channel: u32, payload: &[u8]) -> bool {
        self.ingested.packets += 1;
        match self.core.ingest(payload) {
            Ok(outcome) => {
                match outcome {
                    IngestOutcome::SchemaInstalled { .. } => self.ingested.schema += 1,
                    IngestOutcome::GlobalsCommitted { .. } => self.ingested.globals += 1,
                    IngestOutcome::ReplicationCommitted { .. } => self.ingested.replication += 1,
                }
                if matches!(outcome, IngestOutcome::SchemaInstalled { .. }) {
                    // A schema installs the decoder and journals no world delta.
                    self.ingested.without_actions += 1;
                } else {
                    self.drain();
                }
                if self.stage == Stage::LoggedIn {
                    self.stage = Stage::Receiving;
                }
                true
            }
            Err(error) => {
                self.ingested.refused += 1;
                log::warn!("session: packet refused: {error}");
                false
            }
        }
    }

    /// Apply every record committed since the last drain.
    ///
    /// Loops until the journal hands back a short batch: a real server commits
    /// more than [`DRAIN_BATCH`] rows per packet often enough that a single
    /// read per packet would fall behind and never catch up, leaving a world
    /// that is always a few hundred instances short of what arrived.
    fn drain(&mut self) {
        while self.drain_batch() == DRAIN_BATCH {}
        // The overlay's numbers, whether or not anything was applied: a second
        // in which the world did not change is not a stuck session, and the
        // only way to tell those apart is for the counters to keep moving.
        if let Some(handover) = &self.handover {
            handover.set_counters(self.overlay_counters());
        }
    }

    /// True when the editor asked for the session to end (M3).
    ///
    /// Without a handover there is nobody to ask, and a session with a deadline
    /// ends at its deadline exactly as before.
    pub fn stop_requested(&self) -> bool {
        self.handover
            .as_ref()
            .is_some_and(|handover| handover.stop_requested())
    }

    /// The counters the overlay draws. Cheap: struct copies, no document reads.
    pub fn overlay_counters(&self) -> crate::live_world::Counters {
        crate::live_world::Counters {
            stage: match self.stage {
                Stage::Idle => "idle",
                Stage::Allocated => "allocate",
                Stage::Connected => "connect",
                Stage::LoggedIn => "log in",
                Stage::Receiving => "receive",
                Stage::Closed(_) => "closed",
            }
            .to_string(),
            packets: self.ingested.packets,
            schema: self.ingested.schema,
            globals: self.ingested.globals,
            replication: self.ingested.replication,
            refused: self.ingested.refused,
            held: self.held,
            released: self.released,
            dropped: self.dropped,
            instances: self.live.len(),
            other_streams: self.other_streams,
            pings: self.pings,
            sync_requests: self.sync_requests,
            sync_replies: self.sync_replies,
            rows: self.journal_rows(),
            seconds: 0,
        }
    }

    /// One read of up to [`DRAIN_BATCH`] rows; returns how many were read.
    fn drain_batch(&mut self) -> usize {
        let rows = match self.core.read_after(self.reader, self.cursor, DRAIN_BATCH) {
            Ok(rows) => rows,
            Err(error) => {
                log::warn!("session: journal read failed: {error}");
                return 0;
            }
        };
        let read = rows.len();
        for row in rows {
            // `SessionDelta` derefs to rove's delta JSON, so this is the
            // actions array rather than a field of the record.
            let actions = row.value.world_delta["actions"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            self.cursor = row.sequence;
            if actions.is_empty() {
                self.ingested.without_actions += 1;
                continue;
            }
            let stats = self.live.apply(&mut self.document, &actions, &mut self.converter);
            self.applied.add(&stats);
            // The editor draws its own document, and this is what keeps the two
            // in step: the same actions, in the same order, applied by the same
            // `Live` on the other side of the thread boundary (M3).
            //
            // Published *after* applying, so a batch this side could not use is
            // never sent as if it had been, and with the counters of the moment
            // so the overlay describes the world the batch is about to move.
            if let Some(handover) = &self.handover {
                handover.publish(actions.clone(), self.overlay_counters());
            }
            log::debug!(
                "session: applied {} action(s) at sequence {}: {:?}",
                actions.len(),
                row.sequence,
                stats
            );
        }
        read
    }

    /// Rows the journal holds. A counter, not a scan: this is polled by a
    /// progress callback while the session runs, and `read_after(.., 4096)`
    /// walks the whole journal every time it is asked.
    pub fn journal_rows(&self) -> usize {
        self.core.journal_stats().records
    }

    /// Alias kept for the tests and the `Debug` impl, which ask by this name.
    pub fn journal_records(&self) -> usize {
        self.journal_rows()
    }

    pub fn rejected(&self) -> u64 {
        self.core.rejected_packets()
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // A reader lease holds journal entries alive; releasing it here keeps a
        // long-lived session from pinning rows it has already applied.
        let _ = self.core.release(self.reader);
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("stage", &self.stage)
            .field("ingested", &self.ingested)
            .field("journal_records", &self.journal_records())
            .finish()
    }
}

/// Everything a live dial needs, derived from an allocation that parsed.
#[cfg(feature = "transport")]
pub fn transport_allocation(issued: &IssuedJoinAllocation) -> Result<IssuedAllocation, &'static str> {
    if !issued.is_transport_ready() {
        return Err("allocation is missing an endpoint, backend, token or certificate pin");
    }
    Ok(IssuedAllocation {
        endpoint: issued.endpoint.expect("checked"),
        backend: issued.backend.expect("checked"),
        token: *issued.token().expect("checked"),
        certificate_sha256: issued.certificate_sha256.clone(),
        // The offer is a statement about what *this client* implements, not
        // about the server, and the server intersects it. `protocol_sync` now
        // sends the 13..14 bytecode range, so the range has to be offered:
        // rove's live session sets exactly this bit for exactly this reason,
        // and logs in with `Some((13, 14))` to match.
        capabilities: BYTECODE_RANGE_CAPABILITY,
    })
}

/// Streams the server may open before this client stops accepting.
///
/// Upstream's `MAX_STREAMS` is 256, and the number is not decorative: place
/// 1818 opened **fourteen** streams in the first millisecond of a real session
/// (applications 0, 4 and 6 across a dozen channels), so a cap of 16 — what
/// this file asked for when it was written — was one busy place away from
/// failing the join for a reason that had nothing to do with the join.
#[cfg(feature = "transport")]
const MAX_STREAMS: usize = 256;

/// How many read packets may wait for the feed loop. Bounded so a server that
/// floods a stream cannot grow the queue without limit; a reader that finds it
/// full waits, which is back-pressure rather than a loss.
#[cfg(feature = "transport")]
const READER_QUEUE: usize = 8192;

/// Run one live join against the issued allocation, feeding `session` until the
/// peer finishes or the deadline passes.
///
/// The order here is the protocol, and the log says which step was reached:
/// connect, open the control channels, write the five login packets, then read.
///
/// ## Why the loop is shaped like this
///
/// A device showed two faults in the earlier shape of this function, and each
/// one is visible in the log:
///
/// 1. **Packets were fed after the loop.** They were collected into a `Vec` and
///    handed to the decoder once the loop ended. A server keeps its reliable
///    streams open for the life of the session, so "after the loop" was the
///    deadline: for thirty seconds the overlay said *logged in* and the counters
///    stayed at zero. Now every reader task hands what it reads to this loop
///    through a channel, and the loop feeds it at once — the same shape as
///    rove's client (`enqueue` → world owner), which is what makes the counters
///    move while the join is still running.
/// 2. **The final drain joined without a bound.** `readers.join_next().await`
///    on a stream the peer has not closed waits forever, so a join could sit
///    past its own deadline with the runtime blocked — the log stopped at the
///    last accepted stream and never reached the summary line below it. Every
///    wait here is bounded now, and the drain aborts what is left.
///
/// A real server also opens streams that are not the world's, and their packets
/// are not inert: application 4 channel 3 carries replication pings, which a
/// client must answer or the server throttles it; channel 1 carries the feature
/// table (`0x93`), which selects the property encoding. Upstream dispatches on
/// `(application, channel, first byte)`; so does this loop.
#[cfg(feature = "transport")]
pub async fn run(
    issued: &IssuedJoinAllocation,
    session: &mut Session,
    seconds: u64,
    mut progress: impl FnMut(&Session),
) -> Result<Stage, &'static str> {
    let allocation = transport_allocation(issued)?;
    session.stage = Stage::Allocated;
    let (endpoint, connection, _socket) = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        connect_webtransport(&allocation),
    )
    .await
    .map_err(|_| "connect timed out")?
    .map_err(|error| {
        log::warn!("session: connect failed: {error}");
        "connect was refused"
    })?;
    session.stage = Stage::Connected;
    log::info!("session: connected to {}", allocation.endpoint);
    // A caller polls a join from a render loop; `progress` is what makes those
    // polls say "connected" rather than "idle" while the session runs.
    progress(session);

    // The server expects the control applications announced before a login is
    // read. rove's live session sends one empty control-open on each of 0, 4
    // and 6 and keeps using them; the same here.
    let sender = ReliableSender::new(&connection);
    for application in [0, 4, 6] {
        sender
            .send(application, rove_client_core::channels::CONTROL_CHANNEL, &[0, 0])
            .await
            .map_err(|_| "control channel could not be opened")?;
    }

    // The capability the login packets ask for has to be the one the handshake
    // negotiated. Before this check a server that did not advertise it was sent
    // a range it had refused, and rove's live session closes the connection at
    // exactly this point rather than continue into a mismatched login.
    let negotiated = connection
        .capability_negotiation()
        .map(|caps| caps.negotiated())
        .ok_or("the server negotiated no capabilities")?;
    if negotiated & BYTECODE_RANGE_CAPABILITY == 0 {
        connection.close(0u32.into(), b"bytecode range unsupported");
        return Err("the server did not negotiate the bytecode range");
    }
    log::info!("session: capabilities negotiated: {negotiated:016x}");

    // --- Before the login packets: the clock and time-sync the server expects
    // a client to have set up. rove's live session does exactly this, in this
    // order, and the omitted steps are not decoration: the replicator register
    // (6/1) and the two announced clock datagram channels (6/2 on wire 2, 6/3
    // on wire 4) are what the server's timing probe answers on, and it waits.
    let mut clock = SessionWireClock::new();
    clock.initialize_replicator().map_err(|_| "the wire clock could not be initialized")?;
    sender
        .open(6, 1)
        .await
        .map_err(|_| "the clock register could not be opened")?;
    for (channel, wire) in [(2u8, 2u32), (3, 4)] {
        let announcement =
            rove_client_core::channels::control_open(6, channel, wire)
                .map_err(|_| "a clock channel could not be announced")?;
        sender
            .send(6, rove_client_core::channels::CONTROL_CHANNEL, &announcement)
            .await
            .map_err(|_| "a clock channel could not be announced")?;
    }
    let mut sync = TimeSyncProbe::default();
    sync.start(&clock).map_err(|_| "the time-sync probe could not be started")?;
    log::info!("session: wire clock initialized, clock channels 6/1, 6/2, 6/3 announced");

    // The five login packets, in the order the server reads them.
    let packets = login_packets(issued).map_err(|_| "login packets could not be encoded")?;
    for (index, packet) in packets.iter().enumerate() {
        sender
            .send(REPLICATION_APPLICATION, REPLICATION_CHANNEL, packet)
            .await
            .map_err(|_| "a login packet could not be sent")?;
        log::info!("session: login packet {} ({} bytes) sent", index + 1, packet.len());
    }
    session.stage = Stage::LoggedIn;
    progress(session);

    // Every stream the server opens, as rove's client does — not only the
    // first. A real server opens several (login replies, pings, the world), and
    // reading one of them is how a session sits at "logged in" while the world
    // it asked for arrives on another. Each has its own header, which is where
    // its application and channel come from.
    // `seconds` is a **bound**, not a session length: `0` means "until the
    // connection ends or the caller stops it", which is what a live session is
    // for. A non-zero value is a harness's or a test's timeout, and the report
    // says which one ended the session so the difference is never guessed at.
    let deadline = if seconds == 0 {
        None
    } else {
        Some(std::time::Instant::now() + std::time::Duration::from_secs(seconds))
    };
    let (packets_tx, mut packets_rx) = tokio::sync::mpsc::channel::<(u8, u32, Vec<u8>)>(READER_QUEUE);
    let mut readers = tokio::task::JoinSet::new();
    let mut fed = 0usize;
    // Why the loop stopped; `None` means the peer finished every stream.
    let end: Option<&'static str>;
    let mut accepting = Box::pin(ReliableReceiver::accept(&connection));
    // The deadline as a future, not a duration recomputed per turn: a
    // `sleep(remaining)` arm would be ready immediately on every loop and
    // would compete with arriving packets for turns.
    // A future that never resolves, for a session with no deadline: the select
    // needs an arm, and "no deadline" must not become "a deadline in the year
    // 2255" that a `sleep` still has to be polled for every turn.
    let mut sleep_until_deadline: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
        match deadline {
            Some(at) => Box::pin(tokio::time::sleep_until(tokio::time::Instant::from_std(at))),
            None => Box::pin(std::future::pending()),
        };
    // A dropped connection ends the join now, not at the deadline: before this
    // arm existed, a server that hung up was indistinguishable from one that
    // had gone quiet, and both were reported as the deadline passing.
    let mut connection_closed = Box::pin(connection.closed());
    // The probe emits a request on an interval and is quiet otherwise, so it is
    // polled on a coarse tick rather than on every turn. Its replies arrive as
    // datagrams, never on a reliable stream, which is why the loop reads them.
    let mut sync_tick = tokio::time::interval(std::time::Duration::from_millis(20));
    sync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if deadline.is_some_and(|at| std::time::Instant::now() >= at) {
            end = Some("the session's deadline passed");
            break;
        }
        // The editor's Stop: the session ends the way a person asked it to.
        if session.stop_requested() {
            end = Some("stopped");
            break;
        }
        tokio::select! {
            accepted = &mut accepting => {
                let application = match &accepted {
                    Ok(receiver) => receiver.application(),
                    Err(error) => {
                        // A real server opens its streams at once, so this is
                        // either the connection ending or a server that never
                        // opened one; the first packet path can still work.
                        log::warn!("session: no further reliable stream: {error}");
                        // Still read what is already queued: a server that
                        // opened its streams and then closed the connection
                        // leaves its packets behind.
                        end = Some("no further stream could be accepted");
                        break;
                    }
                };
                let mut receiver = accepted.expect("checked");
                let channel = receiver.channel();
                log::info!("session: the server opened application {application} channel {channel}");
                session.streams += 1;
                if session.streams > MAX_STREAMS {
                    end = Some("the server opened too many streams");
                    break;
                }
                if application != REPLICATION_APPLICATION || channel != REPLICATION_CHANNEL {
                    // Read it anyway: this is where a ping request or a login
                    // reply arrives, and a refused one is data. The counters
                    // keep the two apart.
                    session.other_streams += 1;
                }
                accepting = Box::pin(ReliableReceiver::accept(&connection));
                let packets = packets_tx.clone();
                readers.spawn(async move {
                    // Constants of this stream, read once: `receiver.next()`
                    // borrows it mutably, so reading them inside a `match` on
                    // its result would hold that borrow across the arms.
                    loop {
                        let read = receiver.next().await;
                        match read {
                            Ok(Some(payload)) => {
                                if packets.send((application, channel, payload)).await.is_err() {
                                    break;
                                }
                            }
                            Ok(None) => break,
                            Err(error) => {
                                log::warn!(
                                    "session: stream {application}/{channel} read failed: {error}"
                                );
                                break;
                            }
                        }
                    }
                });
            }
            // A packet as it arrives: this arm is the point. `feed_on` applies
            // it to the world before the next packet is read, so the counters
            // and the document both move while the streams are open.
            packet = packets_rx.recv() => {
                let Some((application, channel, payload)) = packet else {
                    end = Some("every stream reader finished");
                    break;
                };
                session.feed_on(application, channel, &payload);
                // Republished every 16 packets: often enough that the overlay
                // moves, rarely enough that the lock is not the hot path.
                fed += 1;
                if fed % 16 == 0 {
                    progress(session);
                }
            }
            Some(joined) = readers.join_next(), if !readers.is_empty() => {
                if let Err(error) = joined {
                    // A reader that failed is not the session's end: the other
                    // streams are still readable, and its packets are counted.
                    log::warn!("session: a stream reader failed: {error}");
                }
            }
            // A time-sync request due now. Unreliable on purpose: it is a
            // probe, and a lost one is retried on the next interval.
            _ = sync_tick.tick() => {
                match sync.due(&clock) {
                    Ok(Some(packet)) => {
                        session.sync_requests += 1;
                        if let Err(error) =
                            rove_client_core::transport::session::send_unreliable(
                                &connection,
                                4,
                                session.sync_requests as u64,
                                &packet,
                            )
                        {
                            log::warn!("session: time-sync request could not be sent: {error}");
                        }
                    }
                    Ok(None) => {}
                    Err(error) => log::warn!("session: time-sync clock error: {error}"),
                }
            }
            // A datagram. Only the time-sync channel's replies are interpreted
            // here; everything else is counted, because a datagram a server
            // sent and nobody read sits in the connection's queue, and the
            // probe's answer is exactly the kind of packet that never arrives
            // on a reliable stream.
            datagram = receive_unreliable(&connection) => {
                session.datagrams += 1;
                match datagram {
                    Ok(packet) => {
                        if packet.wire_id == 4 && packet.complete {
                            match sync.receive(&packet.payload, &clock, true) {
                                Ok(true) => session.sync_replies += 1,
                                Ok(false) => {}
                                Err(error) => {
                                    log::warn!("session: time-sync reply refused: {error}")
                                }
                            }
                        }
                    }
                    // A datagram that will not parse is not the end of the
                    // connection: count it and read the next one.
                    Err(error) => log::debug!("session: a datagram was unreadable: {error}"),
                }
            }
            _ = &mut connection_closed => {
                log::warn!("session: the connection ended");
                end = Some("the connection ended");
                break;
            }
            _ = &mut sleep_until_deadline => {
                end = Some("the session's deadline passed");
                break;
            }
        }
    }
    // Nothing below may wait unbounded: a peer that keeps a stream open is the
    // normal case, not an error, and it is what stalled a join past its own
    // deadline before. Take what is already queued, then abort the readers.
    drop(packets_tx);
    while let Ok((application, channel, payload)) = packets_rx.try_recv() {
        session.feed_on(application, channel, &payload);
    }
    readers.abort_all();
    while readers.join_next().await.is_some() {}
    log::info!(
        "session: {} stream(s) accepted, {} packet(s) fed, {} refusal(s), \
         {} other stream(s), {} datagram(s), time-sync {}/{}",
        session.streams,
        session.ingested.packets,
        session.ingested.refused,
        session.other_streams,
        session.datagrams,
        session.sync_replies,
        session.sync_requests
    );
    if let Some(reason) = end {
        session.stage = Stage::Closed(reason);
    }
    progress(session);
    // A join's own shutdown must not outlive its deadline either.
    connection.close(0u32.into(), b"done");
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), endpoint.wait_idle()).await;
    Ok(session.stage.clone())
}

/// The login packets for an allocation, in send order.
///
/// Exposed so a caller can inspect or record them without a socket, and so the
/// order is asserted in a test rather than buried in [`run`].
///
/// | # | packet | id |
/// |---|---|---|
/// | 1 | early auth, from the issued ticket | `0xa8` |
/// | 2 | protocol sync, the player object | `0x90` |
/// | 3 | a fresh verification nonce | `0x92` |
/// | 4 | the client ticket and its checksum | `0x8a` |
/// | 5 | no preferred spawn name | `0x8f` |
pub fn login_packets(issued: &IssuedJoinAllocation) -> Result<Vec<Vec<u8>>, &'static str> {
    let client_ticket = issued
        .tickets
        .client_ticket()
        .ok_or("the allocation has no client ticket")?;
    let early_auth = login::issued_early_auth(client_ticket)
        .map_err(|_| "the client ticket is not a join ticket")?;
    // The range, not `None`: the server reads this to decide which bytecode the
    // client will accept, and rove's live session sends `Some((13, 14))`.
    let sync = login::protocol_sync(
        &issued.join_fields,
        issued.join_ticket_value.as_ref(),
        Some(BYTECODE_RANGE),
    )
    .map_err(|_| "the player object could not be encoded")?;
    let verification = login::verification_packet().map_err(|_| "no verification nonce")?;
    let ticket = login::ticket_packet(&issued.join_fields)
        .map_err(|_| "the ticket packet could not be encoded")?;
    Ok(vec![
        early_auth,
        sync,
        verification,
        ticket,
        login::server_id_packet(),
    ])
}

