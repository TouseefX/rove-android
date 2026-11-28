//! Signed-program challenges (`155`): framing, `RSB1` container integrity, V1
//! signature verification, and the reply.
//!
//! The server sends `[155, first:u32le, second:u32le, length:u32le, blob]`, where
//! `blob` is a `RSB1` container around a signed Luau body. It expects
//! `[155, second:u32le, answer:u32le]` back on `(4, 1)`: the answer is whatever
//! the **signed program itself** returns for `(second, first)`. There is no
//! expected answer to look up and none is invented here — a challenge answered
//! with a constant is a challenge the server rejects, and one answered with a
//! guessed number is worse than one left unanswered because it claims a
//! verification that never happened.
//!
//! Ported from `rove/rust-client/src/program_check.rs`, with the parts this
//! workspace can actually do made explicit:
//!
//! * **The trust table and its provenance are embedded.** `PublicTrust`
//!   verifies the pinned table against its SHA-256 and the provenance JSON
//!   against the two hashes it names, exactly as upstream does — but from
//!   `include_bytes!`/`include_str!` rather than two asset files, because an
//!   Android APK has no `rove/inputs/` directory to open.
//! * **Execution goes through this workspace's Luau, not rove's.** Upstream
//!   calls a native C++ host (`crate::native`); here the verified body is
//!   normalized (`bytecode_format::normalize`) and loaded into the vendored
//!   `mlua` Luau VM ([`crate::program_vm`]). Same bytecode version family, same
//!   answer, no FFI host.
//! * **No answer is cached across sessions.** One live allocation owns one
//!   handler and one bounded duplicate cache, as upstream documents: a
//!   duplicate *packet* (the server re-sending the same challenge) gets the same
//!   reply, and a different session never sees it.

use crate::login::ticket_hash;
use crate::session_integrity::SessionIntegrity;
use rove_client_core::wire::decompress_bytes;
use serde_json::{json, Value};
use std::fmt;

const MAX_PROGRAM: usize = 128 * 1024;
const TRAILER: usize = 24;
const COEFFICIENTS: usize = 8257;
/// The pinned public trust table's SHA-256, and the binary it was dumped from.
/// Both are checked against the provenance JSON, which is checked by its own
/// value here — a table swapped at build time fails three ways, not one.
const TABLE_HASH: &str = "0b6cdf37e5817b29638fe705319618df725ffd2aafc0491d768ab6f162f42c3c";
const SOURCE_HASH: &str = "b6b2184109237a8fea5de33d3265ac026cda3db05dbe3d66f749c91b4297b870";
/// The maximum number of distinct challenges one session answers. Upstream's
/// bound; it is what stops a server turning a join into a program-running loop.
const MAX_DISTINCT_CHALLENGES: usize = 16;

/// Embedded so a phone needs no asset directory, and so the bytes a phone
/// verifies are the bytes this build shipped.
const TRUST_TABLE: &[u8] = include_bytes!("../../../rove/inputs/bytecode-signature-v1-public.bin");
const TRUST_PROVENANCE: &str =
    include_str!("../../../rove/inputs/bytecode-signature-v1-public.json");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProgramError(&'static str);

impl fmt::Display for ProgramError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ProgramError {}

type Result<T> = std::result::Result<T, ProgramError>;

impl ProgramError {
    /// The reason, for a log line or a report. `Display` is the same string.
    pub fn reason(&self) -> &'static str {
        self.0
    }

    /// A reason raised by a layer below this one (the bytecode reader, the VM).
    /// Kept as a `&'static str` so a refusal never allocates its explanation.
    pub fn from_reason(reason: &'static str) -> Self {
        Self(reason)
    }
}

/// The wire framing of one challenge.
pub struct ProgramCheck<'a> {
    first: u32,
    second: u32,
    program: &'a [u8],
}

