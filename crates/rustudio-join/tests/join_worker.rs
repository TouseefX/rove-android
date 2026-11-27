//! The failure half of a real join, which needs no network.
//!
//! Every branch that fails *before* a request goes out can be reached with a
//! cookie closure that returns nothing, and those are the branches a person is
//! most likely to hit first: not signed in, and a cookie Roblox will not accept.
//! The stages after them need an account and a live server, and are what the
//! phone run reports.

#![cfg(feature = "live")]

use std::time::{Duration, Instant};

#[test]
fn a_join_with_no_account_stops_at_sign_in_and_says_so() {
    let join = rustudio_join::join::start(1818, 1, || None);
    // The worker builds no runtime and makes no request: it should settle at
    // once. A generous bound keeps the test from hanging if it does not.
    let deadline = Instant::now() + Duration::from_secs(10);
    while join.running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!join.running(), "the worker finished");
    let report = join.report().expect("a report");
    assert_eq!(report.place_id, 1818);
    assert_eq!(
        report.verdict().as_deref(),
        Some("sign in failed: no account is signed in")
    );
    assert_eq!(report.stage, "sign in", "the stage it stopped at");
    assert!(join.take_document().is_none(), "no world without a session");
}

#[test]
fn a_cookie_that_cannot_go_into_a_header_fails_at_sign_in_not_at_the_join() {
    // A space or a newline in the value would be a request split;
    // `AccountSession::new` refuses it, and the step that reports it must be
    // sign in — a failure attributed to "join" says nothing about where it
    // happened.
    let join = rustudio_join::join::start(1818, 1, || Some("not a cookie".to_owned()));
    let deadline = Instant::now() + Duration::from_secs(10);
    while join.running() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let report = join.report().expect("a report");
    let verdict = report.verdict().expect("a failed step");
    assert_eq!(
        verdict,
        "sign in failed: Cookie must contain only the .ROBLOSECURITY value"
    );
    assert_eq!(report.stage, "sign in");
    // And the value itself is nowhere in what an overlay draws.
    for line in report.lines() {
        assert!(!line.contains("not a cookie"), "the cookie value must not appear: {line}");
    }
}

#[test]
fn the_report_is_readable_from_another_thread_while_the_join_runs() {
    // The overlay polls this every frame, so it must not block on the worker.
    let join = rustudio_join::join::start(1818, 1, || None);
    let started = Instant::now();
    for _ in 0..20 {
        let _ = join.lines();
        let _ = join.stats();
        let _ = join.running();
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "polling the report took {:?}",
        started.elapsed()
    );
}
