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
use rove_client_core::transport::session::{ReliableReceiver, ReliableSender};
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
        // The channel is not known here; `feed_on` records it.
        self.note_arrival(REPLICATION_APPLICATION, REPLICATION_CHANNEL, payload);
        self.feed_inner(payload)
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
            // The world: schema, globals, and replication.
            (REPLICATION_APPLICATION, REPLICATION_CHANNEL, 129 | 131 | 151) => {
                self.feed_inner(payload)
            }
            // Instance state (161) rides application 4 channel 5, and is the
            // packet most of a real place's world arrives in.
            (REPLICATION_APPLICATION, INSTANCE_STATE_CHANNEL, 161) => self.feed_instance_state(payload),
            // Replication on channel 3 is the world *after* globals; before
            // them the same kind is a ping (upstream's `if scope.is_none()`).
            (REPLICATION_APPLICATION, PING_CHANNEL, 131) => {
                if self.core.scope().is_none() {
                    self.ping_requested(payload)
                } else {
                    self.feed_inner(payload)
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

    fn feed_inner(&mut self, payload: &[u8]) -> bool {
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
        // This client implements no optional features; offering none is what
        // rove's allocation-only path does, and the server intersects it.
        capabilities: 0,
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
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let (packets_tx, mut packets_rx) = tokio::sync::mpsc::channel::<(u8, u32, Vec<u8>)>(READER_QUEUE);
    let mut readers = tokio::task::JoinSet::new();
    let mut total = 0usize;
    let mut fed = 0usize;
    // Why the loop stopped; `None` means the peer finished every stream.
    let end: Option<&'static str>;
    let mut accepting = Box::pin(ReliableReceiver::accept(&connection));
    // The deadline as a future, not a duration recomputed per turn: a
    // `sleep(remaining)` arm would be ready immediately on every loop and
    // would compete with arriving packets for turns.
    let deadline_at = tokio::time::Instant::from_std(deadline);
    let mut sleep_until_deadline = Box::pin(tokio::time::sleep_until(deadline_at));
    // A dropped connection ends the join now, not at the deadline: before this
    // arm existed, a server that hung up was indistinguishable from one that
    // had gone quiet, and both were reported as the deadline passing.
    let mut connection_closed = Box::pin(connection.closed());
    loop {
        if std::time::Instant::now() >= deadline {
            end = Some("the session's deadline passed");
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
                total += 1;
                if total > MAX_STREAMS {
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
        "session: {} stream(s) accepted, {} packet(s) fed, {} refusal(s), {} other stream(s)",
        total,
        session.ingested.packets,
        session.ingested.refused,
        session.other_streams
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
    let sync = login::protocol_sync(&issued.join_fields, issued.join_ticket_value.as_ref(), None)
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