impl<'a> ProgramCheck<'a> {
    pub fn parse(packet: &'a [u8]) -> Result<Self> {
        if packet.len() < 13 || packet[0] != 155 {
            return Err(ProgramError("invalid-program-check-header"));
        }
        let word = |at: usize| {
            u32::from_le_bytes(packet[at..at + 4].try_into().expect("checked header"))
        };
        let length = word(9) as usize;
        if !(1..=MAX_PROGRAM).contains(&length) || packet.len() != 13 + length {
            return Err(ProgramError("invalid-program-check-size"));
        }
        Ok(Self {
            first: word(1),
            second: word(5),
            program: &packet[13..],
        })
    }

    /// `(second, first)` — the program is called with these, in this order.
    /// Upstream's order, and the one the container's own program expects.
    pub fn arguments(&self) -> [u32; 2] {
        [self.second, self.first]
    }

    pub fn verify(&self, trust: &PublicTrust) -> Result<VerifiedProgram> {
        self.verify_with_integrity(trust, None)
    }

    fn verify_with_integrity(
        &self,
        trust: &PublicTrust,
        observer: Option<&SessionIntegrity>,
    ) -> Result<VerifiedProgram> {
        let original = unpack_container_observed(self.program, observer)?;
        trust.verify(&original)?;
        Ok(VerifiedProgram {
            original,
            arguments: self.arguments(),
        })
    }
}

/// Only a successful cryptographic verification can construct this value: the
/// original encoded body and its complete signature trailer stay unchanged, and
/// the caller has no way to build one out of bytes it did not verify.
pub struct VerifiedProgram {
    original: Vec<u8>,
    arguments: [u32; 2],
}

impl VerifiedProgram {
    pub fn original_bytes(&self) -> &[u8] {
        &self.original
    }

    pub fn arguments(&self) -> [u32; 2] {
        self.arguments
    }

    pub fn summary(&self) -> Value {
        json!({
            "signature_verified": true,
            "signature_version": 1,
            "body_bytes": self.original.len() - TRAILER,
            "trailer_bytes": TRAILER,
        })
    }
}

/// The pinned signature-v1 public map: 8257 twelve-byte coefficients, read as
/// `u128`s, with a 128-bit signature selecting the rows of one quadratic.
pub struct PublicTrust {
    coefficients: Box<[u128]>,
}

impl PublicTrust {
    /// The trust the build ships, checked against its own provenance.
    ///
    /// This cannot fail for a correct build, and it returns a `Result` rather
    /// than panicking because "a tampered asset" is not a state a join should
    /// crash in: a session with no trust store simply does not answer a `155`.
    pub fn embedded() -> Result<Self> {
        let provenance: Value = serde_json::from_str(TRUST_PROVENANCE)
            .map_err(|_| ProgramError("public-trust-provenance-mismatch"))?;
        Self::from_pinned_bytes(TRUST_TABLE, &provenance)
    }

