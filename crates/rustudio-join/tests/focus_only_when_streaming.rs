//! **A focus is an answer to a streamer, not a heartbeat.**
//!
//! `ReplicationFocus` exists for places that stream: the server sends the parts
//! around it and nothing else, so a client that never sends one gets a spawn and
//! silence — and a client that sends one to a place that does not stream is
//! sending a packet about a streamer that does not exist there, which is traffic
//! on the one reliable stream whose stalls cost the most.
//!
//! The rule this file pins: *no region has arrived, so no focus is sent.*
//! `note_regions` is the only place a region is ever seen, so the first focus is
//! the answer to a stream the server has already started; before that, the
//! editor's per-frame position — which `LiveWorld::focus` supplies from the very
//! first frame of a join — is not sent at all.

use rustudio_join::session::Session;

#[test]
fn no_focus_is_sent_before_a_region_arrives() {
    let mut session = Session::new();
    assert!(!session.streamer_seen, "nothing has streamed yet");
    // The editor asks every frame. Before a region, the answer is no — and not
    // even a forced one, which is what a keep-alive would pass.
    assert!(!session.refocus(Some([0.0, 5.0, 0.0])), "a non-streaming place is not asked about");
    // The gate is in `request_focus` — the one place a focus is queued — so no
    // caller gets round it, including a forced one.
    assert!(!session.request_focus([0.0, 5.0, 0.0], true), "nothing was queued for it");
    assert_eq!(session.focus_sent, 0, "and nothing reached the wire");
}

#[test]
fn a_streamed_place_still_gets_its_focus() {
    let mut session = Session::new();
    // The spawn region as the record carries it: a cell coordinate, 64 studs a
    // side with the centre at +32 (`note_regions`' own arithmetic).
    let regions = vec![serde_json::json!({"coordinate": [0.0, 0.0, 0.0]})];
    session.note_regions_for_test(&regions);
    assert!(session.streamer_seen, "a region arrived: this place streams");
    assert!(session.focus_sent > 0, "the spawn region is answered with a focus");
}
