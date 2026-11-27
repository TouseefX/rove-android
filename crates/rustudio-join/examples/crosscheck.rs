//! The ported login codec against rove's own, in one binary.
//!
//! Why an example and not a test: the reference is `rove/rust-client`'s
//! `login.rs`, and making that crate a dev-dependency of this one pulls a whole
//! client target into the test graph — which changes cargo's feature resolution
//! for the workspace (its `ureq` chain reaches `tungstenite`'s native-tls, and
//! that is not in the lock). So the reference is `include!`d here instead: same
//! crate, same module graph, no dependency arc. Two `login` modules appear side
//! by side, which is exactly what a cross-check should look like.
//!
//! Run it with `cargo run -p rustudio-join --example crosscheck`. It prints one
//! line per check and exits non-zero on the first disagreement, so CI can gate
//! on it.

// The reference, compiled as it ships. `login.rs` is a plain module with one
// outside reference of its own (`crate::allocation::new_join_attempt_id`, for
// the verification nonce), so `allocation` here is a shim onto the port's own —
// which makes that one path a shared implementation rather than a comparison.
// The verification packet is therefore checked against the *vector* below, not
// against the reference; everything else is compared byte for byte.
mod reference {
    pub mod allocation {
        pub fn new_join_attempt_id() -> Result<String, &'static str> {
            crate::port::allocation::new_join_attempt_id().map_err(|_| "no entropy")
        }
    }
    #[allow(dead_code, unused_imports)]
    pub mod login {
        include!("../../../rove/rust-client/src/login.rs");
    }
}

use rustudio_join::login as port;

const VECTORS: &str = include_str!("../../../rove/rust-client/tests/fixtures/login/python_reference_vectors.json");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let vectors: serde_json::Value = serde_json::from_str(VECTORS)?;
    let mut checks = 0usize;
    let mut compare = |what: &str, mine: Vec<u8>, theirs: Vec<u8>| {
        checks += 1;
        if mine != theirs {
            eprintln!(
                "DISAGREE {what}\n  port:      {} bytes {}\n  reference: {} bytes {}",
                mine.len(),
                hex(&mine),
                theirs.len(),
                hex(&theirs)
            );
            std::process::exit(1);
        }
        println!("  agree  {what} ({} bytes)", mine.len());
    };

    // The five packets of a join, from the vectors' own join document.
    let join = &vectors["protocol_sync"]["input_join"];
    let ticket = &vectors["protocol_sync"]["join_ticket"];
    for range in [None, Some((13, 14)), Some((6, 14))] {
        compare(
            &format!("protocol_sync{range:?}"),
            port::protocol_sync(join, Some(ticket), range)?,
            reference::login::protocol_sync(join, Some(ticket), range)?,
        );
    }
    let ticket_join = &vectors["ticket"]["join"];
    compare(
        "ticket_packet",
        port::ticket_packet(ticket_join)?,
        reference::login::ticket_packet(ticket_join)?,
    );
    let early = vectors["early_auth"]["ticket"].as_str().unwrap();
    compare(
        "issued_early_auth",
        port::issued_early_auth(early)?,
        reference::login::issued_early_auth(early)?,
    );
    compare(
        "server_id_packet",
        port::server_id_packet(),
        reference::login::server_id_packet(),
    );

    // The codec beneath them, over a spread of magnitudes.
    for value in [
        0i64,
        -1,
        1,
        -2,
        127,
        128,
        -128,
        i64::MIN,
        i64::MAX,
        1_234_567_890,
        -987_654_321,
    ] {
        compare(
            &format!("encode_signed({value})"),
            port::encode_signed(value),
            reference::login::encode_signed(value),
        );
    }
    for value in [0u64, 1, 127, 128, 624_485, u64::MAX / 3, u64::MAX] {
        compare(
            &format!("encode_unsigned({value})"),
            port::encode_unsigned(value)?,
            reference::login::encode_unsigned(value)?,
        );
    }
    for text in ["", "a", "é", "a longer string with spaces", "🤖"] {
        compare(
            &format!("encode_string({text:?})"),
            port::encode_string(text)?,
            reference::login::encode_string(text)?,
        );
    }
    for input in [0u32, 1, 0x1234_5678, u32::MAX] {
        checks += 1;
        if port::ticket_checksum(input) != reference::login::ticket_checksum(input) {
            eprintln!("DISAGREE ticket_checksum({input:#x})");
            std::process::exit(1);
        }
        if port::ticket_hash(&input.to_le_bytes(), 1) != reference::login::ticket_hash(&input.to_le_bytes(), 1) {
            eprintln!("DISAGREE ticket_hash({input:#x})");
            std::process::exit(1);
        }
    }
    println!("  agree  ticket_checksum and ticket_hash over four inputs");

    // The one packet whose reference shares this crate's nonce source: checked
    // against rove's own recorded vectors instead (`login.rs`'s test module).
    for (nonce, expected) in [
        (0u32, "92909ebee101"),
        (1, "92929ebee121"),
        (0x8000_0000, "92efe1c19eeeffffffff01"),
        (u32::MAX, "92919ebee101"),
    ] {
        checks += 1;
        let mine = port::verification_with_nonce_for_check(nonce);
        let expected: Vec<u8> = expected
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        if mine != expected {
            eprintln!("DISAGREE verification_with_nonce({nonce}) {}", hex(&mine));
            std::process::exit(1);
        }
    }
    println!("  agree  the verification packet's four recorded nonces");

    println!("{checks} cross-checks, no disagreement");
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