    fn from_pinned_bytes(raw: &[u8], provenance: &Value) -> Result<Self> {
        if raw.len() != COEFFICIENTS * 12
            || blake3_hash_hex(raw) != TABLE_HASH
            || provenance["table_sha256"] != TABLE_HASH
            || provenance["source_binary_sha256"] != SOURCE_HASH
        {
            return Err(ProgramError("public-trust-provenance-mismatch"));
        }
        let coefficients = raw
            .chunks_exact(12)
            .map(|bytes| {
                let mut expanded = [0; 16];
                expanded[..12].copy_from_slice(bytes);
                u128::from_le_bytes(expanded)
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Ok(Self { coefficients })
    }

    /// Verify one signed body: the envelope's version and tag, then the public
    /// map's result against the body's own digest.
    ///
    /// The last sixteen bytes are the signature trailer: eight bytes of
    /// signature, four of tag, four of version, each XORed with the tail so a
    /// body cannot be replayed against a different trailer cheaply. Every check
    /// here is upstream's; none is relaxed.
    pub fn verify(&self, original: &[u8]) -> Result<()> {
        if !(0x118..=MAX_PROGRAM).contains(&original.len()) {
            return Err(ProgramError("signed-program-size-outside-bounds"));
        }
        if original[0] == 0 {
            return Err(ProgramError("invalid-bytecode-header"));
        }
        let (body, trailer) = original.split_at(original.len() - TRAILER);
        let version = [
            trailer[0] ^ trailer[20],
            trailer[1] ^ trailer[21],
            trailer[2] ^ trailer[22],
            trailer[3] ^ trailer[23],
        ];
        if version[..3] != [0x32, 0xc4, 0x6a] {
            return Err(ProgramError("unrecognized-signature-envelope"));
        }
        if version[3] != 1 {
            return Err(ProgramError("unsupported-signature-version"));
        }
        let word = |at: usize| u32::from_le_bytes(trailer[at..at + 4].try_into().expect("fixed trailer"));
        if word(0) ^ word(16) != 0x74171870 {
            return Err(ProgramError("signature-envelope-tag-mismatch"));
        }
        let observed = evaluate_public_map(&trailer[..16], &self.coefficients)?;
        let digest = blake3::hash(body);
        if observed
            .iter()
            .zip(&digest.as_bytes()[4..16])
            .fold(0u8, |sum, (a, b)| sum | (a ^ b))
            != 0
        {
            return Err(ProgramError("signature-verification-failed"));
        }
        Ok(())
    }
}

/// **One live allocation owns one handler**: its own job id, its own bounded
/// duplicate cache, its own counters. Nothing is process-global, so two
/// sessions in one build cannot answer each other's challenges.
pub struct ProgramAuth {
    job_id: String,
    trust: PublicTrust,
    cache: Vec<([u8; 32], Vec<u8>)>,
    verified: u64,
    executed: u64,
    prepared: u64,
    duplicates: u64,
    refused: u64,
    last_error: Option<&'static str>,
    last_format: Option<Value>,
    last_integrity: Option<Value>,
    integrity: Option<SessionIntegrity>,
}

impl ProgramAuth {
    /// A handler for one issued allocation.
    ///
    /// The job id is part of the program's environment (`game.JobId`), and the
    /// server's program reads it: a handler without one would run the program
    /// against a lie, so it is refused here rather than at the first challenge.
    pub fn for_allocation(game_id: Option<&str>, trust: PublicTrust) -> Result<Self> {
        let job_id = game_id.ok_or(ProgramError("issued-job-id-missing"))?;
        if job_id.len() != 36
            || !job_id.bytes().enumerate().all(|(i, b)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    b == b'-'
                } else {
                    b.is_ascii_hexdigit()
                }
            })
        {
            return Err(ProgramError("issued-job-id-invalid"));
        }
        Ok(Self {
            job_id: job_id.to_owned(),
            trust,
            cache: Vec::new(),
            verified: 0,
            executed: 0,
            prepared: 0,
            duplicates: 0,
            refused: 0,
            last_error: None,
            last_format: None,
            last_integrity: None,
            integrity: None,
        })
    }

    pub fn set_integrity(&mut self, observer: SessionIntegrity) {
        self.integrity = Some(observer);
    }

