//! The replication ping: what the server asks on `(4, 3)`, and the answer.
//!
//! This is rove's `rust-client/src/ping_responder.rs` reduced to the part that
//! is portable — the codec. The responder itself (a bounded queue, a transport
//! trait, telemetry providers) is not here, because a join does not need a
//! queue: `Session::ping_requested` sees one packet, answers it or does not, and
//! the network loop writes what it left in `outbound`.
//!
//! **Why the answer needs the session.** A ping reply is not an echo. It carries
//! a clock word the client reads from its own replicator, six reply words of
//! replication statistics, and an integrity status word. rove's responder has no
//! defaults for those on purpose: a reply with invented telemetry is a reply the
//! server accepts and then trusts. So this module produces the encoding given
//! real observations, and `Session` supplies them from the clock it already runs
//! for the time-sync probe.
//!
//! Ported rather than depended on: `rove/rust-client` is a whole client target,
//! and making it a dependency changes the workspace's feature resolution (see
//! `examples/crosscheck.rs`). The encoding is checked against rove's own in
//! `tests/ping.rs` by loading the reference by path.

use rove_client_core::wire::Reader;

pub const MAX_PING_PACKET_BYTES: usize = 4096;
pub const MAX_PING_ITEMS: usize = 64;
pub const PING_APPLICATION: u8 = 4;
pub const PING_CHANNEL: u32 = 3;

/// The packet header every ping starts with: `0x83`, which is 131.
///
/// Worth writing both ways because the two notations are read apart: rove's
/// `PingCounts::from_packet` compares against `131`, and `encode_ping_reply`
/// writes `0x83`. They are the same byte, and a reader who takes "not 0x83" for
/// "not 131" goes looking for a bug that is not there.
pub const PING_HEADER: u8 = 0x83;
pub const ITEM_REQUEST: u8 = 5;
pub const ITEM_RESPONSE: u8 = 6;
/// The reply prefix, as rove writes it: the same header byte, the response
/// item kind, and sub-kind 1.
const REPLY_PREFIX: [u8; 3] = [PING_HEADER, ITEM_RESPONSE, 1];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PingKind {
    Request,
    Response,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PingFlags {
    Basic,
    ReplyWords,
    ExtraWord,
    ReplyWordsAndExtraWord,
}

impl PingFlags {
    fn from_bits(bits: u8) -> Result<Self, PingCodecError> {
        match bits {
            0 => Ok(Self::Basic),
            1 => Ok(Self::ReplyWords),
            4 => Ok(Self::ExtraWord),
            5 => Ok(Self::ReplyWordsAndExtraWord),
            other => Err(PingCodecError::InvalidFlags(other)),
        }
    }

    fn has_reply_words(self) -> bool {
        matches!(self, Self::ReplyWords | Self::ReplyWordsAndExtraWord)
    }

    fn has_extra_word(self) -> bool {
        matches!(self, Self::ExtraWord | Self::ReplyWordsAndExtraWord)
    }
}

/// One item in a ping packet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PingItem {
    pub kind: PingKind,
    pub flags: PingFlags,
    pub timestamp: u64,
    pub request_word: u32,
    pub reply_words: Option<[u32; 6]>,
    pub trailer_words: [u32; 2],
    pub unreliable: bool,
    pub extra_word: Option<u32>,
}

impl PingItem {
    fn read(reader: &mut Reader<'_>, kind: u8) -> Result<Self, PingCodecError> {
        let kind = match kind {
            ITEM_REQUEST => PingKind::Request,
            ITEM_RESPONSE => PingKind::Response,
            other => return Err(PingCodecError::InvalidKind(other)),
        };
        let bits = reader.u8().map_err(|_| PingCodecError::Truncated)?;
        let flags = PingFlags::from_bits(bits)?;
        let timestamp = reader.u64().map_err(|_| PingCodecError::Truncated)?;
        let request_word = reader.u32().map_err(|_| PingCodecError::Truncated)?;
        let reply_words = if flags.has_reply_words() {
            let mut words = [0u32; 6];
            for word in &mut words {
                *word = reader.u32().map_err(|_| PingCodecError::Truncated)?;
            }
            Some(words)
        } else {
            None
        };
        let trailer_words = [
            reader.u32().map_err(|_| PingCodecError::Truncated)?,
            reader.u32().map_err(|_| PingCodecError::Truncated)?,
        ];
        let route = reader.u8().map_err(|_| PingCodecError::Truncated)?;
        if route > 1 {
            return Err(PingCodecError::InvalidRoute(route));
        }
        let extra_word = if flags.has_extra_word() {
            Some(reader.u32().map_err(|_| PingCodecError::Truncated)?)
        } else {
            None
        };
        Ok(Self {
            kind,
            flags,
            timestamp,
            request_word,
            reply_words,
            trailer_words,
            unreliable: route != 0,
            extra_word,
        })
    }

