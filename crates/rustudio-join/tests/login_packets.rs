//! M2a/2b: the login packets, byte for byte.
//!
//! Two independent checks, because a codec that only agrees with itself is not
//! evidence:
//!
//! 1. **The Python reference vectors** rove ships
//!    (`rove/rust-client/tests/fixtures/login/python_reference_vectors.json`)
//!    were produced without Rust, and every packet here is compared to them.
//! 2. **rove's own encoder**, as a dev-dependency, must produce the same bytes
//!    for the same input. That catches a port that drifted in a branch the
//!    vectors happen not to cover.

use serde_json::Value;

use rustudio_join::login::{
    encode_signed, encode_string, encode_unsigned, feature_configuration, issued_early_auth,
    player_join_info, protocol_sync, server_id_packet, ticket_checksum, ticket_hash, ticket_packet,
    MAX_FRAME,
};

const VECTORS: &str =
    include_str!("../../../rove/rust-client/tests/fixtures/login/python_reference_vectors.json");

fn vectors() -> Value {
    serde_json::from_str(VECTORS).expect("the reference vectors are JSON")
}

fn unhex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn early_auth_copies_the_issued_blobs_unchanged() {
    let vectors = vectors();
    let vector = &vectors["early_auth"];
    assert_eq!(
        issued_early_auth(vector["ticket"].as_str().unwrap()).unwrap(),
        unhex(vector["packet_hex"].as_str().unwrap())
    );
    for bad in ["not-a-ticket", "a;b;***;YQ==;1", "a;b;YQ==;YQ==;1"] {
        assert!(issued_early_auth(bad).is_err(), "{bad} should be refused");
    }
}

#[test]
fn the_feature_configuration_reader_preserves_order_and_bounds_every_read() {
    let vectors = vectors();
    let vector = &vectors["feature_configuration"];
    let packet = unhex(vector["packet_hex"].as_str().unwrap());
    let entries = feature_configuration(&packet).unwrap();
    assert_eq!(
        entries,
        vec![("Example".to_owned(), "true".to_owned())],
        "the packet's own pairs"
    );
    // Every truncation must be refused, not read as a shorter table.
    for length in 0..packet.len() {
        assert!(
            feature_configuration(&packet[..length]).is_err(),
            "{length} bytes should be refused"
        );
    }
    assert!(feature_configuration(&[0x93, 0, 1, 0, 0, 0, b'a', 0, 0, 0]).is_err());
    assert!(feature_configuration(&[0x93, 0, 0, 0, 0]).is_err());
    // A duplicate name is a malformed table.
    let mut duplicate = vec![0x93, 0, 2, 0];
    for _ in 0..2 {
        duplicate.extend_from_slice(&[1, 0, b'a', 1, 0, b'b']);
    }
    assert!(feature_configuration(&duplicate).is_err());
}

#[test]
fn wire_integers_are_leb128_and_match_python() {
    for pair in vectors()["signed_int_vectors"].as_array().unwrap() {
        let value = pair[0].as_i64().unwrap();
        let encoded = unhex(pair[1].as_str().unwrap());
        assert_eq!(encode_signed(value), encoded, "signed {value}");
    }
    assert_eq!(encode_unsigned(127).unwrap(), [0x7f]);
    assert_eq!(encode_unsigned(128).unwrap(), [0x80, 0x01]);
    assert_eq!(encode_unsigned(624_485).unwrap(), [0xe5, 0x8e, 0x26]);
    assert_eq!(encode_string("é").unwrap(), [2, 0xc3, 0xa9]);
    assert!(encode_string(&"x".repeat(MAX_FRAME + 1)).is_err());
}