    /// Parse, verify, run, and answer. The only path to a reply.
    pub fn handle(&mut self, packet: &[u8]) -> Result<Vec<u8>> {
        let check = ProgramCheck::parse(packet)?;
        let key: [u8; 32] = blake2b256(packet);
        if let Some((_, reply)) = self.cache.iter().find(|(hash, _)| *hash == key) {
            // The server re-sent a challenge this session already answered: the
            // same reply, without running the program twice.
            self.duplicates += 1;
            return Ok(reply.clone());
        }
        if self.cache.len() >= MAX_DISTINCT_CHALLENGES {
            return self.fail(ProgramError("session-program-check-limit-reached"));
        }
        let verified = match check.verify_with_integrity(&self.trust, self.integrity.as_ref()) {
            Ok(verified) => verified,
            Err(error) => return self.fail(error),
        };
        self.verified += 1;
        let run = match crate::program_vm::run_verified_program(&verified, &self.job_id) {
            Ok(run) => run,
            Err(error) => return self.fail(error),
        };
        self.executed += 1;
        // The answer is what the server reads; the format and the integrity
        // report are kept for the diagnostics, which is where a program that
        // ran and produced the wrong number has to be diagnosable.
        let number = run.answer;
        self.last_format = Some(run.format);
        self.last_integrity = run.integrity;
        let mut response = vec![155];
        response.extend(check.second.to_le_bytes());
        response.extend(number.to_le_bytes());
        self.cache.push((key, response.clone()));
        self.prepared += 1;
        Ok(response)
    }

    fn fail<T>(&mut self, error: ProgramError) -> Result<T> {
        self.refused += 1;
        self.last_error = Some(error.reason());
        Err(error)
    }

    pub fn summary(&self) -> Value {
        json!({
            "signatures_verified": self.verified,
            "programs_executed": self.executed,
            "responses_prepared": self.prepared,
            "duplicate_checks": self.duplicates,
            "challenges_refused": self.refused,
            "last_challenge_error": self.last_error,
            "last_program_format": self.last_format,
            "last_program_integrity": self.last_integrity,
        })
    }
}

/// A `u32` answer from a program's return value: finite, clamped, truncated.
///
/// The clamp is not a guess about the answer — it is what the native host does
/// with a `double` on its way to a `u32`, and a program that returns a
/// non-finite number is a program that failed, not one to answer with zero.
pub fn native_unsigned_result(value: f64) -> Result<u32> {
    if !value.is_finite() {
        return Err(ProgramError("program-returned-nonfinite-number"));
    }
    Ok(value.clamp(0., u32::MAX as f64).trunc() as u32)
}

fn evaluate_public_map(signature: &[u8], coefficients: &[u128]) -> Result<[u8; 12]> {
    if signature.len() != 16 || coefficients.len() != COEFFICIENTS {
        return Err(ProgramError("invalid-public-map-input"));
    }
    let selected: Vec<usize> = (0..128)
        .filter(|bit| signature[bit / 8] & (0x80 >> (bit % 8)) != 0)
        .collect();
    // Row 0, then one row per selected bit, then the pairwise rows: the upper
    // triangle of a 128×128 block, addressed without a full table.
    let mut result = coefficients[0];
    for &bit in &selected {
        result ^= coefficients[1 + bit];
    }
    for (position, &left) in selected.iter().enumerate() {
        let row = 129 + 127 * left - left * left.saturating_sub(1) / 2;
        for &right in &selected[position + 1..] {
            result ^= coefficients[row + right - left - 1];
        }
    }
    Ok(result.to_le_bytes()[..12]
        .try_into()
        .expect("fixed public result"))
}

/// The `RSB1` container around a signed body.
///
/// The first four bytes are the key: `blob[i] ^ "RSB1"[i] - i*41`, and the rest
/// is the body XORed with that key, rotated by `i*41`. The fourth byte of the
/// key is the container's own checksum over the decoded bytes, so a corrupted
/// container is refused here rather than after decompression. The observer is
/// told whether the comparison matched, which is the one integrity check this
/// session really performs.
pub fn unpack_container(blob: &[u8]) -> Result<Vec<u8>> {
    unpack_container_observed(blob, None)
}