    /// A packet item that is a reply already, which must not be answered.
    fn is_reply(&self) -> bool {
        self.kind == PingKind::Response || self.flags.has_reply_words()
    }

    /// Only a plain request gets an answer. A flagged one asks for things this
    /// client does not collect, and a reply that pretends otherwise is worse
    /// than no reply.
    fn supported_request(&self) -> bool {
        self.kind == PingKind::Request && self.flags == PingFlags::Basic
    }

    /// A committed record's ping item, validated field by field.
    ///
    /// `SessionRecord.pings` is rove's decoded form:
    /// `{kind, flags, timestamp, first_word, reply_words, trailer_words,
    /// unreliable[, extra_word]}`. This is where a post-globals request is
    /// read — the packet that carried it was committed as world state, and the
    /// request never reaches the wire decoder again. Validating the cross-field
    /// shape (six words only when flagged, no extra word unless flagged) is what
    /// keeps a malformed record from becoming a reply with invented content.
    pub fn from_record(value: &serde_json::Value) -> Result<Self, PingCodecError> {
        let kind = match value.get("kind").and_then(|v| v.as_u64()) {
            Some(5) => PingKind::Request,
            Some(6) => PingKind::Response,
            Some(_) => return Err(PingCodecError::InvalidRecord),
            None => return Err(PingCodecError::InvalidRecord),
        };
        let raw_flags = value
            .get("flags")
            .and_then(|v| v.as_u64())
            .ok_or(PingCodecError::InvalidRecord)?;
        let flags = PingFlags::from_bits(
            u8::try_from(raw_flags).map_err(|_| PingCodecError::InvalidRecord)?,
        )?;
        let timestamp = value
            .get("timestamp")
            .and_then(|v| v.as_u64())
            .ok_or(PingCodecError::InvalidRecord)?;
        let request_word = u32::try_from(
            value
                .get("first_word")
                .and_then(|v| v.as_u64())
                .ok_or(PingCodecError::InvalidRecord)?,
        )
        .map_err(|_| PingCodecError::InvalidRecord)?;
        let reply_words = if flags.has_reply_words() {
            let values = value
                .get("reply_words")
                .and_then(|v| v.as_array())
                .ok_or(PingCodecError::InvalidRecord)?;
            if values.len() != 6 {
                return Err(PingCodecError::InvalidRecord);
            }
            let mut out = [0u32; 6];
            for (dst, value) in out.iter_mut().zip(values) {
                *dst = u32::try_from(
                    value.as_u64().ok_or(PingCodecError::InvalidRecord)?,
                )
                .map_err(|_| PingCodecError::InvalidRecord)?;
            }
            Some(out)
        } else {
            if !value
                .get("reply_words")
                .and_then(|v| v.as_array())
                .is_some_and(|words| words.is_empty())
            {
                return Err(PingCodecError::InvalidRecord);
            }
            None
        };
        let trailer = value
            .get("trailer_words")
            .and_then(|v| v.as_array())
            .ok_or(PingCodecError::InvalidRecord)?;
        if trailer.len() != 2 {
            return Err(PingCodecError::InvalidRecord);
        }
        let trailer_words = [
            u32::try_from(
                trailer[0]
                    .as_u64()
                    .ok_or(PingCodecError::InvalidRecord)?,
            )
            .map_err(|_| PingCodecError::InvalidRecord)?,
            u32::try_from(
                trailer[1]
                    .as_u64()
                    .ok_or(PingCodecError::InvalidRecord)?,
            )
            .map_err(|_| PingCodecError::InvalidRecord)?,
        ];
        let unreliable = value
            .get("unreliable")
            .and_then(|v| v.as_bool())
            .ok_or(PingCodecError::InvalidRecord)?;
        let extra_word = if flags.has_extra_word() {
            Some(
                u32::try_from(
                    value
                        .get("extra_word")
                        .and_then(|v| v.as_u64())
                        .ok_or(PingCodecError::InvalidRecord)?,
                )
                .map_err(|_| PingCodecError::InvalidRecord)?,
            )
        } else {
            if !value.get("extra_word").is_none_or(|v| v.is_null()) {
                return Err(PingCodecError::InvalidRecord);
            }
            None
        };
        Ok(Self {
            kind,
            flags,
            timestamp,
            request_word,
            reply_words,
            trailer_words,
            unreliable,
            extra_word,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PingCodecError {
    PacketTooLarge,
    InvalidHeader,
    InvalidKind(u8),
    InvalidFlags(u8),
    InvalidRoute(u8),
    TooManyItems,
    MissingTerminator,
    TrailingBytes,
    Truncated,
    /// A committed record's ping item whose fields do not form a ping.
    InvalidRecord,
    NotARequest,
}

impl std::fmt::Display for PingCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PacketTooLarge => write!(f, "the packet is too large to be a ping"),
            Self::InvalidHeader => write!(f, "it does not start 0x83 (131)"),
            Self::InvalidKind(kind) => write!(f, "item kind {kind} is not a ping"),
            Self::InvalidFlags(bits) => write!(f, "flags {bits:#x} are not a ping's"),
            Self::InvalidRoute(route) => write!(f, "route {route} is not reliable or unreliable"),
            Self::TooManyItems => write!(f, "more items than a ping may carry"),
            Self::MissingTerminator => write!(f, "it has no item terminator"),
            Self::TrailingBytes => write!(f, "bytes follow the terminator"),
            Self::Truncated => write!(f, "it ends mid-item"),
            Self::InvalidRecord => write!(f, "the committed ping record is malformed"),
            Self::NotARequest => write!(f, "it is not a plain request, so it takes no reply"),
        }
    }
}

/// Every item in a ping packet, or the reason it is not one.
pub fn decode_ping_packet(payload: &[u8]) -> Result<Vec<PingItem>, PingCodecError> {
    if payload.len() > MAX_PING_PACKET_BYTES {
        return Err(PingCodecError::PacketTooLarge);
    }
    let mut reader = Reader::new(payload);
    if reader.u8().map_err(|_| PingCodecError::Truncated)? != PING_HEADER {
        return Err(PingCodecError::InvalidHeader);
    }
    let mut items = Vec::with_capacity(4);
    let mut terminated = false;
    while reader.remaining() > 0 {
        let kind = reader.u8().map_err(|_| PingCodecError::Truncated)?;
        if kind == 0 {
            terminated = true;
            if reader.remaining() != 0 {
                return Err(PingCodecError::TrailingBytes);
            }
            break;
        }
        if items.len() >= MAX_PING_ITEMS {
            return Err(PingCodecError::TooManyItems);
        }
        items.push(PingItem::read(&mut reader, kind)?);
    }
    if !terminated {
        return Err(PingCodecError::MissingTerminator);
    }
    Ok(items)
}

/// What a reply carries beyond the request it answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PingReplyTelemetry {
    /// Six words of replication statistics, in the server's order.
    pub reply_words: [u32; 6],
    /// The integrity status word.
    pub integrity_status: u32,
}

