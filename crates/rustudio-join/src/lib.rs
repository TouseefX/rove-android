//! A Roblox **player** join: the allocation a server issues, the login packets
//! derived from it, and the session that feeds rove's decoder.
//!
//! Three layers, each usable on its own:
//!
//! | module | what it holds | needs |
//! |---|---|---|
//! | [`allocation`] | the join-game response and everything parsed out of it | nothing |
//! | [`login`](crate::login) | the 5 login packets, byte for byte | nothing |
//! | [`session`] | WebTransport + `SessionCore` + the live bridge | `transport` / `live` |
//!
//! The offline layers are the point of the split: the wire shape is checked
//! against recorded JSON, the packets against rove's own encoder, and neither
//! needs a socket or a TLS stack. What cannot be checked offline is the one
//! thing no local test can assert — that Roblox accepts the join — and that is
//! why the real session sits behind `--features live` and reports every stage it
//! reached.
//!
//! ## Where this came from
//!
//! `rove/rust-client/src/allocation.rs` and `login.rs` are the reference: a
//! working join against the current gamejoin shape. This crate ports them, and
//! the differences are deliberate and listed where they happen:
//!
//! - rove's `AllocationResult::Pending` is a poll loop with a 3 s sleep; here
//!   the parsing is separated from the waiting, so the same bytes can be
//!   replayed in a test.
//! - rove's HTTP client is a `ureq::Agent`; here the transport is a trait, so
//!   the parsing has no I/O in it at all, and the one real client pins the
//!   certificate to the allocation's hashes exactly as the WebTransport side
//!   does.
//! - Secrets keep rove's discipline: private fields, `[REDACTED]` in `Debug`,
//!   and no `Display`.

pub mod allocation;
pub mod diagnostics;
// The whole of a real join, for the app: sign in, allocate, dial, log in,
// receive, with a report. Needs the transport and the HTTPS calls, so it is
// only built behind `live`.
#[cfg(feature = "live")]
pub mod join;
pub mod login;
// The session's state machine (stage, counters, feed) is not behind a feature:
// the live transport is, but what a session *does* with bytes is testable
// everywhere, and that is the half the recording proves.
pub mod session;

#[cfg(feature = "live")]
pub mod http;
// The version in the `User-Agent` the launcher checks for (behind `live`: it is
// fetched over HTTPS).
pub mod client_version;

pub use allocation::{
    new_join_attempt_id, request_allocation_until_ready, AllocationError, AllocationResult,
    IssuedJoinAllocation, JoinTarget, Selection, ALLOWED_HOSTS,
};