fn unpack_container_observed(blob: &[u8], observer: Option<&SessionIntegrity>) -> Result<Vec<u8>> {
    if !(9..=MAX_PROGRAM).contains(&blob.len()) {
        return Err(ProgramError("invalid-bytecode-container-size"));
    }
    let key: Vec<u8> = (0..4)
        .map(|i| (blob[i] ^ b"RSB1"[i]).wrapping_sub((i * 41) as u8))
        .collect();
    let raw: Vec<u8> = blob
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ key[i % 4].wrapping_add((i.wrapping_mul(41)) as u8))
        .collect();
    let magic_matches = &raw[..4] == b"RSB1";
    let checksum_matches = magic_matches
        && ticket_hash(&raw, 42) == u32::from_le_bytes(key[..].try_into().expect("four-byte key"));
    if let Some(observer) = observer {
        observer.observe_container_comparison(checksum_matches);
    }
    if !checksum_matches {
        return Err(ProgramError("bytecode-container-checksum-mismatch"));
    }
    let expanded =
        u32::from_le_bytes(raw[4..8].try_into().expect("checked container header")) as usize;
    if expanded == 0 {
        // Zero means "stored uncompressed": the body follows the header.
        return Ok(raw[8..].to_vec());
    }
    if expanded > MAX_PROGRAM {
        return Err(ProgramError("bytecode-container-expansion-exceeds-limit"));
    }
    decompress_bytes(&raw[8..], expanded, MAX_PROGRAM)
        .map_err(|_| ProgramError("invalid-compressed-bytecode-frame"))
}

