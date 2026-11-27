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
}

impl Session {
    /// A session over an empty place. The document is a bare DataModel, which
    /// is what a join's globals packet fills with the place's roots.
    pub fn new() -> Self {
        let mut core = SessionCore::with_limits(4096, 8 << 20).expect("a session with sane limits");
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
        }
    }

    pub fn document(&self) -> &Document {
        &self.document
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
    fn drain(&mut self) {
        let rows = match self.core.read_after(self.reader, self.cursor, 64) {
            Ok(rows) => rows,
            Err(error) => {
                log::warn!("session: journal read failed: {error}");
                return;
            }
        };
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
    }

    /// Rows the journal holds for this session, for the diagnostics overlay.
    pub fn journal_rows(&self) -> usize {
        self.core
            .read_after(self.reader, 0, 4096)
            .map(|rows| rows.len())
            .unwrap_or(0)
    }

    pub fn journal_records(&self) -> usize {
        self.core.journal_stats().records
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

/// Run one live join against the issued allocation, feeding `session` until the
/// peer finishes or the deadline passes.
///
/// The order here is the protocol, and the log says which step was reached:
/// connect, open the control channels, write the five login packets, then read.
#[cfg(feature = "transport")]
pub async fn run(
    issued: &IssuedJoinAllocation,
    session: &mut Session,
    seconds: u64,
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

    let mut receiver = ReliableReceiver::accept(&connection)
        .await
        .map_err(|_| "the server opened no reliable stream")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            session.stage = Stage::Closed("the session's deadline passed");
            break;
        }
        match tokio::time::timeout(remaining, receiver.next()).await {
            Ok(Ok(Some(payload))) => {
                session.feed(&payload);
            }
            Ok(Ok(None)) => {
                session.stage = Stage::Closed("the server finished the stream");
                break;
            }
            Ok(Err(_)) => {
                session.stage = Stage::Closed("a stream read failed");
                break;
            }
            Err(_) => {
                session.stage = Stage::Closed("the session's deadline passed");
                break;
            }
        }
    }
    connection.close(0u32.into(), b"done");
    endpoint.wait_idle().await;
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

