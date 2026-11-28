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
use rove_client_core::transport::{connect_webtransport, Connection, IssuedAllocation};

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

/// The application and channel the clock's own datagrams ride, as this client
/// announces them (`(6, 2)` on wire 2 and `(6, 3)` on wire 4 upstream; the
/// time-sync replies answer on `6/3`).
pub const CLOCK_APPLICATION: u8 = 6;
pub const CLOCK_CHANNEL: u32 = 3;

/// How many datagram channels the server may announce. rove's own cap
/// (`MAX_CHANNELS`); a place opens a handful — the clock's two, physics, ISR —
/// and a server that announces dozens is one this client should not track
/// without bound.
pub const MAX_DATAGRAM_CHANNELS: usize = 16;

/// The radius a streaming server is asked to keep loaded, and how long the
/// focus stands before it lapses. rove's own client sends exactly these
/// (`live_session.rs:762`: `512.0`, `4000`) — generous on purpose: without a
/// focus the streamer has no idea what to send, so the spawn region never
/// materializes and the place is a character in an empty sky.
pub const FOCUS_RADIUS_STUDS: f32 = 512.0;
pub const FOCUS_TIMEOUT_MS: u32 = 4000;

/// How far the focus must move before the server is asked again.
pub const FOCUS_MOVE_STUDS: f32 = 64.0;

/// How long a focus stands before it is re-sent: comfortably inside
/// [`FOCUS_TIMEOUT_MS`], so the region never lapses between refreshes.
pub const FOCUS_REFRESH: std::time::Duration = std::time::Duration::from_secs(1);

/// How many journal rows one read takes. The journal is read in batches and
/// drained fully, so this is a page size and not a limit.
const DRAIN_BATCH: usize = 64;

/// How long one reliable write may take before the loop stops waiting on it.
///
/// Every reliable write in this loop goes through `ReliableSender`, whose
/// stream map is held across the `await` — so a write that never returns does
/// not just delay its own reply, it stops the `select!`. The tick stops with it,
/// and with the tick the counters, the flush and `progress`: the phone's report
/// was an overlay frozen at its last value over a session that had *not* closed.
///
/// Five seconds is far longer than a healthy write (which is microseconds on a
/// connection that is being read) and far shorter than a join is allowed to sit
/// there. What a timeout means is counted and said out loud, not guessed at.
#[cfg(feature = "transport")]
const WRITE_STALL_MS: u64 = 5_000;

/// **How long a live session may go without a packet before it says so.**
///
/// A world arrives in bursts and a streamed place goes quiet between them, so
/// this is not a failure threshold — nothing is closed at it. What it is: the
/// line that names a stall while it is happening, with the numbers that tell a
/// blocked loop from a silent server. Three seconds is long enough that a
/// healthy join never prints it (the initial patch is continuous) and short
/// enough that it appears inside the freeze a person is looking at.
#[cfg(feature = "transport")]
const STALL_REPORT_MS: u64 = 3_000;

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

/// A packet a session owes the server, and what kind it is.
///
/// The kind is not decoration: a property acknowledgment is the one reply whose
/// *delivery* has to be reported back to its queue, and it is recognised by
/// being what it is rather than by its first byte. It could not be the byte:
/// `0x83` is also how `ReplicationFocus` (`[131, 26]`) and the streaming resume
/// (`[131, 3]`) begin, so a focus packet sent while a receipt was in flight
/// would have committed that receipt as delivered — a receipt the server then
/// never resends and cannot know was lost.
#[derive(Debug, Clone)]
pub enum Outbound {
    /// A marker reply, a ping reply, a focus, the streaming resume.
    Reply(u8, u32, Vec<u8>),
    /// A `CFrame` receipt acknowledgment: `[0x83, items…, 0]` on `(4, 1)`.
    PropertyAck(u8, u32, Vec<u8>),
}

impl Outbound {
    pub fn application(&self) -> u8 {
        match self {
            Outbound::Reply(application, _, _) | Outbound::PropertyAck(application, _, _) => {
                *application
            }
        }
    }
    pub fn channel(&self) -> u32 {
        match self {
            Outbound::Reply(_, channel, _) | Outbound::PropertyAck(_, channel, _) => *channel,
        }
    }
    pub fn payload(&self) -> &[u8] {
        match self {
            Outbound::Reply(_, _, payload) | Outbound::PropertyAck(_, _, payload) => payload,
        }
    }
    fn is_property_ack(&self) -> bool {
        matches!(self, Outbound::PropertyAck(..))
    }
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
    /// The last applied journal sequence: the read cursor, and what every
    /// acknowledgment names after its batch has been applied.
    pub cursor: u64,
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
    /// Marker requests that could not be answered (an unreadable one).
    pub marker_refused: usize,
    /// Marker replies held until the client has a scope **and** the initial
    /// world tag. rove's own queue, ported rather than reimplemented: the order
    /// and the gate are both part of the handshake.
    markers_queue: rove_client_core::replication_out::MarkerQueue,
    /// Marker replies actually written, as opposed to queued.
    pub marker_replies: usize,
    /// The highest journal sequence acknowledged, and how many acknowledgments
    /// failed. The journal prunes behind the acknowledgment, so a session whose
    /// `acknowledged_through` is stuck at 0 is one the journal will fill.
    /// When this session was created. The overlay's `seconds` is read from it:
    /// a join is measured against wall time, not against packets.
    pub started: std::time::Instant,
    pub acknowledged_through: u64,
    /// Rows applied over the session's life. Distinct from the journal's
    /// retained count, which prunes to zero as the session keeps up.
    pub rows_applied: u64,
    pub acknowledged_errors: usize,
    /// A region has arrived, so this place streams (`note_regions`). Until then
    /// no focus is sent: the streamer does not exist to read one. See `refocus`.
    pub streamer_seen: bool,
    /// When a packet last arrived (reliable or datagram).
    last_packet_at: Option<std::time::Instant>,
    /// Times the physics uplink had no pose to send (see `PhysicsWriter::tick`):
    /// `physics_frames_sent == 0, physics_skipped > 0` is a client that is
    /// running and has nothing to say about its own character.
    pub physics_skipped: u64,
    /// Heartbeats the loop actually served. A session whose `stall_ticks` stops
    /// rising while a stall is reported is a loop blocked inside a write.
    pub stall_ticks: u64,
    /// Stall lines printed, so a test can assert the report happened and the
    /// end-of-session summary can say whether the session ever went quiet.
    pub stalls_reported: u64,
    /// Reliable writes that took longer than [`WRITE_STALL_MS`] and were given
    /// up on. A non-zero value is the one explanation for a session whose
    /// counters froze: every write in this loop goes through one sender whose
    /// stream lock is held across the await, so a peer that stops reading
    /// stalls the loop itself — and a stalled loop is the tick, the counters and
    /// the flush together.
    pub writes_stalled: usize,
    /// Sends that failed outright (a closed or reset stream).
    pub writes_refused: usize,
    /// Streaming: the regions the server has sent, where we last asked it to
    /// focus, whether the spawn acknowledgment went, and whether our own Player
    /// has appeared yet. A streaming place sends half a world until the client
    /// acknowledges the spawn region, so these four numbers say whether that
    /// handshake happened at all.
    pub regions: usize,
    pub focus_id: u32,
    pub focus_sent: usize,
    pub streaming_ready_sent: bool,
    pub streaming_resume_refused: usize,
    /// Our account's user id, and whether a character of ours has resolved.
    pub account_user_id: Option<u64>,
    pub character_seen: bool,
    /// The ids of every `Player` in the world, for finding ours.
    pub player_ids: std::collections::BTreeSet<String>,
    /// Ping packets that carried no request (a pure reply packet) and those
    /// whose request this client cannot answer truthfully. Both counted so a
    /// session with pings and no replies names the reason.
    pub ping_unanswered: usize,
    pub ping_refused: usize,
    /// The integrity status word a ping reply carries. Zero until the session
    /// has something to say about its own integrity, which is honest: rove's
    /// responder refuses to invent it, and so does this.
    pub integrity_status: u32,
    /// Replies the session wrote back (markers, handshakes) and those the
    /// transport refused. A session with a rising `replies_failed` is one the
    /// server will throttle.
    pub replies_sent: usize,
    pub replies_failed: usize,
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
    /// Packets this session must send back on `(application, channel)`.
    ///
    /// A join is not receive-only: the server asks the client things and waits
    /// for the answer before it sends the next thing. The marker handshake
    /// (`132` → `131, 4`) is answered here rather than in the network loop so
    /// the decision lives next to the packet that caused it, and the loop only
    /// writes what it finds.
    pub outbound: Vec<Outbound>,
    /// **Unreliable channels, as the server announces them.**
    ///
    /// A datagram names only a `wire_id`; which application and channel it
    /// belongs to arrives once, on the reliable `CONTROL_CHANNEL` stream, as an
    /// announcement (`parse_control_open`). Without this map every datagram but
    /// the clock's was thrown away — and a live server sends the *interesting*
    /// packets there: replication over unreliable transport, pings, instance
    /// state. The phone's log is what that looks like: 40 seconds of a world
    /// arriving with every part frozen, because every datagram was dropped.
    pub channels: std::collections::BTreeMap<u32, (u8, u32)>,
    /// Datagrams that named a `wire_id` no announcement had described.
    pub unknown_datagrams: u64,
    /// Datagrams fed through [`Session::feed_on`] (everything but the clock's).
    pub datagrams_fed: u64,
    /// Fragments that arrived incomplete (the transport splits large
    /// datagrams; an incomplete one is not a packet).
    pub fragmented_datagrams: u64,
    /// Datagrams the transport could not parse at all.
    pub unreadable_datagrams: u64,
    /// Signed-program challenges (`155`) the server sent, and how many this
    /// client could answer. Zero answered: there is no signature trust store
    /// in this build, and a fabricated reply would be rejected — but the
    /// number is what names the reason a server is holding state back.
    pub program_challenges: u64,
    pub program_answered: u64,
    /// **The world view before the receipt about to be drained.**
    ///
    /// `RuntimeDeltaNormalizer` refuses a record whose first action sequence is
    /// at or below the view's sequence — it needs the world as it stood *before*
    /// the delta, to know a `set`'s class and to prove a create is unambiguous.
    /// The committed world is read *after* the packet, so this is captured at
    /// ingest and consumed by the next drain.
    pre_view: Option<rove_client_core::world::WorldView>,
    /// rove's per-record normaliser. Built once the schema is installed: it
    /// holds the class → property → type index that turns a raw wire value
    /// (`ReplicationPV` type 57, `CFrame` type 27/51 without its tag, a
    /// lowercase `size` type 22) into the one the bridge can convert. Without
    /// it a live `Workspace` has every instance and **no `Part.CFrame` and no
    /// `Part.Size`** — the map exists and cannot be drawn or stood on.
    normalizer: Option<rove_client_core::runtime_delta::RuntimeDeltaNormalizer>,
    /// The world tag `13`: rove's "the initial world has arrived". Some packets
    /// (markers, streaming) must not be answered before it.
    initial_tag: bool,
    /// Records the normaliser refused, counted rather than swallowed: a refusal
    /// here means the bridge read that record's raw wire shape.
    normalize_refused: u64,
    /// The `155` handler for this allocation: one per session, with its own
    /// bounded duplicate cache and its own job id. Built on the first challenge.
    program_auth: Option<crate::program_check::ProgramAuth>,
    /// The allocation's `game.JobId`, which the challenge program reads.
    game_id: Option<String>,
    /// A test's scope, when there is no globals packet to commit one. Never set
    /// in a real session: `core.scope()` is the only honest answer there.
    marker_gate_open_for_test: bool,
    /// Challenges refused (a bad container, a failed signature, a VM that would
    /// not run it). Named separately from answered, because a server that holds
    /// a client back names no reason of its own.
    pub program_refused: u64,
    /// **Where this client is**, as `ReplicationFocus` asks the server to
    /// stream around: the last position fed in from the renderer (its camera
    /// or character). `None` until the first frame reports one.
    pub focus_position: Option<[f32; 3]>,
    /// The last focus position a packet was sent for, so an idle client does
    /// not ask the server to re-stream a region it already has every tick.
    pub focus_last_sent: Option<[f32; 3]>,
    /// When that packet went, for [`Session::refocus`].
    pub focus_at: Option<std::time::Instant>,
    /// `CFrame` acknowledgments (`0x83` on `(4, 1)`). The server holds a
    /// property's next version until the client confirms the one it sent, so a
    /// client that never acknowledges is a client the world stops moving for.
    property_acks: rove_client_core::property_acknowledgments::PropertyAcknowledgments,
    /// Property acknowledgments the transport accepted.
    pub property_acks_sent: u64,
    /// The property-acknowledgment payload in flight, until the transport says
    /// whether it was written.
    ack_in_flight: Option<u64>,
    /// **The monotonic clock a ping reply's first three words are read from.**
    ///
    /// Milliseconds since this session was created, modulo `u32`. Not counters:
    /// a reply whose "received / queued / written" words are packet and instance
    /// counts makes the server's queue-delay estimate read seconds of delay for
    /// a session with none, which is why a reply has to carry real clocks.
    ping_clock: crate::ping::PingClock,
    /// The ping request queue: bounded, one write in flight, telemetry-blocked
    /// rather than filled in. Every request on `(4, 3)` — before globals from the
    /// wire decoder, after globals from the committed record — goes here.
    ping_responder: crate::ping::PingResponder,
    /// What a ping reply's integrity word may say: only checks this session ran.
    integrity: crate::session_integrity::SessionIntegrity,
    /// `0xe3` on application 4: an extended packet family whose presence says
    /// the legacy replication statistics this client does not collect are being
    /// asked for, which blocks that part of a reply instead of sending zeros.
    extended_family_seen: bool,
    /// The capability bits the server negotiated at connect (`0x10_0000` is the
    /// QUIC profile `rove` reports statistics-zero on).
    negotiated: u64,
    /// Replies the responder would not prepare (a missing observation, an
    /// unsupported request). Counted so "requests with no replies" has a reason
    /// next to it on the overlay.
    pub ping_prepare_failures: u64,
    /// Character-root physics records written and refused. A session whose world
    /// renders and whose character is frozen on the server has a zero here, and
    /// that is the number that says why the two disagree.
    pub physics_frames_sent: u64,
    pub physics_frames_failed: u64,
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
            marker_refused: 0,
            markers_queue: rove_client_core::replication_out::MarkerQueue::new(),
            marker_replies: 0,
            started: std::time::Instant::now(),
            rows_applied: 0,
            streamer_seen: false,
            last_packet_at: None,
            physics_skipped: 0,
            stall_ticks: 0,
            stalls_reported: 0,
            acknowledged_through: 0,
            acknowledged_errors: 0,
            writes_stalled: 0,
            writes_refused: 0,
            regions: 0,
            focus_id: 0,
            focus_sent: 0,
            streaming_ready_sent: false,
            streaming_resume_refused: 0,
            account_user_id: None,
            character_seen: false,
            player_ids: std::collections::BTreeSet::new(),
            ping_unanswered: 0,
            ping_refused: 0,
            integrity_status: 0,
            replies_sent: 0,
            replies_failed: 0,
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
            outbound: Vec::new(),
            channels: std::collections::BTreeMap::new(),
            unknown_datagrams: 0,
            datagrams_fed: 0,
            fragmented_datagrams: 0,
            unreadable_datagrams: 0,
            program_challenges: 0,
            program_answered: 0,
            pre_view: None,
            normalizer: None,
            initial_tag: false,
            normalize_refused: 0,
            program_auth: None,
            game_id: None,
            marker_gate_open_for_test: false,
            program_refused: 0,
            focus_position: None,
            focus_last_sent: None,
            focus_at: None,
            property_acks: rove_client_core::property_acknowledgments::PropertyAcknowledgments::new(),
            property_acks_sent: 0,
            ack_in_flight: None,
            ping_clock: crate::ping::PingClock::new(std::time::Instant::now()),
            ping_responder: crate::ping::PingResponder::new(64)
                .expect("a ping queue bound of 64 is inside the protocol's 1..=128"),
            integrity: crate::session_integrity::SessionIntegrity::new(),
            extended_family_seen: false,
            negotiated: 0,
            ping_prepare_failures: 0,
            physics_frames_sent: 0,
            physics_frames_failed: 0,
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

