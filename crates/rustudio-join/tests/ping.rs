//! The ported ping codec against rove's own, byte for byte.
//!
//! `ping.rs` is a port, so the question a port has to answer is not "does it
//! work" but "does it agree". The reference is loaded by path rather than as a
//! dependency: `rove/rust-client` is a whole client target, and adding it to
//! this crate's graph changes cargo's feature resolution for the workspace (see
//! `examples/crosscheck.rs`, which does the same for the login encoder).
//!
//! `ping_responder.rs` refers to nothing outside `rove_client_core::wire` and
//! `std`, so a `#[path]` module is enough — no shim, unlike `login.rs`.
//!
//! What is compared: the decode of a set of packets (every flag shape, a
//! multi-item packet, and the malformed ones), and the encoding of a reply for
//! each request. A disagreement is printed with both sides in hex, because the
//! number of bytes that differ is not the useful fact — which byte is.

#[path = "../../../rove/rust-client/src/ping_responder.rs"]
#[allow(dead_code)]
mod reference;

use rustudio_join::ping as port;

/// A plain request, built the way a server builds one.
fn request(timestamp: u64, request_word: u32, unreliable: bool) -> Vec<u8> {
    let mut bytes = vec![port::PING_HEADER, 5, 0];
    bytes.extend_from_slice(&timestamp.to_le_bytes());
    bytes.extend_from_slice(&request_word.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.push(u8::from(unreliable));
    bytes.push(0);
    bytes
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

const TELEMETRY: port::PingReplyTelemetry = port::PingReplyTelemetry {
    reply_words: [11, 22, 33, 44, 55, 66],
    integrity_status: 0x1234_5678,
};

/// The same telemetry in the reference's type.
fn reference_telemetry() -> reference::PingReplyTelemetry {
    reference::PingReplyTelemetry {
        reply_words: [11, 22, 33, 44, 55, 66],
        integrity_status: 0x1234_5678,
    }
}

/// The reference's item fields are private, so the comparison is `Debug` text.
///
/// That is not a weaker check: both types derive `Debug` with the same field
/// names in the same order (`kind: Request, flags: Basic, timestamp: ...,
/// request_word: ..., reply_words: ..., trailer_words: ..., unreliable: ...,
/// extra_word: ...`) and the same enum variant names, so any field that
/// disagrees changes the string. It also prints both sides in the failing
/// assertion, which is what a port needs — and it does not depend on the
/// reference's fields being public.
#[test]
fn both_decoders_read_a_packet_the_same_way() {
    let packets = [
        // A plain request, reliable and unreliable.
        request(0x1122_3344_5566_7788, 0x9abc_def0, false),
        request(1, 2, true),
        // A timestamp with bit 5 set, which changes the reply's response word.
        request(0x20, 7, false),
        // Two requests in one packet.
        {
            let mut payload = vec![port::PING_HEADER];
            for timestamp in [10u64, 20] {
                payload.push(5);
                payload.push(0);
                payload.extend_from_slice(&timestamp.to_le_bytes());
                payload.extend_from_slice(&1u32.to_le_bytes());
                payload.extend_from_slice(&0u32.to_le_bytes());
                payload.extend_from_slice(&0u32.to_le_bytes());
                payload.push(0);
            }
            payload.push(0);
            payload
        },
        // An empty packet: a header and a terminator.
        vec![port::PING_HEADER, 0],
    ];
    for packet in &packets {
        let mine = port::decode_ping_packet(packet).unwrap_or_else(|error| {
            panic!("the port refused {}: {error}", hex(packet));
        });
        let theirs = reference::decode_ping_packet(packet).unwrap_or_else(|error| {
            panic!("rove refused {}: {error:?}", hex(packet));
        });
        assert_eq!(mine.len(), theirs.len(), "item count for {}", hex(packet));
        for (mine, theirs) in mine.iter().zip(theirs.iter()) {
            assert_eq!(
                format!("{mine:?}"),
                format!("{theirs:?}"),
                "item disagrees for {}",
                hex(packet)
            );
        }
    }
}

#[test]
fn both_encoders_write_the_same_reply() {
    let telemetry = reference_telemetry();
    for packet in [
        request(0x1122_3344_5566_7788, 0x9abc_def0, false),
        request(0x20, 0, true),
        request(0, u32::MAX, false),
    ] {
        let mine = port::decode_ping_packet(&packet).expect("the port reads it");
        let theirs = reference::decode_ping_packet(&packet).expect("rove reads it");
        let mine = port::encode_ping_reply(&mine[0], &TELEMETRY).expect("the port answers");
        let theirs =
            reference::encode_ping_reply(&theirs[0], &telemetry).expect("rove answers");
        assert_eq!(
            hex(&mine),
            hex(&theirs),
            "reply to {} disagrees",
            hex(&packet)
        );
    }
}

#[test]
fn both_refuse_the_same_packets() {
    // The malformed shapes: no header, no terminator, trailing bytes, a
    // truncated item, an unknown item kind, bad flags, a bad route.
    let malformed: [Vec<u8>; 7] = [
        vec![0x84, 0],             // 0x84 is not the ping header (0x83 is)
        vec![port::PING_HEADER],   // no terminator
        vec![port::PING_HEADER, 0, 1], // bytes after the terminator
        vec![port::PING_HEADER, 5, 0], // truncated item
        vec![port::PING_HEADER, 7, 0, 0], // item kind 7
        {
            let mut bad = request(1, 2, false);
            bad[2] = 3; // flags 3 is not a ping shape
            bad
        },
        {
            let mut bad = request(1, 2, false);
            let route = bad.len() - 2;
            bad[route] = 9; // a route that is neither reliable nor unreliable
            bad
        },
    ];
    for packet in &malformed {
        let mine = port::decode_ping_packet(packet);
        let theirs = reference::decode_ping_packet(packet);
        assert!(
            mine.is_err(),
            "the port accepted {}",
            hex(packet)
        );
        assert_eq!(
            theirs.is_err(),
            mine.is_err(),
            "disagreement on {}: port {:?}, rove {:?}",
            hex(packet),
            mine.err(),
            theirs.err()
        );
        // The *variant* has to match too: a port that refuses for a different
        // reason than the reference is a port with a different rule.
        assert_eq!(
            format!("{:?}", mine.unwrap_err()),
            format!("{:?}", theirs.unwrap_err()),
            "the refusal reason for {}",
            hex(packet)
        );
    }
}

#[test]
fn the_port_and_rove_agree_on_what_gets_no_reply() {
    // A response packet: rove's `supported_request` refuses it, and so must the
    // port, or a client would answer its own echo forever.
    let mut response = vec![port::PING_HEADER, 6, 0];
    response.extend_from_slice(&0u64.to_le_bytes());
    response.extend_from_slice(&0u32.to_le_bytes());
    response.extend_from_slice(&0u32.to_le_bytes());
    response.extend_from_slice(&0u32.to_le_bytes());
    response.push(0);
    response.push(0);
    let item = port::decode_ping_packet(&response).expect("the port reads it");
    let reference_item =
        reference::decode_ping_packet(&response).expect("rove reads it");
    assert!(port::encode_ping_reply(&item[0], &TELEMETRY).is_err());
    assert!(
        reference::encode_ping_reply(&reference_item[0], &reference_telemetry()).is_err()
    );
    // And `reply_to_request`, which is the port's addition, declines it too.
    assert_eq!(
        port::reply_to_request(&response, &TELEMETRY).expect("a valid packet"),
        None
    );
}