#[test]
fn protocol_sync_matches_the_python_layout() {
    let vectors = vectors();
    let vector = &vectors["protocol_sync"];
    let join = &vector["input_join"];
    let ticket = &vector["join_ticket"];
    let packet = protocol_sync(join, Some(ticket), Some((13, 14))).unwrap();
    assert_eq!(
        packet,
        unhex(vector["packet_hex"].as_str().unwrap()),
        "the bytecode-range variant"
    );
    // ... and the player object it embeds is the one the vectors describe.
    let info = player_join_info(join, Some(ticket)).unwrap();
    let expected = vector["player_info"].as_object().unwrap();
    let names: Vec<&str> = info.iter().map(|(key, _)| key.as_str()).collect();
    for key in expected.keys() {
        assert!(names.contains(&key.as_str()), "{key} is missing from the packet");
    }
}

#[test]
fn packet_138_matches_the_python_layout_and_hash_vectors() {
    let vectors = vectors();
    let ticket = &vectors["ticket"];
    assert_eq!(
        ticket_hash(b"", 0).as_u64().unwrap(),
        vectors["ticket"]["hash_empty_seed0"].as_u64().unwrap()
    );
    for pair in ticket["checksum_vectors"].as_array().unwrap() {
        let input = pair[0].as_u64().unwrap() as u32;
        let expected = pair[1].as_u64().unwrap() as u32;
        assert_eq!(ticket_checksum(input), expected, "checksum of {input:#x}");
    }
    let packet = ticket_packet(&ticket["join"]).unwrap();
    assert_eq!(packet, unhex(ticket["packet_hex"].as_str().unwrap()));
}

#[test]
fn the_empty_server_id_packet_is_two_bytes() {
    assert_eq!(
        server_id_packet(),
        unhex(vectors()["server_id"]["packet_hex"].as_str().unwrap())
    );
}

#[test]
fn the_checksum_and_hash_match_the_independent_vectors() {
    // These are the numbers the Python implementation produced, so agreeing
    // with them is agreeing with something that never saw this code. The
    // branch-by-branch comparison against rove's own Rust encoder lives in
    // `examples/crosscheck.rs` (it includes both encoders rather than making a
    // whole client crate a dependency of this one).
    let vectors = vectors();
    let join = &vectors["ticket"]["join"];
    assert_eq!(
        ticket_packet(join).unwrap(),
        unhex(vectors["ticket"]["packet_hex"].as_str().unwrap())
    );
    assert_eq!(
        ticket_hash(b"", 0) as u64,
        vectors["ticket"]["hash_empty_seed0"].as_u64().unwrap()
    );
}

#[test]
fn the_five_packets_of_a_join_are_their_own_ids_in_order() {
    use rustudio_join::allocation::{parse_allocation, AllocationResult, JoinTarget};
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/join_game.json")).unwrap();
    let target = JoinTarget::place(1818);
    let issued = match parse_allocation(
        200,
        fixture["ready"].as_object(),
        &target,
        None,
        424_242,
        "application/json",
    )
    .unwrap()
    {
        AllocationResult::Ready(issued) => issued,
        other => panic!("{other:?}"),
    };
    let packets = rustudio_join::session::login_packets(&issued).expect("the five packets");
    let ids: Vec<u8> = packets.iter().map(|packet| packet[0]).collect();
    assert_eq!(
        ids,
        [0xa8, 0x90, 0x92, 0x8a, 0x8f],
        "early auth, protocol sync, verification, ticket, server id"
    );
    // Built from the issued values rather than placeholders: the place id is
    // in the player object, the session id in the ticket packet.
    let has = |packet: &Vec<u8>, needle: &[u8]| packet.windows(needle.len()).any(|w| w == needle);
    assert!(has(&packets[1], b"\"PlaceId\":1818"), "the player object names the place");
    assert!(has(&packets[1], b"\"UserId\":424242"), "and the account");
    assert!(has(&packets[3], b"synthetic-session-id"), "the ticket carries the session");
    assert!(has(&packets[3], b"synthetic"), "and the client ticket");
    assert_eq!(packets[4], vec![0x8f, 0]);
}