    /// Feed a packet and report how many handshake replies it left behind.
    ///
    /// The network loop writes `outbound` after every packet and does not care
    /// which packet asked; a test does, and this is the count it can assert
    /// without reaching into the queue.
    #[doc(hidden)]
    pub fn outbound_len(&self) -> usize {
        self.outbound.len()
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
        let fed = self.dispatch(application, channel, payload);
        // After the packet, not inside it: see `settle`.
        self.settle();
        fed
    }

    /// The dispatch itself, without the handshakes a packet can unblock.
    fn dispatch(&mut self, application: u8, channel: u32, payload: &[u8]) -> bool {
        self.note_arrival(application, channel, payload);
        // **A channel announcement**, before anything else looks at the packet:
        // it is how a datagram's bare `wire_id` becomes `(application,
        // channel)`, and it travels on the reliable control channel rather than
        // on the world's. (`parse_control_open` is rove's own reader: a
        // ten-byte `2, app, channel u32 be, wire u32 be`.)
        if channel == rove_client_core::channels::CONTROL_CHANNEL {
            // The one `settle` for every outcome is at the end of this function:
            // calling it here as well drained the marker queue twice for a
            // control packet and made "markers sent" double-count.
            return self.control_channel(application, payload);
        }
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
            // **A signed-program challenge (`155`).** The server asks the
            // client to run a program it signed and answer with the result; a
            // client that ignores it is one the server does not trust, and it
            // answers by holding the character and the map back. rove's
            // `ProgramAuth` does the verification and the reply
            // (`program_check.rs`), including a duplicate cache, and this
            // session has no signature trust store yet — so the packet is
            // *named* rather than silently counted as unhandled, and the
            // report says how many went unanswered.
            (REPLICATION_APPLICATION, REPLICATION_CHANNEL, 155) => {
                self.program_challenges += 1;
                match self.program_challenge(payload) {
                    Some(reply) => {
                        self.program_answered += 1;
                        self.outbound
                            .push(Outbound::Reply(application, channel, reply));
                        true
                    }
                    None => false,
                }
            }
            // The server's feature-name/value table: the one packet that says
            // which encoding the server will use for the session.
            (REPLICATION_APPLICATION, REPLICATION_CHANNEL, 147) => self.features_offered(payload),
            // A marker round-trip. The server sends `132` and waits for the
            // reply before it streams: rove's client answers with
            // `replication_out::marker_reply` (kind 131, sub 4, the marker
            // echoed, a zero trailer), and a client that only *counted* the
            // request left the handshake open — the world arrived slowly or not
            // at all. Answered here, where the request is understood.
            //
            // **Queued, not answered yet.** A marker reply sent before the
            // client has its scope and the initial world tag is a reply about a
            // world that does not exist yet: rove's own rule
            // (`MarkerQueue::drain`) is to hold every marker until both are
            // true, and to return them in receive order. Answering one early is
            // worse than answering late — the server takes the marker as "this
            // client has the world" and starts streaming into a session that
            // cannot place what arrives.
            (REPLICATION_APPLICATION, REPLICATION_CHANNEL, 132) => {
                self.markers += 1;
                match self.markers_queue.add(payload) {
                    Ok(()) => true,
                    Err(error) => {
                        // A marker we cannot read is the server's, not a reason
                        // to drop the session; counted so the report says so.
                        self.marker_refused += 1;
                        log::warn!("session: marker request refused: {error}");
                        false
                    }
                }
            }
            _ => {
                self.unhandled += 1;
                false
            }
        }
    }

    /// Finish the packet: release any marker the gate now allows, and answer any
    /// ping a committed record carried.
    ///
    /// Both happen here rather than inside the arms because both depend on what
    /// this packet changed: a marker's gate is opened by a *record* in an earlier
    /// packet, and a ping is a record's own field. Calling this after the packet
    /// has been fed is what makes the order right in one place instead of three.
    fn settle(&mut self) {
        self.drain_markers();
    }

    /// A packet on the reliable control channel: a datagram channel's
    /// announcement, a close, or the empty open the client itself sends.
    ///
    /// The announcement is the whole point: `(2, application, channel, wire)`
    /// is the only thing that says what a datagram's `wire_id` means, and a
    /// client that does not keep the map reads every datagram but the clock's
    /// as unknown. rove's own session does exactly this
    /// (`live_session.rs:2117`), with a cap and a re-assignment check; both are
    /// kept, because a server that reassigns a wire id mid-session is telling
    /// this client something it must not silently accept.
    fn control_channel(&mut self, application: u8, payload: &[u8]) -> bool {
        match payload.first().copied() {
            // An announcement: `2, app, channel u32, wire u32`.
            Some(2) => match rove_client_core::channels::parse_control_open(payload) {
                Ok((app, channel, wire)) => {
                    if self.channels.len() >= MAX_DATAGRAM_CHANNELS
                        && !self.channels.contains_key(&wire)
                    {
                        log::warn!(
                            "session: the server announced more than {MAX_DATAGRAM_CHANNELS} \
                             datagram channels; {wire} is not tracked"
                        );
                        return false;
                    }
                    let wanted = (app, u32::from(channel));
                    if self
                        .channels
                        .get(&wire)
                        .is_some_and(|existing| *existing != wanted)
                    {
                        log::warn!(
                            "session: wire id {wire} was re-announced as {app}/{channel}, \
                             it was {:?}",
                            self.channels.get(&wire)
                        );
                        return false;
                    }
                    self.channels.insert(wire, wanted);
                    log::debug!("session: datagram channel {app}/{channel} is wire {wire}");
                    true
                }
                Err(error) => {
                    log::warn!("session: a control-channel announcement was refused: {error}");
                    false
                }
            },
            // The empty open (`[0, 0]`) a client sends, or anything else: not
            // world state either way.
            _ => {
                self.unhandled += 1;
                let _ = application;
                false
            }
        }
    }

    /// **One unreliable datagram, routed.** The clock's channel goes to the
    /// time-sync probe; everything else is a game packet that arrived over
    /// datagrams instead of a stream, and goes through the same
    /// [`Session::feed_on`] dispatch a reliable one takes.
    ///
    /// Upstream does this per datagram (`live_session.rs:2138`: look the wire id
    /// up, then `(6, 3)` to the clock and everything else into the world). This
    /// join used to keep only the clock's and drop the rest, which is why a
    /// live world arrived with its instances and none of its motion.
    ///
    /// Returns whether the packet was world state this session accepted.
    #[cfg(feature = "transport")]
    pub fn datagram(
        &mut self,
        wire_id: u32,
        payload: &[u8],
        clock: &SessionWireClock,
        sync: &mut TimeSyncProbe,
    ) -> bool {
        self.last_packet_at = Some(std::time::Instant::now());
        let Some(&(application, channel)) = self.channels.get(&wire_id) else {
            self.unknown_datagrams += 1;
            if self.unknown_datagrams == 1 {
                // Said once, with the wire id: the first unannounced datagram is
                // the answer to "is the server sending at all", and a repeated
                // line would be 40 seconds of the same sentence.
                log::warn!(
                    "session: a datagram named wire id {wire_id}, which no control-channel \
                     announcement described; it cannot be routed"
                );
            }
            return false;
        };
        // The clock's own two datagram channels (6/2 and 6/3 upstream): the
        // probe reads them, and they are not world state.
        if application == CLOCK_APPLICATION && channel == CLOCK_CHANNEL {
            match sync.receive(payload, clock, true) {
                Ok(true) => self.sync_replies += 1,
                Ok(false) => {}
                Err(error) => log::warn!("session: time-sync reply refused: {error}"),
            }
            return false;
        }
        self.datagrams_fed += 1;
        self.feed_on(application, channel, payload)
    }

    /// **Ask the streaming server to keep the region around us.**
    ///
    /// Called on the same tick as the clock probe, not by the renderer: the join
    /// is a thread of its own and the editor's camera is on the other side of
    /// the handover. The position reused is the last one sent — a focus names a
    /// *region*, and the server keeps it for `FOCUS_TIMEOUT_MS` — so an idle
    /// character's region does not lapse and stop streaming.
    ///
    /// Returns whether a packet was queued (the caller flushes).
    ///
    /// Not behind `transport`: nothing here touches a socket. Its one caller
    /// sits in the receive loop, and the rule it holds — *no region, no focus* —
    /// is exactly the kind of thing that has to be checkable without one.
    pub fn refocus(&mut self, from_editor: Option<[f32; 3]>) -> bool {
        // Where the view is, if the editor has said: the camera moves with the
        // character, so this is the region the player is actually in. Before the
        // first frame that says, the spawn region's own focus (`note_regions`)
        // is the only one that can be honest — and it has already been sent.
        // The editor asks every frame (`LiveWorld::focus`); on a place with no
        // streamer there is nobody to ask, and `request_focus` is where that is
        // decided.
        if let Some(position) = from_editor {
            return self.request_focus(position, false);
        }
        // Otherwise the timeout decides: a focus lapses (`FOCUS_TIMEOUT_MS`),
        // and the last one is re-sent well inside that, so the region never
        // stops streaming while the character stands still.
        let Some(sent_at) = self.focus_at else {
            return false;
        };
        if sent_at.elapsed() < FOCUS_REFRESH {
            return false;
        }
        let Some(position) = self.focus_position else {
            return false;
        };
        self.request_focus(position, true)
    }

    /// Whether a wire id is the clock's `(6, 3)`, by the map and nothing else.
    ///
    /// Deliberately *not* behind `transport`: it is a map lookup, it is what
    /// decides which channel's bytes a reader sees, and a test that cannot ask
    /// the question without a socket is a test that does not ask it. (The
    /// `datagram` that uses it needs the probe and the clock, and stays gated.)
    pub fn datagram_is_clock(&self, wire_id: u32) -> bool {
        self.channels
            .get(&wire_id)
            .is_some_and(|&(application, channel)| {
                application == CLOCK_APPLICATION && channel == CLOCK_CHANNEL
            })
    }

    /// **Where we are, for the server's streamer.** A streaming place sends the
    /// parts around the client's `ReplicationFocus` and nothing else, so a
    /// client that never sends one is a client that gets the spawn region and
    /// then silence — which is a spawn, a skybox and no map.
    ///
    /// Sent when the position has moved meaningfully since the last one, not
    /// every frame: a focus packet names a radius, and re-sending the same
    /// region is asking for parts that already arrived. `force` sends once
    /// regardless, for the first frame after the character binds.
    pub fn request_focus(&mut self, position: [f32; 3], force: bool) -> bool {
        // **No region has arrived, so there is no streamer to answer.** A place
        // that streams sends its spawn region before any of its parts; a place
        // that does not never sends one, and a focus for it is a packet about a
        // streamer that does not exist there. The gate is here, in the one
        // method that queues a focus, so no caller can get round it — the
        // editor's per-frame position, the spawn region and the keep-alive all
        // come through this door. (`note_regions` sets `streamer_seen` *before*
        // it calls this for the region it just read.)
        if !self.streamer_seen {
            self.focus_position = Some(position);
            return false;
        }
        self.focus_position = Some(position);
        let moved = match self.focus_last_sent {
            None => true,
            Some(last) => {
                let d = |a: usize| position[a] - last[a];
                d(0) * d(0) + d(1) * d(1) + d(2) * d(2) >= FOCUS_MOVE_STUDS * FOCUS_MOVE_STUDS
            }
        };
        if !moved && !force {
            return false;
        }
        self.focus_id = self.focus_id.wrapping_add(1);
        if let Ok(packet) = rove_client_core::replication_out::replication_focus(
            self.focus_id,
            position,
            FOCUS_RADIUS_STUDS,
            FOCUS_TIMEOUT_MS,
        ) {
            self.outbound
                .push(Outbound::Reply(REPLICATION_APPLICATION, REPLICATION_CHANNEL, packet));
            self.focus_sent += 1;
            self.focus_last_sent = Some(position);
            self.focus_at = Some(std::time::Instant::now());
            true
        } else {
            false
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
    /// Tell a streaming server we have the spawn region, so it will send the
    /// character and the parts around it.
    ///
    /// This is rove's `prepare_streaming_resume`, and it exists because of a
    /// chicken-and-egg a streaming place creates: the server holds the
    /// character until the client acknowledges the spawn region, and a client
    /// waiting for the character before acknowledging waits forever. Half a
    /// DataModel — Models with no parts in them, no character — is what that
    /// deadlock looks like from the outside.
    ///
    /// What "we have it" means is evidence, not a timer: a region received, or
    /// a `Player` of ours in the world, or a resolved character. rove's own
    /// comment records that some servers never publish regions before the ack,
    /// so the character resolves first there — and at least one of the three
    /// must hold before anything is sent.
    ///
    /// Sent once per session on `(4, 1)`, the reliable world stream.
    fn prepare_streaming_resume(&mut self) -> bool {
        if self.streaming_ready_sent {
            return false;
        }
        let Some(view) = self.core.world_view() else {
            return false;
        };
        // Our own Player: the one whose `UserId` is the account's.
        let me = self.local_player_in(&view);
        // The evidence: a Player of ours exists, or a character has resolved.
        let character = self.character_seen;
        let Some(player) = me else {
            // No Player of ours yet. Not a failure — this is asked again after
            // the next packet — but a character with no Player is not evidence
            // enough to spend the one acknowledgment on.
            return false;
        };
        if !character && self.regions == 0 {
            // Nothing proves the region arrived, and a `131, 3` sent on a guess
            // tells the server we have a world we do not have.
            return false;
        }
        let Some((property, property_type)) = self.gameplay_paused() else {
            return false;
        };
        let Ok(guid) = parse_instance_guid(player) else {
            return false;
        };
        let evidence = rove_client_core::replication_out::StreamingEvidence {
            player_spawned: character,
            spawn_region_received: true,
        };
        match rove_client_core::replication_out::streaming_ready(
            guid,
            property,
            property_type,
            evidence,
        ) {
            Ok(payload) => {
                self.outbound
                    .push(Outbound::Reply(REPLICATION_APPLICATION, REPLICATION_CHANNEL, payload));
                self.streaming_ready_sent = true;
                true
            }
            Err(error) => {
                // Counted, not fatal: the next packet asks again.
                log::debug!("session: streaming resume not encoded: {error}");
                false
            }
        }
    }

    /// **Our own** `Player`, the one whose `UserId` is the account's.
    ///
    /// Ours specifically: a server sends every player in the place, and a
    /// session that names the first `Player` it sees as "me" spawns, focuses
    /// and simulates somebody else's character. `None` until the account id is
    /// known and a `Player` carries it.
    fn local_player_in(&self, view: &rove_client_core::world::WorldView) -> Option<&String> {
        let uid = self.account_user_id?;
        self.player_ids.iter().find(|id| {
            view.class_name(id) == Some("Player")
                && view
                    .get_property(id, "UserId")
                    .and_then(|v| v.as_f64())
                    .is_some_and(|v| v as u64 == uid)
        })
    }

    /// The same lookup against the committed world.
    fn local_player(&self) -> Option<String> {
        let view = self.core.world_view()?;
        self.local_player_in(&view).cloned()
    }

    /// Whether **our** Player has a `Character` that is in the world.
    ///
    /// Any `Player` with a character is not the same question: the server sends
    /// the whole `Players` service, so every other player in the place has one
    /// long before ours exists. Answering with "somebody has a character" is
    /// how a session decides it has spawned, tells the server it has the spawn
    /// region, and then stands at the origin looking at its own overlay.
    fn character_present(&self) -> bool {
        let Some(me) = self.local_player() else {
            return false;
        };
        self.live
            .referent(&me)
            .is_some_and(|r| match self.document.prop(r, "Character") {
                // `rustudio-core` re-exports the DOM crate as `__rbx`, which
                // is how a crate that only holds a `Document` names its
                // values without depending on `rbx_dom_weak` itself.
                Some(rustudio_core::__rbx::types::Variant::Ref(c)) => self.document.exists(c),
                _ => false,
            })
    }

    /// `Player.GameplayPaused`'s schema id **and type**, which the resume packet
    /// names.
    ///
    /// Both, from the schema, rather than an id from one lookup and a hardcoded
    /// `9` beside it: `streaming_ready` refuses a type that is not the boolean
    /// the server expects, so a schema that spelled it differently would fail
    /// silently with a number nobody could check. Read from one place and the
    /// mismatch becomes this session's own refusal.
    fn gameplay_paused(&self) -> Option<(u16, u8)> {
        let schema = self.core.schema()?;
        let class = schema.classes.iter().find(|c| c.name == "Player")?;
        let property = schema
            .properties
            .iter()
            .find(|p| p.name == "GameplayPaused" && class.properties.contains(&p.id))?;
        Some((u16::try_from(property.id).ok()?, property.type_id))
    }

    /// Where to ask the streamer to focus: the last region the server sent.
    ///
    /// Region coordinates arrive as 64-stud cell indices; a focus point is world
    /// studs, so the cell centre is what to ask for — rove's own conversion.
    /// Sent on `(4, 1)`, cycling the id the server echoes back.
    /// `note_regions` for a test: the region arithmetic and the focus gate are
    /// what the tests in `tests/focus_only_when_streaming.rs` are about, and
    /// driving them through a whole recorded stream would test the recording
    /// instead. The body is identical — this is not a second path.
    pub fn note_regions_for_test(&mut self, regions: &[serde_json::Value]) {
        self.note_regions(regions);
    }

    fn note_regions(&mut self, regions: &[serde_json::Value]) {
        self.regions += regions.len();
        let Some(center) = regions
            .last()
            .and_then(|region| region.get("coordinate"))
            .and_then(|c| c.as_array())
            .filter(|values| values.len() == 3)
        else {
            return;
        };
        let to_stud = |index: usize| {
            (center[index].as_f64().unwrap_or(0.0) * 64.0 + 32.0) as f32
        };
        // **The region centre in studs, sent as a focus** — and only once a
        // region has actually arrived. `note_regions` is the only place a
        // region is seen, so the focus this sends is the answer to a stream the
        // server has already started: a place that is *not* streaming sends no
        // regions, and this then sends nothing, which is what a non-streaming
        // place expects (a focus for it is a packet about a streamer that does
        // not exist there). The keep-alive in `refocus` is deliberately *not*
        // gated this way: a focus lapses (`FOCUS_TIMEOUT_MS`) and the region
        // must not stop streaming while the character stands still.
        self.streamer_seen = true;
        self.request_focus(
            [to_stud(0), to_stud(1), to_stud(2)],
            self.focus_last_sent.is_none(),
        );
    }

    /// A ping on `(4, 3)`, **before globals**: a request on the wire, not in a
    /// committed record.
    ///
    /// Queued, not answered inline. A reply is built from observations taken at
    /// three different moments — the packet's arrival, the queue's, and the
    /// socket write's — and it is the transport that knows the last one. So this
    /// only enqueues (with the arrival clock word), and `run` flushes the queue
    /// before the next read.
    ///
    /// A request this client cannot answer truthfully is *blocked*: rove's
    /// responder refuses a flagged request, a queue overflow, or a missing
    /// observation, and a fabricated reply is worse than a missing one because
    /// the server accepts it and then trusts the numbers.
    fn ping_requested(&mut self, payload: &[u8]) -> bool {
        self.pings += 1;
        // The arrival clock word, stamped here: the one moment this session can
        // honestly call "received".
        let received = self.ping_clock.word();
        let mut telemetry = self.ping_telemetry();
        match self
            .ping_responder
            .receive_packet(payload, received, &mut telemetry)
        {
            Ok(0) => {
                // Replies, or requests the client does not support: counted by
                // the responder, and nothing to write.
                self.ping_unanswered += 1;
                false
            }
            Ok(_) => true,
            Err(error) => {
                self.ping_refused += 1;
                log::warn!("session: a ping packet was refused: {error:?}");
                false
            }
        }
    }

    /// Release the marker replies the gate now allows.
    ///
    /// Called after every packet *and* after every record: a marker arrives as
    /// its own packet and commits no record, so draining only after a record
    /// would hold it until the next one — and the gate's second fact (the
    /// initial world tag) usually becomes true in a record that arrived before
    /// the marker did. Cheap when the queue is empty, which is most packets.
    fn drain_markers(&mut self) {
        if self.markers_queue.is_empty() {
            return;
        }
        let readiness = rove_client_core::replication_out::MarkerReadiness {
            client_scope_received: self.core.scope().is_some() || self.marker_gate_open_for_test,
            initial_world_tag_received: self.initial_tag,
        };
        for reply in self.markers_queue.drain(readiness) {
            self.outbound.push(Outbound::Reply(
                REPLICATION_APPLICATION,
                REPLICATION_CHANNEL,
                reply,
            ));
            self.marker_replies += 1;
        }
    }

    /// **Post-globals pings, which live in the record.**
    ///
    /// Once globals commit, `(4, 3, 131)` is world state to the decoder: the
    /// packet is committed as a receipt and its ping items land in
    /// `SessionRecord.pings`. This is where they are read. It is the whole of
    /// gap 3A — a session that fed those packets to the decoder and never looked
    /// at `record.pings` answered nothing at all after globals, so the server
    /// stopped trusting the client's liveness a second into the join.
    fn pings_for_record(&mut self, pings: &[serde_json::Value]) -> usize {
        if pings.is_empty() {
            return 0;
        }
        self.pings += pings.len();
        let received = self.ping_clock.word();
        let mut telemetry = self.ping_telemetry();
        match self
            .ping_responder
            .receive_records(pings, received, &mut telemetry)
        {
            Ok(enqueued) => {
                if enqueued == 0 {
                    self.ping_unanswered += 1;
                }
                enqueued
            }
            Err(error) => {
                self.ping_refused += 1;
                log::warn!("session: committed ping records were refused: {error:?}");
                0
            }
        }
    }

    /// This session's observations, as the responder asks for them.
    ///
    /// The statistics provider is deliberately strict about the profile: on
    /// anything but the QUIC profile (or once an `0xe3` extended-family packet
    /// has been seen) the legacy replication statistics are *not* collected, and
    /// zero is not a reading. It blocks the reply, as upstream blocks it.
    fn ping_telemetry(&self) -> SessionPingTelemetry {
        SessionPingTelemetry {
            clock: self.ping_clock,
            supported_profile: matches!(self.negotiated, 0 | 0x10_0000),
            extended_family_seen: self.extended_family_seen,
            integrity: self.integrity.clone(),
        }
    }

    /// Whether a reply is waiting and which route it takes: the transport owner
    /// asks before each flush so an unreliable reply goes out as a datagram.
    pub fn ping_pending_route(&self) -> Option<crate::ping::DeliveryRoute> {
        self.ping_responder.front_route()
    }

    /// Prepare the next reply. Called by the transport owner immediately before
    /// the write, and only then: the write clock word is sampled inside.
    pub fn prepare_ping_reply(
        &mut self,
    ) -> Option<crate::ping::PreparedPing> {
        let mut telemetry = self.ping_telemetry();
        match self.ping_responder.prepare(&mut telemetry) {
            Ok(prepared) => prepared,
            Err(error) => {
                // Blocked or dropped, never sent with invented content. The
                // responder's diagnostics say which provider was missing; the
                // log names it so a report can be read without guessing.
                if self.ping_prepare_failures < 3 {
                    log::warn!("session: a ping reply was not prepared: {error:?}");
                }
                self.ping_prepare_failures += 1;
                None
            }
        }
    }

    /// Report an in-flight reply's write, which is what advances the queue.
    pub fn ping_reply_written(&mut self, token: u128, written: bool) {
        match self.ping_responder.commit(token, written) {
            Ok(true) => {}
            Ok(false) => {}
            Err(error) => log::warn!("session: ping write accounting failed: {error}"),
        }
    }

    /// The observations a reply is built from, at the moment of the write.
    pub fn ping_counts(&self) -> crate::ping::PingDiagnostics {
        self.ping_responder.diagnostics()
    }

    /// Replies still queued: a rising number with a flat sent count is a client
    /// whose writes are failing, which the overlay must show.
    pub fn ping_pending(&self) -> usize {
        self.ping_responder.pending()
    }

    /// The negotiated capability bits, recorded by `run` at connect.
    pub fn set_negotiated(&mut self, negotiated: u64) {
        self.negotiated = negotiated;
    }

    /// An `0xe3` packet on application 4 was seen: the session is on an extended
    /// family whose statistics this client does not collect.
    pub fn note_extended_family(&mut self) {
        self.extended_family_seen = true;
    }

    /// **A signed-program challenge (`155`), answered by running it.**
    ///
    /// The reply is `[155, second, answer]` with the answer the signed program
    /// itself returned: the trust table verifies the program, the program's own
    /// arithmetic produces the number, and nothing here knows what it will be.
    /// A challenge this session cannot verify or run is refused and counted —
    /// the server then holds back whatever it was gating, and the report names
    /// the reason rather than showing an answer that was invented.
    fn program_challenge(&mut self, payload: &[u8]) -> Option<Vec<u8>> {
        if self.program_auth.is_none() {
            // Built once, from the embedded table and this allocation's job id.
            match crate::program_check::PublicTrust::embedded() {
                Ok(trust) => {
                    match crate::program_check::ProgramAuth::for_allocation(
                        self.game_id.as_deref(),
                        trust,
                    ) {
                        Ok(mut auth) => {
                            auth.set_integrity(self.integrity.clone());
                            self.program_auth = Some(auth);
                        }
                        Err(error) => {
                            self.program_refused += 1;
                            log::warn!("session: no program challenge can be answered: {error}");
                            return None;
                        }
                    }
                }
                Err(error) => {
                    self.program_refused += 1;
                    log::warn!("session: the embedded signature table was refused: {error}");
                    return None;
                }
            }
        }
        match self.program_auth.as_mut().expect("built above").handle(payload) {
            Ok(reply) => {
                log::info!(
                    "session: answered a signed-program challenge (155) with the program's \
                     own result"
                );
                Some(reply)
            }
            Err(error) => {
                self.program_refused += 1;
                log::warn!("session: a signed-program challenge was refused: {}", error.reason());
                None
            }
        }
    }

    /// Open the marker gate without a server, for a test.
    ///
    /// The gate is two facts — a scope and the initial world tag — and a unit
    /// test has no globals packet and no tag to give it. This says the two
    /// things the gate asks about, and nothing about the world: it is
    /// `#[doc(hidden)]` and exists so the *queue* can be tested (held before,
    /// answered after) without a recording of a real join.
    #[doc(hidden)]
    pub fn open_marker_gate_for_test(&mut self) {
        self.initial_tag = true;
        self.marker_gate_open_for_test = true;
    }

    /// The programme-challenge counts, for the report: what arrived, what was
    /// answered, and what was refused with which reason.
    pub fn program_summary(&self) -> serde_json::Value {
        let mut summary = self
            .program_auth
            .as_ref()
            .map(|auth| auth.summary())
            .unwrap_or_else(|| serde_json::json!({}));
        summary["challenges_received"] = serde_json::json!(self.program_challenges);
        summary["challenges_answered"] = serde_json::json!(self.program_answered);
        summary["challenges_refused_here"] = serde_json::json!(self.program_refused);
        summary
    }

    /// A compact instance-state packet (161), which upstream reads through
    /// `ingest_instance_state` rather than `ingest`.
    fn feed_instance_state(&mut self, payload: &[u8]) -> bool {
        self.ingested.packets += 1;
        // The world as it stood before this packet: what the normaliser needs
        // to read the record the ingest is about to commit. (Captured here and
        // not in the drain because by then the commit has moved the view past
        // the record's own sequence.)
        self.pre_view = self.core.world_view();
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
        self.last_packet_at = Some(std::time::Instant::now());
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

    /// The decoder, on a packet already dispatched. The application and channel
    /// are kept in the signature because every call site names them and a reader
    /// wants to see *which* stream a packet came from at the call; the decoder
    /// itself takes only the payload.
    fn feed_inner_on(&mut self, _application: u8, _channel: u32, payload: &[u8]) -> bool {
        self.ingested.packets += 1;
        self.pre_view = self.core.world_view();
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
            // Cumulative, not retained: the journal now prunes what was read, so
            // `journal_rows()` is almost always 0. The overlay's "rows" is what
            // the session has *applied*, which is the number that tells a
            // person the world is moving.
            rows: self.rows_applied as usize,
            markers: self.markers,
            acked: self.acknowledged_through,
            regions: self.regions,
            focus_sent: self.focus_sent,
            streaming_ready: self.streaming_ready_sent,
            journal: self.journal_rows(),
            replies_sent: self.replies_sent,
            replies_failed: self.replies_failed,
            // The session's own elapsed time. It was hardcoded to 0, so every
            // overlay line read "0 s" for a session that had been joined for
            // forty seconds — the one number a person uses to tell "still
            // joining" from "stuck" was the one number that never moved.
            seconds: self.started.elapsed().as_secs(),
            // `self.core.scope()` **is this client's peer id** — rove's own
            // `Globals.scope`, committed from the globals packet, and the word
            // the server writes into `NetworkOwnerV3` when it hands an assembly
            // to this client (`GetNetworkOwner` returns this player because of
            // it). Without it the client cannot recognise its own ownership and
            // simulates nothing, which is a game where nothing moves.
            self_peer: self.core.scope().unwrap_or(0),
            datagram_channels: self.channels.len(),
            datagrams_fed: self.datagrams_fed,
            unknown_datagrams: self.unknown_datagrams,
            property_acks_sent: self.property_acks_sent,
            program_challenges: self.program_challenges,
            program_answered: self.program_answered,
            normalize_refused: self.normalize_refused,
            physics_frames_sent: self.physics_frames_sent,
            physics_frames_failed: self.physics_frames_failed,
            pings_answered: self.ping_counts().replies_sent,
            pings_pending: self.ping_pending(),
            pings_blocked: {
                let counts = self.ping_counts();
                counts.unsupported_requests
                    + counts.queue_overflows
                    + counts.telemetry_failed_requests
                    + self.ping_prepare_failures
            },
            writes_stalled: self.writes_stalled,
            writes_refused: self.writes_refused,
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
        // The highest sequence this batch got through, applied or not: the
        // journal prunes behind a reader's *acknowledgment*, and this session
        // never made one. So every record retained since the join stayed
        // retained, and at 1277 records / 8.4 MB the journal refused the next
        // packet — "journal is full while a reader needs retained data" — and
        // after that refused every packet, which is exactly what a stuck join
        // looks like from the outside: packets arriving, rows counted, the world
        // frozen at 2039 instances.
        //
        // Acknowledging `self.cursor` *after* the row it names has been applied
        // is the contract the journal documents; acknowledging before applying
        // would let it prune data this loop still has to read.
        let mut through = self.cursor;
        // The normaliser is the schema's, built once. It is the one thing
        // between rove's wire spelling and the bridge's: `ReplicationPV`
        // (type 57) copied into `CFrame`, a `CFrame` (27/51) given its
        // `"$cframe"` tag and an explicit identity rotation, a lowercase `size`
        // (22) copied into `Size`. Every one of those is a property the report
        // used to count as "written" while the DOM stayed at its default.
        if self.normalizer.is_none() {
            // Built into a local first: the schema borrow of `core` must end
            // before the normaliser is stored next to it.
            let built = self
                .core
                .schema()
                .map(rove_client_core::runtime_delta::RuntimeDeltaNormalizer::new);
            match built {
                Some(Ok(normalizer)) => self.normalizer = Some(normalizer),
                Some(Err(error)) => {
                    log::warn!("session: rove refused this schema's normalizer: {error}");
                }
                None => {}
            }
        }
        // The world as it stood before this packet, read once for the batch.
        //
        // The globals packet is the one receipt with no world before it, and it
        // is also the record that creates the `DataModel`, the services and our
        // own `Player`. So an absent view means the empty one — not "skip
        // normalization", which is how the globals record would arrive raw.
        //
        // Captured before the drain, not inside it: this is one `WorldView`
        // clone for the batch rather than one per record, and every record in
        // the batch is entitled to the same "before the packet" answer.
        let pre_view = self.pre_view.clone().unwrap_or_else(empty_world_view);
        for row in rows {
            // The world as it stood before this record. Each drain consumes the
            // one captured at ingest; a second record in the same batch reuses
            // it only when its sequence is still above that view's, which the
            // normaliser checks and refuses rather than guessing at.
            //
            // It is *not* consumed: a packet can commit more than one record,
            // and every record in it needs the same "before the packet" world.
            // The normaliser itself refuses a view that is too new, so a stale
            // one can only cost a normalization, never a misread.
            // Tag 13 is rove's "the initial world has arrived". It is read from
            // the record, as upstream reads it, before the record is given to
            // the normaliser — records after it may emit compact creates.
            self.initial_tag |= row.value.tags.contains(&13);
            let normalized = match &mut self.normalizer {
                Some(normalizer) => {
                    match normalizer.normalize(&pre_view, &row.value, self.initial_tag) {
                        Ok(normalized) => Some(normalized.worker_delta),
                        Err(error) => {
                            self.normalize_refused += 1;
                            if self.normalize_refused <= 3 {
                                log::warn!(
                                    "session: record {} was not normalized ({error}); its raw \
                                     wire values are being read instead",
                                    row.sequence
                                );
                            }
                            None
                        }
                    }
                }
                None => None,
            };
            // The normalized actions, not the raw ones. The raw shape is what
            // left a live `Workspace` with 954 instances and zero `CFrame`s.
            let actions = normalized
                .as_ref()
                .and_then(|delta| delta["actions"].as_array().cloned())
                .or_else(|| {
                    // No normaliser yet (no schema, or a view it could not
                    // use): the raw actions still build instances, and a world
                    // that is half right beats a world that stops.
                    row.value.world_delta["actions"].as_array().cloned()
                })
                .unwrap_or_default();
            self.cursor = row.sequence;
            through = row.sequence;
            self.rows_applied += 1;
            // Streaming, and the spawn handshake: both read from the record
            // that carried them. A streaming place sends the region set before
            // the parts, and holds the character until this client says the
            // spawn region arrived — so these are answered from here, where the
            // records are, rather than guessed at from the packet count.
            //
            // `regions` is a field of the **record** (`SessionRecord.regions`,
            // rove's `session.rs:160`), not of `world_delta`: that object is
            // `{"version","delta","sequence","actions"}` and never carries a
            // region. Reading it out of the delta is why the overlay said
            // `stream 0/0` for a whole session — no region was ever seen, so no
            // focus was ever sent, so the character never came and the cover
            // never came down.
            self.note_regions(&row.value.regions.clone());
            // Our own Player — the one the account's id names — and whether a
            // character has been replicated for it. Both are read from the
            // ids this batch made, which is the only place the wire's names
            // become refs.
            for action in &actions {
                if action.get("kind").and_then(|k| k.as_str()) != Some("create") {
                    continue;
                }
                let (Some(id), Some(class)) = (
                    action.get("id").and_then(|v| v.as_str()),
                    action
                        .get("node")
                        .and_then(|n| n.get("class_name"))
                        .and_then(|c| c.as_str()),
                ) else {
                    continue;
                };
                if class == "Player" {
                    self.player_ids.insert(id.to_string());
                }
            }
            // A character is a Model with a Humanoid, parented to our Player:
            // the cheapest sound evidence is a `Character` property that
            // resolves, which the `Live` already holds.
            if !self.character_seen && self.character_present() {
                self.character_seen = true;
            }
            // **The pings this record carried.** After globals, a `(4, 3, 131)`
            // packet is world state to the decoder and its requests land in
            // `record.pings`; nothing else reads them, which is why a session
            // that had joined successfully answered no ping at all a second in.
            self.pings_for_record(&row.value.pings);
            // **Markers, once the gate is open.** Both facts are required: a
            // scope (globals committed) and the initial world tag (this record's
            // own `13`). The drain is in receive order, so the answers the server
            // is waiting on go out in the order it asked.
            self.drain_markers();
            // **Acknowledge the `CFrame`s this record changed.** The server
            // keeps a property's next version queued until the client confirms
            // the one it sent; a client that only reads is a client the world
            // stops moving for, one property at a time. rove's
            // `PropertyAcknowledgments` is the accounting (coalescing versions,
            // one write in flight, no retry after an ambiguous send), and the
            // payload it prepares is rove's own `0x83` packet.
            self.acknowledge_properties(&row.value.property_changes);
            if self.prepare_streaming_resume() {
                log::info!("session: spawn region acknowledged; the server will stream the character");
            }
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
        if read > 0 {
            if let Err(error) = self.core.acknowledge(self.reader, through) {
                // Acknowledgment that fails is not fatal — the world it named is
                // already applied — but it is the one error that makes the
                // journal grow without bound, so it is counted and said out loud.
                self.acknowledged_errors += 1;
                log::warn!("session: journal acknowledgment failed: {error}");
            } else {
                self.acknowledged_through = through;
            }
        }
        read
    }

    /// Observe one record's property changes and queue the acknowledgment they
    /// are owed, if there is one to send.
    ///
    /// Only `CFrame`s are acknowledged: rove's own rule
    /// (`property_acknowledgments::eligible`), because a frame is the one
    /// property the server will not send again until it knows the client has
    /// this one. Observing more would acknowledge state this client reads but
    /// the server never had to wait for.
    fn acknowledge_properties(&mut self, changes: &[serde_json::Value]) {
        if changes.is_empty() {
            return;
        }
        let (Some(schema), Some(view)) = (self.core.schema(), self.core.world_view()) else {
            return;
        };
        if let Err(error) = self
            .property_acks
            .observe(schema, &view, changes, self.rows_applied as u32)
        {
            log::debug!("session: property acknowledgment refused: {error}");
            return;
        }
        // One payload in flight at a time (`prepare` says `None` while one is),
        // and its id is kept: the transport is what says whether the write went,
        // and an ambiguous one must not be retried.
        match self.property_acks.prepare(&view) {
            Ok(Some(prepared)) => {
                self.ack_in_flight = Some(prepared.id());
                self.outbound.push(Outbound::PropertyAck(
                    REPLICATION_APPLICATION,
                    REPLICATION_CHANNEL,
                    prepared.payload().to_vec(),
                ));
            }
            Ok(None) => {}
            Err(error) => log::debug!("session: property acknowledgment not prepared: {error}"),
        }
    }

    /// **The send the queue was waiting on has happened** (or provably has not).
    ///
    /// `written == false` is not a retry: rove's queue marks itself
    /// `transport_submission_ambiguous_no_retry` and stops, because a receipt
    /// that may or may not have arrived cannot be safely repeated.
    pub fn property_ack_written(&mut self, written: bool) {
        let Some(id) = self.ack_in_flight.take() else {
            return;
        };
        if let Err(error) = self.property_acks.commit(id, written) {
            log::debug!("session: property acknowledgment commit failed: {error}");
        }
    }

    /// How many property acknowledgments have gone out, and how many are queued.
    pub fn property_ack_report(&self) -> (u64, u64) {
        let report = self.property_acks.report();
        let sent = report
            .get("packets_sent")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let pending = report
            .get("pending")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        (sent, pending)
    }

    /// **Names a stall while it is happening.** See [`STALL_REPORT_MS`].
    ///
    /// Everything here is a counter or a map length — no document reads, no
    /// allocation beyond the line itself — because it runs on the session's own
    /// loop and the loop is the thing being measured.
    #[cfg(feature = "transport")]
    fn report_stall(&mut self, silent_for: std::time::Duration, ticks: u64) {
        self.stalls_reported += 1;
        let (frames, ack_packets, ack_pending) = self.property_ack_counts();
        let held = self.held_packets.len();
        // The most recent arrivals, by `(application, channel, kind)`: the one
        // line that says what a server is still sending when the world is not
        // moving (a keep-alive only? pings only? the world, refused?).
        let mut arrivals: Vec<_> = self.arrivals.iter().collect();
        arrivals.sort_by_key(|(_, &n)| std::cmp::Reverse(n));
        let top: String = arrivals
            .iter()
            .take(4)
            .map(|((application, channel, kind), n)| format!("{application}/{channel}:{kind}x{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        log::warn!(
            "session: quiet for {} ms after {} s — {} pkt ({} rep, {} refused, {} rows), \
             held {}, frames {} (acks {} out, {} pending), physics {}/{} ({} skipped), focus {} ({} ready), \
             datagrams {}/{}, writes {} stalled {} refused, {} tick(s) served since the last \
             line, top arrivals: {}",
            silent_for.as_millis(),
            self.started.elapsed().as_secs(),
            self.ingested.packets,
            self.ingested.replication,
            self.ingested.refused,
            self.rows_applied,
            held,
            frames,
            ack_packets,
            ack_pending,
            self.physics_frames_sent,
            self.physics_frames_failed,
            self.physics_skipped,
            self.focus_sent,
            self.streaming_ready_sent,
            self.datagrams_fed,
            self.unknown_datagrams,
            self.writes_stalled,
            self.writes_refused,
            ticks,
            if top.is_empty() { "(none)".to_string() } else { top },
        );
        // And the editor's own counters, so its overlay is not the last word:
        if let Some(handover) = &self.handover {
            handover.set_counters(self.overlay_counters());
        }
    }

    /// **The `CFrame` ledger, for a stall's log line.** rove's queue counts what
    /// it was *handed* — `received_cframe_changes` — separately from what it
    /// could send, which is the difference that names the fault: changes
    /// arriving with `packets_sent == 0` and `pending == 0` is a queue refusing
    /// every one of them (an index or a schema mismatch), while zero changes at
    /// all is a server that never sent a frame, and neither is the same bug as
    /// an acknowledgment that went out and was not committed.
    pub fn property_ack_counts(&self) -> (u64, u64, u64) {
        let report = self.property_acks.report();
        let n = |k: &str| report.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
        (n("received_cframe_changes"), n("packets_sent"), n("pending"))
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
    // `mut` on the *binding*, not only on the type: a `&mut Session` that is not
    // a mutable binding cannot be re-borrowed, so `feed_on(&mut session)` —
    // which `flush_outbound` takes — is E0596.
    mut session: &mut Session,
    seconds: u64,
    mut progress: impl FnMut(&Session),
) -> Result<Stage, &'static str> {
    let allocation = transport_allocation(issued)?;
    // **Whose character to look for.** The join allocation names the user; every
    // "our Player", "our character" and "our control" decision in this session
    // is a comparison against it. It was never assigned, so `local_player()`
    // answered `None` for a whole session and no character was ever recognised
    // as ours even when the server sent it.
    session.account_user_id = Some(issued.user_id);
    // The job id a `155` challenge program reads as `game.JobId`. Set from the
    // allocation here, once, so the first challenge has it.
    if session.game_id.is_none() {
        session.game_id = issued.game_id.clone();
    }
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
    // The ping responder reads the profile from these bits: on the QUIC profile
    // the legacy replication statistics are not collected, and the reply says so
    // by blocking that provider rather than sending a zero it never measured.
    session.set_negotiated(negotiated);

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
    // The map is seeded with the same announcements, in the same place they are
    // built: a datagram naming wire 2 or 4 is the clock's, and until the server
    // announces its own channels that is the only way to know. The server's
    // announcements replace nothing — `parse_control_open` of its own names more
    // wires — so this is the client's half of the contract, not a guess.
    for (channel, wire) in [(2u8, 2u32), (3, 4)] {
        session.channels.insert(wire, (6, u32::from(channel)));
    }
    // Unreliable replies (a ping request whose route byte asked for one) get
    // their own announced wire id, `rove`'s `PING_WIRE_ID = 6`. One writer for
    // the session: the announcement is once, and the sequence is per connection.
    let mut unreliable_writer = UnreliableWriter::default();
    // The physics uplink's own writer and clock: the timestamp a root frame
    // carries is this session's monotonic milliseconds, the same clock a ping
    // reply's words are read from.
    let mut physics = PhysicsWriter {
        announced: false,
        sequence: 0,
    };
    let mut sync = TimeSyncProbe::default();
    sync.start(&clock).map_err(|_| "the time-sync probe could not be started")?;
    log::info!("session: wire clock initialized, clock channels 6/1, 6/2, 6/3 announced");

    // The five login packets, in the order the server reads them.
    let packets = login_packets(issued).map_err(|_| "login packets could not be encoded")?;
    for (index, packet) in packets.iter().enumerate() {
        // **Bounded, and retried while the peer is merely slow.** Five packets
        // in a row are the first writes of the connection, so a stall here is
        // the whole join; a stall that repeats is the server not reading, and
        // that is a connection failure to report rather than to wait out.
        let mut attempt = 0;
        loop {
            match send_bounded(
                &mut session,
                &sender,
                REPLICATION_APPLICATION,
                REPLICATION_CHANNEL,
                packet,
            )
            .await
            {
                Ok(()) => break,
                Err(error) if attempt >= 2 => {
                    log::warn!("session: login packet {} was refused: {error}", index + 1);
                    return Err("a login packet could not be sent");
                }
                // A torn-down stream from a cancelled write: the next attempt
                // opens a new one. Anything else has already been logged.
                Err(_) => attempt += 1,
            }
        }
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
    // **A stall is named while it is happening, not after the session ends.**
    // A join that freezes is reported from a phone as a screenshot of an
    // overlay reading `236 pkt / 216 rep / acks 0 / phys 0`, and every number
    // in that line is consistent with three different faults: the loop is
    // wedged on a write, the server has stopped sending, and the loop is
    // turning but nothing is being fed. The heartbeat below fires only when
    // nothing has arrived for [`STALL_REPORT`] — so a healthy session logs
    // nothing at all — and it prints what tells them apart, including what the
    // loop did during the silence.
    let mut stall_tick = tokio::time::interval(std::time::Duration::from_millis(STALL_REPORT_MS));
    stall_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_stall_report: Option<std::time::Instant> = None;
    let mut ticks_since_report = 0u64;
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
                // An `0xe3` on application 4 is the extended packet family: once
                // one is seen the legacy statistics are not this session's to
                // report, and its ping replies stop carrying them.
                if application == REPLICATION_APPLICATION && payload.first() == Some(&0xe3) {
                    session.note_extended_family();
                }
                session.feed_on(application, channel, &payload);
                // Anything the packet asked for, written before the next one is
                // read: a handshake the server is waiting on must not queue
                // behind the world it is holding back.
                flush_outbound(&mut session, &sender).await;
                // Pings last, and every time: a record's requests were queued by
                // the feed above, and a reply is only timely if it is written
                // before the next read.
                flush_pings(&mut session, &sender, &connection, &mut unreliable_writer).await;
                // Republished for the first packets as well as every 16 after:
                // a join that stopped at three packets never reached a sixteen,
                // so the report still said "log in: waiting" while the world was
                // arriving. The first sixteen are the ones a person watches.
                fed += 1;
                if fed <= 16 || fed % 16 == 0 {
                    // The handover's counters too, not only the join report's:
                    // `publish` (progress) and the overlay are two different
                    // consumers, and a session whose world has stopped arriving
                    // still has to keep saying where it is — the `join live:`
                    // line and the focus are read from these.
                    if let Some(handover) = session.handover.as_ref() {
                        handover.set_counters(session.overlay_counters());
                    }
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
            _ = stall_tick.tick() => {
                // A tick that keeps being served is a loop that is turning; one
                // that is not served is a loop blocked in a write, and the
                // difference is the whole diagnosis. (Only a *silent* session is
                // reported, and only once per silence.)
                ticks_since_report += 1;
                session.stall_ticks += 1;
                let silent_for = session
                    .last_packet_at
                    .map(|at| at.elapsed())
                    .unwrap_or_else(|| session.started.elapsed());
                if silent_for >= std::time::Duration::from_millis(STALL_REPORT_MS) {
                    let repeat = last_stall_report
                        .is_none_or(|at| at.elapsed() >= std::time::Duration::from_millis(STALL_REPORT_MS));
                    if repeat {
                        last_stall_report = Some(std::time::Instant::now());
                        session.report_stall(silent_for, ticks_since_report);
                        ticks_since_report = 0;
                    }
                }
            }
            _ = sync_tick.tick() => {
                // **The replicator wire clock, not a local one.** A root frame's
                // timestamp is read by the server against the same clock the
                // time-sync probe measures, so it has to come from that clock and
                // not from "milliseconds since this process started". No reading
                // means no frame: a zero timestamp is a claim about an instant in
                // 1970 and the server would place the character by it.
                let physics_now_ms = match clock.physics_milliseconds() {
                    Ok(ms) => Some(ms),
                    Err(error) => {
                        log::warn!("session: the physics clock has no reading: {error}");
                        None
                    }
                };
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
                // And the streamer's own keep-alive, on the same tick: a
                // streaming place only sends the parts around this client's
                // focus, and a focus lapses. It cannot be `None` here — the
                // `if` is the answer — but `refocus` re-checks, because it is
                // also called from a path where no region has arrived.
                let from_editor = session
                    .handover
                    .as_ref()
                    .and_then(|live_world| live_world.focus());
                if session.refocus(from_editor) {
                    flush_outbound(&mut session, &sender).await;
                }
                // The 20 ms tick is the other place replies go out: a request
                // queued by a record is answered here when the read loop is
                // quiet, which is most of a joined session.
                flush_pings(&mut session, &sender, &connection, &mut unreliable_writer).await;
                // And the character's own motion, at the same rate the tick
                // runs: the server reconciles a client by the root frames it
                // receives, and a client that sends none is one whose character
                // stands still for everyone but itself.
                if let Some(now_ms) = physics_now_ms {
                    physics.tick(&mut session, &sender, &connection, now_ms).await;
                }
                // **The counters, every second, whatever else moved.** The
                // handover's numbers were only refreshed by `drain` (a packet
                // with world actions) and by the 16-packet beat, so a session
                // whose remaining traffic was pings and datagrams published
                // nothing: the overlay stood at its last value while the session
                // was alive and the loop was turning. One assignment a second is
                // the cheapest possible answer to "is this stuck?".
                if let Some(handover) = session.handover.as_ref() {
                    handover.set_counters(session.overlay_counters());
                }
            }
            // **A datagram, routed by its announcement.** This used to keep
            // only the clock's and throw the rest away, which is exactly the
            // bug that arrived reported from a phone: a live world with every
            // instance in it and every part frozen, because `(6, 3)` was the
            // only wire id this loop would read a payload from.
            //
            // Now the wire id is looked up in the map the control channel
            // filled: the clock's channel goes to the probe, and everything
            // else is world state and takes the same `feed_on` path a reliable
            // packet takes. A datagram naming an unannounced wire id is
            // *counted*, not dropped silently — the difference between "the
            // server never announced it" and "the client ignored it" is the
            // whole diagnosis.
            datagram = receive_unreliable(&connection) => {
                session.datagrams += 1;
                match datagram {
                    Ok(packet) => {
                        if !packet.complete {
                            session.fragmented_datagrams += 1;
                            continue;
                        }
                        // **The clock's replies, then world state.** The clock's
                        // wire id is not hard-coded: the map is seeded with the
                        // ids this client announces at login (`(6, 2)` on wire 2,
                        // `(6, 3)` on wire 4), and `sync.receive` is the reader
                        // that knows what a reply means. Every other datagram is
                        // a game packet and takes `feed_on`'s path.
                        let fed_world =
                            session.datagram(packet.wire_id, &packet.payload, &clock, &mut sync);
                        if fed_world {
                            // Republished every 16 accepted packets, reliable or
                            // not: a session moving only on datagrams still
                            // moves the overlay.
                            fed += 1;
                            if fed % 16 == 0 {
                                progress(session);
                            }
                        }
                        // Whatever a datagram asked for goes out now: a ping
                        // that arrives unreliably is a ping the server is
                        // timing, and a reply queued behind the next read is a
                        // reply the server has already given up on.
                        flush_outbound(&mut session, &sender).await;
                        flush_pings(&mut session, &sender, &connection, &mut unreliable_writer)
                            .await;
                    }
                    // A datagram that will not parse is not the end of the
                    // connection: count it and read the next one.
                    Err(error) => {
                        session.unreadable_datagrams += 1;
                        log::debug!("session: a datagram was unreadable: {error}");
                    }
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
    // **Why it ended, and what was waiting when it did.** The report a phone
    // gets is this log, and a session that stops has one of a few reasons; a
    // line that names it is the difference between "the join failed" and "the
    // join ended because the server opened no further stream". The write
    // counters are here for the same reason: a loop that gave up on a write is
    // a session that looked alive and was not.
    // A session that was quiet at any point says so in its last line too: the
    // stall reports above are `warn`s a log may have rotated past, and the
    // summary is what a phone's "copy the log" hands over.
    if session.stalls_reported > 0 {
        log::warn!(
            "session: {} stall report(s) during this session; {} heartbeat(s) served",
            session.stalls_reported,
            session.stall_ticks,
        );
    }
    log::info!(
        "session: ended ({}) after {} s; {} stalled write(s), {} refused, {} \
         batch(es) still waiting for the editor",
        end.unwrap_or("the caller stopped it"),
        session.started.elapsed().as_secs(),
        session.writes_stalled,
        session.writes_refused,
        session
            .handover
            .as_ref()
            .map(|handover| handover.pending())
            .unwrap_or(0)
    );
    progress(session);
    // A join's own shutdown must not outlive its deadline either.
    connection.close(0u32.into(), b"done");
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), endpoint.wait_idle()).await;
    Ok(session.stage.clone())
}

/// One reliable write, bounded, so a peer that stops reading cannot stop the loop.
///
/// The reason this exists rather than `sender.send(..).await` inline: the
/// sender's stream map is held across its await, so an unbounded write is not
/// one stalled reply but a stalled `select!` — no tick, no counters, no flush,
/// and a session that looks frozen while it is in fact waiting on somebody
/// else. A timeout here leaves the stream alone (wtransport drops a stream whose
/// write was cancelled, and the next write opens another), counts the stall,
/// and lets the loop turn.
///
/// Takes `&mut Session` only for the counters; nothing else about the session is
/// touched, so it is safe to call from every arm.
/// Why a bounded write did not go out.
#[cfg(feature = "transport")]
#[derive(Debug)]
enum WriteError {
    /// The transport refused it (a closed or reset stream).
    Refused(rove_client_core::transport::TransportError),
    /// It did not return inside [`WRITE_STALL_MS`], and waiting longer would
    /// have stopped the loop for a peer that is not reading.
    Stalled,
}

#[cfg(feature = "transport")]
impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Refused(error) => write!(f, "{error}"),
            WriteError::Stalled => write!(f, "stalled for {WRITE_STALL_MS} ms"),
        }
    }
}

#[cfg(feature = "transport")]
async fn send_bounded(
    session: &mut Session,
    sender: &ReliableSender,
    application: u8,
    channel: u32,
    payload: &[u8],
) -> Result<(), WriteError> {
    match tokio::time::timeout(
        std::time::Duration::from_millis(WRITE_STALL_MS),
        sender.send(application, channel, payload),
    )
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            session.writes_refused += 1;
            log::warn!("session: a reliable write was refused: {error}");
            Err(WriteError::Refused(error))
        }
        Err(_) => {
            session.writes_stalled += 1;
            log::warn!(
                "session: a reliable write to {application}/{channel} did not return in {WRITE_STALL_MS} ms; \
                 the loop went on without it ({} stalled so far)",
                session.writes_stalled
            );
            Err(WriteError::Stalled)
        }
    }
}


/// One unreliable reply channel: announce it once, then send datagrams.
///
/// `rove`'s `PingWriter`, reduced to what a join needs. An unreliable reply
/// must not be written on the reliable stream: the server's timing reads the
/// *route* the request asked for, and a datagram's sequence is part of the
/// measurement. The announcement is `control_open(4, 3, 6)` on the control
/// channel, once, before the first datagram — a bare `wire_id` with no
/// announcement is a datagram the server cannot attribute.
#[cfg(feature = "transport")]
#[derive(Default)]
struct UnreliableWriter {
    announced: bool,
    sequence: u64,
}

#[cfg(feature = "transport")]
impl UnreliableWriter {
    async fn send(
        &mut self,
        session: &mut Session,
        sender: &ReliableSender,
        connection: &Connection,
        application: u8,
        channel: u32,
        wire_id: u32,
        payload: &[u8],
    ) -> Result<(), &'static str> {
        if !self.announced {
            let announcement = rove_client_core::channels::control_open(
                application,
                u8::try_from(channel).map_err(|_| "the channel number is too large")?,
                wire_id,
            )
            .map_err(|_| "the datagram channel could not be announced")?;
            send_bounded(
                session,
                sender,
                application,
                rove_client_core::channels::CONTROL_CHANNEL,
                &announcement,
            )
            .await
            .map_err(|error| {
                log::warn!("session: a datagram channel was not announced: {error}");
                "the datagram channel announcement was not written"
            })?;
            self.announced = true;
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("the datagram sequence is exhausted")?;
        rove_client_core::transport::session::send_unreliable(
            connection,
            wire_id,
            self.sequence,
            payload,
        )
        .map_err(|_| "the datagram was not written")
    }
}

/// **Write every reply the session is waiting to answer, on the route asked.**
///
/// A ping reply is the reason this exists as a separate step from
/// [`flush_outbound`]: its three clock words are *received, queued, written*, so
/// the last one can only be sampled here, at the write. An unreliable request is
/// answered with a datagram on its own announced wire id; a reliable one goes out
/// on `(4, 3)` as before.
///
/// Nothing is sent with an observation missing: the responder blocks the reply,
/// counts why, and drops it. A reply the server accepts and then trusts is worse
/// than one it never got.
#[cfg(feature = "transport")]
async fn flush_pings(
    session: &mut Session,
    sender: &ReliableSender,
    connection: &Connection,
    unreliable: &mut UnreliableWriter,
) {
    // Bounded, as upstream's flush is: a server that floods requests must not
    // hold the read loop here.
    for _ in 0..crate::ping::MAX_FLUSH_BATCH {
        let Some(route) = session.ping_pending_route() else {
            return;
        };
        let Some(prepared) = session.prepare_ping_reply() else {
            // Blocked at prepare: the responder already dropped it and counted
            // which observation was missing, so this moves to the next.
            continue;
        };
        let token = prepared.token();
        let written = match route {
            crate::ping::DeliveryRoute::Reliable => send_bounded(
                session,
                sender,
                crate::ping::PING_APPLICATION,
                crate::ping::PING_CHANNEL,
                prepared.payload(),
            )
            .await
            .is_ok(),
            crate::ping::DeliveryRoute::Unreliable => unreliable
                .send(
                    session,
                    sender,
                    connection,
                    crate::ping::PING_APPLICATION,
                    crate::ping::PING_CHANNEL,
                    crate::ping::PING_WIRE_ID,
                    prepared.payload(),
                )
                .await
                .is_ok(),
        };
        if written {
            session.replies_sent += 1;
        } else {
            session.replies_failed += 1;
            log::warn!("session: a ping reply could not be written");
        }
        session.ping_reply_written(token, written);
    }
}

/// **The character-root physics uplink** (`(4, 6)` on wire id 8).
///
/// Ported from `rove`'s `live_session.rs` tick: the editor's latest character
/// pose, if it is still fresh, encoded as one own-root client-current record and
/// sent as a datagram. The channel is announced once (`control_open(4, 6, 8)`),
/// because a bare wire id is a datagram the server cannot attribute.
///
/// What this is not: a claim of authority. The record says "my root is here" for
/// the one assembly the server already granted this client; every other assembly
/// in the place stays the server's, and this encoder cannot express one.
#[cfg(feature = "transport")]
struct PhysicsWriter {
    announced: bool,
    sequence: u64,
}

#[cfg(feature = "transport")]
impl PhysicsWriter {
    /// Send the pose, if there is a fresh one. Returns whether a datagram went.
    async fn tick(
        &mut self,
        session: &mut Session,
        sender: &ReliableSender,
        connection: &Connection,
        now_ms: u64,
    ) -> bool {
        let Some(pose) = session
            .handover
            .as_ref()
            .and_then(|live| live.character_pose())
        else {
            // No character, or a pose too old to describe one: nothing to say,
            // and an old record is worse than silence. Counted, because `phys 0`
            // in a stall report is this line and nothing else: whether the
            // editor has a pose at all (`character_pose` is `None`) or nobody
            // owns the character to publish one (`play_owned_root`) are two
            // different answers, and the session cannot see which.
            session.physics_skipped = session.physics_skipped.saturating_add(1);
            return false;
        };
        if !self.announced {
            let Ok(announcement) = rove_client_core::channels::control_open(
                crate::physics_sender::APPLICATION,
                crate::physics_sender::CHANNEL,
                crate::physics_sender::WIRE_ID,
            ) else {
                return false;
            };
            if send_bounded(
                session,
                sender,
                crate::physics_sender::APPLICATION,
                rove_client_core::channels::CONTROL_CHANNEL,
                &announcement,
            )
            .await
            .is_err()
            {
                return false;
            }
            self.announced = true;
        }
        let Some(next) = self.sequence.checked_add(1) else {
            return false;
        };
        self.sequence = next;
        let payload = match crate::physics_sender::encode_root_frame_with_rotation(
            pose.root,
            now_ms,
            self.sequence,
            pose.position.map(f64::from),
            pose.rotation,
            pose.velocity,
            pose.grounded,
        ) {
            Ok(payload) => payload,
            Err(error) => {
                log::debug!("session: a physics frame was not encodable: {error}");
                return false;
            }
        };
        match rove_client_core::transport::session::send_unreliable(
            connection,
            crate::physics_sender::WIRE_ID,
            self.sequence,
            &payload,
        ) {
            Ok(()) => {
                session.physics_frames_sent += 1;
                true
            }
            Err(error) => {
                session.physics_frames_failed += 1;
                log::debug!("session: a physics frame was not written: {error}");
                false
            }
        }
    }
}

/// Write everything a session has queued, in order, and account for it.
///
/// The session's replies are all `(application, channel, payload)` — marker
/// handshakes, ping replies, property acknowledgments, replication focus — and
/// the accounting is *per kind* only where a caller asks; here what matters is
/// that a failed write is counted rather than being fatal (the session keeps
/// reading) and that a property acknowledgment the transport refused is
/// reported to its queue, which must never retry an ambiguous receipt.
#[cfg(feature = "transport")]
async fn flush_outbound(session: &mut Session, sender: &ReliableSender) {
    if session.outbound.is_empty() {
        return;
    }
    // `take`, not `drain` while awaiting: a reply queued by an await point
    // would otherwise be appended to the vector being drained and sent twice.
    let queued = std::mem::take(&mut session.outbound);
    let mut ack_written = None;
    for reply in queued {
        let (application, channel) = (reply.application(), reply.channel());
        // A property acknowledgment is the one reply whose success the session
        // has to record: its queue holds the next version until this one is
        // confirmed. (By kind, never by first byte — `[131, 26]` is a focus.)
        let is_ack = reply.is_property_ack();
        match send_bounded(session, sender, application, channel, reply.payload()).await {
            Ok(()) => {
                session.replies_sent += 1;
                if is_ack {
                    ack_written = Some(true);
                    session.property_acks_sent += 1;
                }
            }
            Err(error) => {
                session.replies_failed += 1;
                if is_ack {
                    ack_written = Some(false);
                }
                log::warn!(
                    "session: a reply on {application}/{channel} could not be sent: {error}"
                );
            }
        }
    }
    if let Some(written) = ack_written {
        session.property_ack_written(written);
    }
}

/// This session's ping observations.
///
/// A provider, not a snapshot: the responder calls it at the moment each word is
/// needed, and every method may refuse. Refusing is the point — the alternative
/// is a reply with a zero in a field the server reads as a measurement.
struct SessionPingTelemetry {
    clock: crate::ping::PingClock,
    supported_profile: bool,
    extended_family_seen: bool,
    integrity: crate::session_integrity::SessionIntegrity,
}

impl crate::ping::PingTelemetryProvider for SessionPingTelemetry {
    fn enqueue_clock_word(&mut self) -> Result<u32, &'static str> {
        Ok(self.clock.word())
    }

    fn write_clock_word(&mut self) -> Result<u32, &'static str> {
        // Sampled at the write, which is why `prepare` is called there and
        // nowhere earlier.
        Ok(self.clock.word())
    }

    fn replication_stats(&mut self) -> Result<crate::ping::ReplicationPingStats, &'static str> {
        if !self.supported_profile || self.extended_family_seen {
            // Upstream's exact refusal: the legacy statistics belong to a packet
            // family this session is not on, and zero is not a reading of them.
            return Err("the server's replication-statistics profile is not collected here");
        }
        Ok(crate::ping::ReplicationPingStats {
            replication_word: 0,
            tail_words: [0, 0],
        })
    }

    fn integrity_status(
        &mut self,
    ) -> Result<crate::ping::IntegrityStatusEvidence, &'static str> {
        self.integrity.wire_evidence()
    }
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

/// `"scope:index"` as the wire's two-number instance key.
///
/// The stream names instances this way and `replication_out` wants the pair;
/// scope 0 is not a real scope and the encoder refuses it, so it is refused
/// here too rather than at the encoder, where the reason is less clear.
fn parse_instance_guid(id: &str) -> Result<(u32, u32), ()> {
    let (scope, index) = id.split_once(':').ok_or(())?;
    let scope: u32 = scope.parse().map_err(|_| ())?;
    let index: u32 = index.parse().map_err(|_| ())?;
    if scope == 0 {
        return Err(());
    }
    Ok((scope, index))
}

/// The world before the first packet: no nodes, sequence zero.
///
/// The globals receipt is the only one that has no world before it, and the
/// normaliser needs *a* view to check its base sequence against. rove's own
/// session builds the same empty store internally (`session.rs`, `empty_world`),
/// so this is the same starting point, not a second opinion about one.
fn empty_world_view() -> rove_client_core::world::WorldView {
    rove_client_core::world::WorldStore::from_snapshot(&serde_json::json!({
        "version": 1,
        "sequence": 0,
        "nodes": []
    }))
    .expect("the built-in empty world snapshot is valid")
    .view()
}
