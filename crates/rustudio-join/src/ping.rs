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

/// The packet header every ping starts with. **131**, not `0x83`: rove's own
/// `PingCounts::from_packet` refuses anything else, and `0x83` is what a *reply*
/// to the marker handshake starts with — the two are easy to confuse, and
/// reading one as the other is a silent no-op.
const HEADER: u8 = 131;
const ITEM_REQUEST: u8 = 5;
const ITEM_RESPONSE: u8 = 6;
/// The reply's item kind/sub pair, as rove writes it.
const REPLY_PREFIX: [u8; 3] = [HEADER, ITEM_RESPONSE, 1];

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
    NotARequest,
}

impl std::fmt::Display for PingCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PacketTooLarge => write!(f, "the packet is too large to be a ping"),
            Self::InvalidHeader => write!(f, "it does not start 131"),
            Self::InvalidKind(kind) => write!(f, "item kind {kind} is not a ping"),
            Self::InvalidFlags(bits) => write!(f, "flags {bits:#x} are not a ping's"),
            Self::InvalidRoute(route) => write!(f, "route {route} is not reliable or unreliable"),
            Self::TooManyItems => write!(f, "more items than a ping may carry"),
            Self::MissingTerminator => write!(f, "it has no item terminator"),
            Self::TrailingBytes => write!(f, "bytes follow the terminator"),
            Self::Truncated => write!(f, "it ends mid-item"),
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
    if reader.u8().map_err(|_| PingCodecError::Truncated)? != HEADER {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain request, built the way a server builds one.
    fn request(timestamp: u64, request_word: u32, unreliable: bool) -> Vec<u8> {
        let mut bytes = vec![HEADER, ITEM_REQUEST, 0];
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
        assert_eq!(&reply[..3], &[0x83, 6, 1], "a reply starts 0x83, not 131");
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
        // Flags 1 (reply words) asks for six words in the request, which this
        // packet does not carry — so it is malformed rather than supportable.
        let mut payload = request(1, 2, false);
        payload[2] = 1;
        let error = decode_ping_packet(&payload).expect_err("flags 1 needs reply words");
        assert_eq!(error, PingCodecError::Truncated);
    }

    #[test]
    fn a_reply_packet_gets_no_answer() {
        // A response item: it is the server answering *us*, or our own echo.
        let mut payload = vec![HEADER, ITEM_RESPONSE, 0];
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
        assert_eq!(
            decode_ping_packet(&[0x83, 0, 0, 0]).expect_err("0x83 is a reply, not a ping"),
            PingCodecError::InvalidHeader
        );
        assert_eq!(
            decode_ping_packet(&[HEADER, ITEM_REQUEST, 0]).expect_err("truncated"),
            PingCodecError::Truncated
        );
        // A terminated packet with bytes after the terminator.
        assert_eq!(
            decode_ping_packet(&[HEADER, 0, 1]).expect_err("trailing"),
            PingCodecError::TrailingBytes
        );
        // No terminator at all: a header and nothing else.
        assert_eq!(
            decode_ping_packet(&[HEADER]).expect_err("unterminated"),
            PingCodecError::MissingTerminator
        );
        // A header and a terminator is a well-formed packet with no items — the
        // server's "nothing to say", not an error.
        assert_eq!(decode_ping_packet(&[HEADER, 0]).expect("empty"), Vec::new());
    }

    #[test]
    fn several_requests_are_all_read_and_the_first_answered() {
        let mut payload = vec![HEADER];
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