/// The reply to one request, in rove's encoding.
///
/// `response_word` is `u32::MAX` when bit 5 of the timestamp is set and `0`
/// otherwise — rove's own rule, kept because a server that checks it will not
/// accept a constant.
pub fn encode_ping_reply(
    item: &PingItem,
    telemetry: &PingReplyTelemetry,
) -> Result<Vec<u8>, PingCodecError> {
    if !item.supported_request() {
        return Err(PingCodecError::NotARequest);
    }
    Ok(encode_reply_for(
        item.timestamp,
        item.request_word,
        item.unreliable,
        telemetry,
    ))
}

/// The reply body for a known-good request shape.
fn encode_reply_for(
    timestamp: u64,
    request_word: u32,
    unreliable: bool,
    telemetry: &PingReplyTelemetry,
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(49);
    bytes.extend_from_slice(&REPLY_PREFIX);
    bytes.extend_from_slice(&timestamp.to_le_bytes());
    bytes.extend_from_slice(&request_word.to_le_bytes());
    for word in telemetry.reply_words {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    bytes.extend_from_slice(&telemetry.integrity_status.to_le_bytes());
    let response_word = if timestamp & 0x20 != 0 { u32::MAX } else { 0 };
    bytes.extend_from_slice(&response_word.to_le_bytes());
    bytes.push(u8::from(unreliable));
    bytes.push(0); // Item terminator.
    bytes
}

/// The one reply a packet is entitled to, or `None`.
///
/// A packet can carry several items, and only the plain requests among them get
/// answers — a packet of replies (`4/3` does both directions) gets none. The
/// first item decides, which is rove's rule: a server's request is the only
/// thing in the packet a client answers.
pub fn reply_to_request(
    payload: &[u8],
    telemetry: &PingReplyTelemetry,
) -> Result<Option<Vec<u8>>, PingCodecError> {
    let items = decode_ping_packet(payload)?;
    let Some(item) = items.iter().find(|item| !item.is_reply()) else {
        return Ok(None);
    };
    Ok(Some(encode_reply_for(
        item.timestamp,
        item.request_word,
        item.unreliable,
        telemetry,
    )))
}


// ---------------------------------------------------------------------------
// The responder: a bounded queue of requests, and the clock words a reply needs.
// ---------------------------------------------------------------------------

use std::collections::VecDeque;
use std::time::Instant;

/// Outgoing wire ids this client allocates, as `rove` does.
///
/// 2 and 4 are the two clock channels announced before login; 6 is the ping's.
/// The server's own announcements are odd ids and are for *inbound* datagrams —
/// this is the client's half of a naming scheme both ends follow.
pub const PING_WIRE_ID: u32 = 6;
pub const MAX_PENDING_PINGS: usize = 128;
pub const MAX_FLUSH_BATCH: usize = 64;

/// A monotonic millisecond clock, modulo `u32`.
///
/// **This is what the first three reply words are**: when the packet arrived,
/// when it was queued, and when it was written. They are not packet counts —
/// sending counts there makes the server's queue-delay estimate read a
/// multi-second delay for a session with no delay at all, which is why the
/// server stops trusting the client's timing.
#[derive(Clone, Copy, Debug)]
pub struct PingClock(Instant);

impl PingClock {
    pub fn new(origin: Instant) -> Self {
        Self(origin)
    }
    pub fn word(&self) -> u32 {
        self.0.elapsed().as_millis() as u32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplicationPingStats {
    /// The replication statistics word. Zero on the QUIC profile: upstream's
    /// `live_ping.rs` reports zero and documents the legacy RakNet fields as
    /// constructor-inactive, so zero here is the profile's own answer, not a
    /// placeholder for a number nobody collected.
    pub replication_word: u32,
    pub tail_words: [u32; 2],
}

/// Where the reply must be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryRoute {
    Reliable,
    Unreliable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrityStatusEvidence {
    pub status_word: u32,
    pub source: String,
    pub covered_producers: Vec<String>,
    pub missing_producers: Vec<String>,
}

impl IntegrityStatusEvidence {
    /// A word is only sendable with a named source and a complete inventory.
    /// Upstream's rule, kept: an empty claim is not a clean bill of health.
    pub fn complete(&self) -> bool {
        !self.source.trim().is_empty()
            && !self.covered_producers.is_empty()
            && self.missing_producers.is_empty()
    }
}

/// The observations a reply is built from. Every method can say "I do not know",
/// and a reply is blocked rather than filled in when one does.
pub trait PingTelemetryProvider {
    fn enqueue_clock_word(&mut self) -> Result<u32, &'static str>;
    fn write_clock_word(&mut self) -> Result<u32, &'static str>;
    fn replication_stats(&mut self) -> Result<ReplicationPingStats, &'static str>;
    fn integrity_status(&mut self) -> Result<IntegrityStatusEvidence, &'static str>;
}

/// One queued request and the observations taken when it arrived.
#[derive(Clone, Debug)]
struct PendingPing {
    id: u128,
    item: PingItem,
    received_clock_word: u32,
    enqueue_clock_word: u32,
    stats: ReplicationPingStats,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReceiveError {
    Codec(PingCodecError),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PingDiagnostics {
    pub requests_received: u64,
    pub responses_received: u64,
    pub replies_sent: u64,
    pub unsupported_requests: u64,
    pub queue_overflows: u64,
    pub telemetry_blocks: u64,
    pub telemetry_failed_requests: u64,
    /// Which provider blocked the reply: the clock words, the replication
    /// statistics, or the integrity inventory. Separated because the three
    /// have different causes and one line has to name the right one.
    pub clock_blocks: u64,
    pub statistics_blocks: u64,
    pub integrity_blocks: u64,
    pub send_failures: u64,
    pub encode_failures: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PrepareError {
    AlreadyInFlight,
    TelemetryUnavailable(&'static str),
    IntegrityEvidenceIncomplete,
    EncodeFailed,
    TokenExhausted,
}

/// A reply the transport owner may write now, and the token that says whether
/// it was written. Nothing is sampled between `prepare` and the write except
/// the write clock word itself, as upstream documents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedPing {
    token: u128,
    payload: Vec<u8>,
    route: DeliveryRoute,
}

impl PreparedPing {
    pub fn token(&self) -> u128 {
        self.token
    }
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
    pub fn route(&self) -> DeliveryRoute {
        self.route
    }
}

struct InFlight {
    token: u128,
    pending_id: u128,
}

/// A bounded queue of ping requests, answered once each, in order.
///
/// The bound and the one-in-flight rule are the point: a server that floods
/// requests must not grow this session's memory, and a reply whose write was
/// ambiguous must not be retried as if it were a second request.
pub struct PingResponder {
    pending: VecDeque<PendingPing>,
    max_pending: usize,
    diagnostics: PingDiagnostics,
    next_token: u128,
    next_pending_id: u128,
    in_flight: Option<InFlight>,
}

impl PingResponder {
    pub fn new(max_pending: usize) -> Result<Self, &'static str> {
        if !(1..=MAX_PENDING_PINGS).contains(&max_pending) {
            return Err("the ping queue bound must be 1..=128");
        }
        Ok(Self {
            pending: VecDeque::with_capacity(max_pending.min(8)),
            max_pending,
            diagnostics: PingDiagnostics::default(),
            next_token: 1,
            next_pending_id: 1,
            in_flight: None,
        })
    }

    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    pub fn diagnostics(&self) -> PingDiagnostics {
        self.diagnostics
    }

    /// The route of the reply at the head, before it is prepared: the transport
    /// owner needs it to know *which* send to call.
    pub fn front_route(&self) -> Option<DeliveryRoute> {
        self.pending.front().map(|pending| {
            if pending.item.unreliable {
                DeliveryRoute::Unreliable
            } else {
                DeliveryRoute::Reliable
            }
        })
    }

    /// Decode and validate a whole packet **before** touching the queue: a
    /// malformed packet must not half-enqueue.
    pub fn receive_packet<P: PingTelemetryProvider>(
        &mut self,
        payload: &[u8],
        received_clock_word: u32,
        providers: &mut P,
    ) -> Result<usize, ReceiveError> {
        let items = decode_ping_packet(payload).map_err(ReceiveError::Codec)?;
        Ok(self.receive_validated_items(&items, received_clock_word, providers))
    }

    /// The same, for requests that arrived already validated — `SessionRecord
    /// .pings`, which a committed packet carried. Nothing is decoded twice, and
    /// a post-globals ping is read from the record rather than the raw bytes.
    pub fn receive_records<P: PingTelemetryProvider>(
        &mut self,
        records: &[serde_json::Value],
        received_clock_word: u32,
        providers: &mut P,
    ) -> Result<usize, ReceiveError> {
        if records.len() > MAX_PING_ITEMS {
            return Err(ReceiveError::Codec(PingCodecError::TooManyItems));
        }
        let items = records
            .iter()
            .map(PingItem::from_record)
            .collect::<Result<Vec<_>, _>>()
            .map_err(ReceiveError::Codec)?;
        Ok(self.receive_validated_items(&items, received_clock_word, providers))
    }

    fn receive_validated_items<P: PingTelemetryProvider>(
        &mut self,
        items: &[PingItem],
        received_clock_word: u32,
        providers: &mut P,
    ) -> usize {
        let mut enqueued = 0;
        for item in items {
            if item.is_reply() {
                // The server answering us, or our own echo: not a request.
                self.diagnostics.responses_received += 1;
                continue;
            }
            self.diagnostics.requests_received += 1;
            if !item.supported_request() {
                // Flags this client does not collect. Answered, it would carry
                // telemetry nobody observed; counted, the report can say why.
                self.diagnostics.unsupported_requests += 1;
                continue;
            }
            if self.pending.len() >= self.max_pending {
                self.diagnostics.queue_overflows += 1;
                continue;
            }
            let stats = match providers.replication_stats() {
                Ok(stats) => stats,
                Err(_) => {
                    self.block(false);
                    self.diagnostics.statistics_blocks += 1;
                    continue;
                }
            };
            let enqueue_clock_word = match providers.enqueue_clock_word() {
                Ok(word) => word,
                Err(_) => {
                    self.block(true);
                    continue;
                }
            };
            let Some(next_id) = self.next_pending_id.checked_add(1) else {
                self.diagnostics.telemetry_failed_requests += 1;
                continue;
            };
            let id = self.next_pending_id;
            self.next_pending_id = next_id;
            self.pending.push_back(PendingPing {
                id,
                item: item.clone(),
                received_clock_word,
                enqueue_clock_word,
                stats,
            });
            enqueued += 1;
        }
        enqueued
    }

    fn block(&mut self, clock: bool) {
        self.diagnostics.telemetry_blocks += 1;
        self.diagnostics.telemetry_failed_requests += 1;
        if clock {
            self.diagnostics.clock_blocks += 1;
        }
    }

    /// Build the head's reply, or say why not. Called immediately before the
    /// write: the write clock word is sampled here and nowhere earlier.
    pub fn prepare<P: PingTelemetryProvider>(
        &mut self,
        providers: &mut P,
    ) -> Result<Option<PreparedPing>, PrepareError> {
        if self.in_flight.is_some() {
            return Err(PrepareError::AlreadyInFlight);
        }
        let Some(pending) = self.pending.front() else {
            return Ok(None);
        };
        let pending_id = pending.id;
        let item = pending.item.clone();
        let received_clock_word = pending.received_clock_word;
        let enqueue_clock_word = pending.enqueue_clock_word;
        let stats = pending.stats;
        let evidence = match providers.integrity_status() {
            Ok(evidence) => evidence,
            Err(reason) => {
                self.block(false);
                self.diagnostics.integrity_blocks += 1;
                // Dropped, not retried: the observation will not improve for
                // this request, and a queue that keeps it blocks the next one.
                self.pending.pop_front();
                return Err(PrepareError::TelemetryUnavailable(reason));
            }
        };
        if !evidence.complete() {
            self.block(false);
            self.diagnostics.integrity_blocks += 1;
            self.pending.pop_front();
            return Err(PrepareError::IntegrityEvidenceIncomplete);
        }
        let write_clock_word = match providers.write_clock_word() {
            Ok(word) => word,
            Err(reason) => {
                self.block(true);
                self.pending.pop_front();
                return Err(PrepareError::TelemetryUnavailable(reason));
            }
        };
        let telemetry = PingReplyTelemetry {
            // Upstream's order, and the whole point of this module:
            //   received, enqueued, written, replication, tail[0], tail[1]
            reply_words: [
                received_clock_word,
                enqueue_clock_word,
                write_clock_word,
                stats.replication_word,
                stats.tail_words[0],
                stats.tail_words[1],
            ],
            integrity_status: evidence.status_word,
        };
        let payload = match encode_ping_reply(&item, &telemetry) {
            Ok(payload) => payload,
            Err(_) => {
                self.diagnostics.encode_failures += 1;
                self.diagnostics.telemetry_failed_requests += 1;
                self.pending.pop_front();
                return Err(PrepareError::EncodeFailed);
            }
        };
        let token = self.next_token;
        let Some(next_token) = token.checked_add(1) else {
            return Err(PrepareError::TokenExhausted);
        };
        self.next_token = next_token;
        let route = if item.unreliable {
            DeliveryRoute::Unreliable
        } else {
            DeliveryRoute::Reliable
        };
        self.in_flight = Some(InFlight { token, pending_id });
        Ok(Some(PreparedPing {
            token,
            payload,
            route,
        }))
    }

    /// Complete an in-flight write. A failed write keeps the request queued for
    /// a retry; only a successful network write counts as a reply.
    pub fn commit(&mut self, token: u128, write_succeeded: bool) -> Result<bool, &'static str> {
        let Some(in_flight) = self.in_flight.as_ref() else {
            return Err("no ping write is in flight");
        };
        if in_flight.token != token {
            return Err("the ping write token is stale");
        }
        if self.pending.front().map(|pending| pending.id) != Some(in_flight.pending_id) {
            return Err("the ping queue changed while a write was in flight");
        }
        self.in_flight = None;
        if write_succeeded {
            self.pending.pop_front();
            self.diagnostics.replies_sent += 1;
            Ok(true)
        } else {
            self.diagnostics.send_failures += 1;
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain request, built the way a server builds one.
    fn request(timestamp: u64, request_word: u32, unreliable: bool) -> Vec<u8> {
        let mut bytes = vec![PING_HEADER, ITEM_REQUEST, 0];
        bytes.extend_from_slice(&timestamp.to_le_bytes());
        bytes.extend_from_slice(&request_word.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.push(u8::from(unreliable));
        bytes.push(0);
        bytes
    }

    const TELEMETRY: PingReplyTelemetry = PingReplyTelemetry {
        reply_words: [1, 2, 3, 4, 5, 6],
        integrity_status: 7,
    };

    #[test]
    fn a_plain_request_gets_a_reply_with_its_own_words_echoed() {
        let payload = request(0x1122_3344_5566_7788, 0x9abc_def0, false);
        let items = decode_ping_packet(&payload).expect("it is a ping");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, PingKind::Request);
        assert_eq!(items[0].flags, PingFlags::Basic);
        assert_eq!(items[0].timestamp, 0x1122_3344_5566_7788);
        assert_eq!(items[0].request_word, 0x9abc_def0);

        let reply = encode_ping_reply(&items[0], &TELEMETRY).expect("a request takes a reply");
        assert_eq!(&reply[..3], &[0x83, 6, 1], "rove's response prefix");
        assert_eq!(
            &reply[3..11],
            &0x1122_3344_5566_7788u64.to_le_bytes(),
            "the timestamp is echoed, not invented"
        );
        assert_eq!(&reply[11..15], &0x9abc_def0u32.to_le_bytes());
        // Six reply words, then integrity, then the response word, route, and
        // the item terminator: 3 + 8 + 4 + 24 + 4 + 4 + 1 + 1.
        assert_eq!(reply.len(), 49);
        assert_eq!(&reply[15..39], &{
            let mut words = Vec::new();
            for word in TELEMETRY.reply_words {
                words.extend_from_slice(&word.to_le_bytes());
            }
            words.into_boxed_slice()
        }[..]);
        assert_eq!(&reply[39..43], &7u32.to_le_bytes());
        assert_eq!(reply[44], 0, "a reliable request is answered reliably");
        assert_eq!(reply[48], 0, "and terminated");
    }

    #[test]
    fn the_response_word_follows_the_timestamps_bit_five() {
        // Bit 5 clear: zero. Set: all ones. rove's rule, and a server that
        // checks it rejects a constant either way.
        let plain = encode_ping_reply(&decode(&request(0x20 & 0, 0, false)), &TELEMETRY)
            .expect("a reply");
        assert_eq!(&plain[43..47], &0u32.to_le_bytes());
        let flagged =
            encode_ping_reply(&decode(&request(0x20, 0, false)), &TELEMETRY).expect("a reply");
        assert_eq!(&flagged[43..47], &u32::MAX.to_le_bytes());
    }

    fn decode(payload: &[u8]) -> PingItem {
        decode_ping_packet(payload)
            .expect("it is a ping")
            .into_iter()
            .next()
            .expect("one item")
    }

    #[test]
    fn a_flagged_request_is_not_answered() {
        // Flags 1 ("reply words") is a shape this client does not collect: the
        // packet is well-formed but the item is not a plain request, so it takes
        // no reply. A port that answered it would send telemetry it does not
        // have — which is the failure rove's responder exists to avoid.
        let mut payload = request(1, 2, false);
        payload[2] = 1;
        // The item itself is readable; there are simply six more words to skip,
        // which this packet does not carry.
        assert_eq!(
            decode_ping_packet(&payload).expect_err("flags 1 needs six more words"),
            PingCodecError::Truncated
        );
    }

    /// A flagged item that *is* complete is read, and refused a reply.
    #[test]
    fn a_complete_flagged_item_gets_no_reply() {
        let mut payload = vec![PING_HEADER, ITEM_REQUEST, 1];
        payload.extend_from_slice(&0u64.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        for word in 1..=6u32 {
            payload.extend_from_slice(&word.to_le_bytes());
        }
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.push(0);
        payload.push(0);
        let items = decode_ping_packet(&payload).expect("six reply words are present");
        assert_eq!(items[0].flags, PingFlags::ReplyWords);
        assert_eq!(
            encode_ping_reply(&items[0], &TELEMETRY).expect_err("not a plain request"),
            PingCodecError::NotARequest
        );
    }

    #[test]
    fn a_reply_packet_gets_no_answer() {
        // A response item: it is the server answering *us*, or our own echo.
        let mut payload = vec![PING_HEADER, ITEM_RESPONSE, 0];
        payload.extend_from_slice(&0u64.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.push(0);
        payload.push(0);
        assert_eq!(
            reply_to_request(&payload, &TELEMETRY).expect("a valid packet"),
            None,
            "a reply is not answered: that would be a loop"
        );
    }

    #[test]
    fn a_packet_that_is_not_a_ping_is_refused_with_a_reason() {
        // 0x84 is not the header. (0x83 *is*, which is 131 — see PING_HEADER.)
        assert_eq!(
            decode_ping_packet(&[0x84, 0, 0, 0]).expect_err("not a ping"),
            PingCodecError::InvalidHeader
        );
        assert_eq!(
            decode_ping_packet(&[PING_HEADER, ITEM_REQUEST, 0]).expect_err("truncated"),
            PingCodecError::Truncated
        );
        // A terminated packet with bytes after the terminator.
        assert_eq!(
            decode_ping_packet(&[PING_HEADER, 0, 1]).expect_err("trailing"),
            PingCodecError::TrailingBytes
        );
        // No terminator at all: a header and nothing else.
        assert_eq!(
            decode_ping_packet(&[PING_HEADER]).expect_err("unterminated"),
            PingCodecError::MissingTerminator
        );
        // A header and a terminator is a well-formed packet with no items — the
        // server's "nothing to say", not an error.
        assert_eq!(decode_ping_packet(&[PING_HEADER, 0]).expect("empty"), Vec::new());
    }

    #[test]
    fn several_requests_are_all_read_and_the_first_answered() {
        let mut payload = vec![PING_HEADER];
        for timestamp in [10u64, 20] {
            payload.push(ITEM_REQUEST);
            payload.push(0);
            payload.extend_from_slice(&timestamp.to_le_bytes());
            payload.extend_from_slice(&1u32.to_le_bytes());
            payload.extend_from_slice(&0u32.to_le_bytes());
            payload.extend_from_slice(&0u32.to_le_bytes());
            payload.push(0);
        }
        payload.push(0);
        let items = decode_ping_packet(&payload).expect("two requests");
        assert_eq!(items.len(), 2);
        let reply = reply_to_request(&payload, &TELEMETRY)
            .expect("valid")
            .expect("a reply");
        assert_eq!(
            &reply[3..11],
            &10u64.to_le_bytes(),
            "the first request's timestamp"
        );
    }
}