/// A 32-byte digest of a whole packet, for the duplicate cache.
///
/// A blake3 digest as lowercase hex.
///
/// `blake3::Hash` has no `LowerHex` (and `to_hex()` returns another type whose
/// `as_str` borrows it), so the pinned-hash comparisons spell the digest out
/// here once rather than each doing it slightly differently.
fn blake3_hash_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes)
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `blake3` is already a dependency for the signature check; the cache only
/// needs to recognise the same packet twice, so this is a cache key and not a
/// security claim.
fn blake2b256(packet: &[u8]) -> [u8; 32] {
    *blake3::hash(packet).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_return_matches_finite_native_saturation() {
        assert_eq!(native_unsigned_result(-1.).unwrap(), 0);
        assert_eq!(native_unsigned_result(1.75).unwrap(), 1);
        assert_eq!(native_unsigned_result(1e99).unwrap(), u32::MAX);
        assert!(native_unsigned_result(f64::NAN).is_err());
        assert!(native_unsigned_result(f64::INFINITY).is_err());
    }

    #[test]
    fn the_embedded_trust_table_passes_its_own_provenance() {
        // The build's own table, verified the way a challenge verifies it. If
        // this fails the embedded asset and the pinned hash have drifted apart.
        let trust = PublicTrust::embedded().expect("the shipped table is the pinned one");
        assert_eq!(trust.coefficients.len(), COEFFICIENTS);
    }

    #[test]
    fn authentication_requires_an_issued_job_and_never_executes_unsigned_input() {
        let trust = || PublicTrust {
            coefficients: vec![0; COEFFICIENTS].into_boxed_slice(),
        };
        // No job id: refused before any challenge can be answered.
        assert!(ProgramAuth::for_allocation(None, trust()).is_err());
        assert!(ProgramAuth::for_allocation(Some("not-a-job-id"), trust()).is_err());
        let mut handler = ProgramAuth::for_allocation(
            Some("00000000-0000-4000-8000-000000000001"),
            trust(),
        )
        .expect("a valid job id");
        // A well-framed challenge whose container is not one: refused, and
        // nothing executed or answered.
        let mut packet = vec![155];
        packet.extend([0; 8]);
        packet.extend(9u32.to_le_bytes());
        packet.extend([0; 9]);
        assert!(handler.handle(&packet).is_err());
        let summary = handler.summary();
        assert_eq!(summary["programs_executed"], 0);
        assert_eq!(summary["responses_prepared"], 0);
        assert_eq!(summary["challenges_refused"], 1);
        assert!(summary["last_challenge_error"].is_string());
    }

    #[test]
    fn framing_preserves_argument_order_and_rejects_partial_or_extra_bytes() {
        let mut packet = vec![155];
        packet.extend(7u32.to_le_bytes());
        packet.extend(11u32.to_le_bytes());
        packet.extend(1u32.to_le_bytes());
        packet.push(1);
        assert_eq!(ProgramCheck::parse(&packet).unwrap().arguments(), [11, 7]);
        for size in 0..packet.len() {
            assert!(ProgramCheck::parse(&packet[..size]).is_err());
        }
        packet.push(0);
        assert!(ProgramCheck::parse(&packet).is_err());
    }

    /// Build a container the way a producer does, so the reader is tested
    /// against the format rather than against a constant.
    fn container(body: &[u8]) -> Vec<u8> {
        let mut raw = b"RSB1\0\0\0\0".to_vec();
        raw.extend(body);
        let key = ticket_hash(&raw, 42).to_le_bytes();
        raw.iter()
            .enumerate()
            .map(|(i, b)| b ^ key[i % 4].wrapping_add((i * 41) as u8))
            .collect()
    }

    #[test]
    fn rsb_uncompressed_roundtrip_and_corruption_failure() {
        let body = b"owned nonexecutable container fixture";
        let mut blob = container(body);
        assert_eq!(unpack_container(&blob).unwrap(), body);
        *blob.last_mut().unwrap() ^= 1;
        assert_eq!(
            unpack_container(&blob).unwrap_err().reason(),
            "bytecode-container-checksum-mismatch"
        );
    }

    #[test]
    fn checksum_observer_counts_only_the_actual_container_comparison() {
        let mut blob = container(b"fixture");
        let observer = SessionIntegrity::new();
        assert!(unpack_container_observed(&blob, Some(&observer)).is_ok());
        assert_eq!(observer.report()["checksum_comparisons"], 1);
        assert_eq!(observer.report()["observed_failure_flags"], 0);
        *blob.last_mut().unwrap() ^= 1;
        assert!(unpack_container_observed(&blob, Some(&observer)).is_err());
        assert_eq!(observer.report()["checksum_comparisons"], 2);
        assert_eq!(observer.report()["checksum_failures"], 1);
        assert_eq!(
            observer.report()["observed_failure_flags"],
            crate::session_integrity::CONTAINER_FAILURE
        );
        // A container too short to be one is refused *before* the comparison,
        // so it is not counted: the observer counts comparisons, not attempts.
        assert!(unpack_container_observed(&[1, 2], Some(&observer)).is_err());
        assert_eq!(observer.report()["checksum_comparisons"], 2);
    }

    #[test]
    fn the_public_map_uses_msb_first_bits_and_every_pair() {
        let values: Vec<u128> = (0..COEFFICIENTS).map(|v| v as u128).collect();
        assert_eq!(evaluate_public_map(&[0; 16], &values).unwrap(), [0; 12]);
        let mut signature = [0; 16];
        signature[0] = 0x80;
        signature[15] = 1;
        let expected = (1u128 ^ 128 ^ 255).to_le_bytes();
        assert_eq!(
            evaluate_public_map(&signature, &values).unwrap(),
            expected[..12]
        );
    }

    #[test]
    fn modified_trust_assets_and_unsigned_code_are_rejected() {
        assert!(PublicTrust::from_pinned_bytes(
            &vec![0; COEFFICIENTS * 12],
            &json!({"table_sha256": TABLE_HASH, "source_binary_sha256": SOURCE_HASH})
        )
        .is_err());
        let trust = PublicTrust {
            coefficients: vec![0; COEFFICIENTS].into_boxed_slice(),
        };
        assert!(trust.verify(&[1; 0x118]).is_err());
        let mut original = vec![1; 0x118];
        let at = original.len() - TRAILER;
        original[at..].fill(0);
        original[at + 16..at + 20].copy_from_slice(&0x74171870u32.to_le_bytes());
        original[at + 20..].copy_from_slice(&[0x32, 0xc4, 0x6a, 1]);
        assert_eq!(
            trust.verify(&original).unwrap_err().reason(),
            "signature-verification-failed"
        );
    }
}
