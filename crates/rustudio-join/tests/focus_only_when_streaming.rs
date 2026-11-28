//! Focus is experimental: only opt-in sessions with a received region queue it.
//! These tests use explicit policies rather than racing on process environment.

use rustudio_join::session::{Outbound, Session, REPLICATION_APPLICATION, REPLICATION_CHANNEL};

#[test]
fn unset_environment_disables_focus_by_default() {
    // Isolate the environment from the rest of the parallel test process.
    const CHILD: &str = "ROVE_FOCUS_DEFAULT_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        assert!(std::env::var_os("ROVE_SEND_FOCUS").is_none());
        let mut session = Session::new();
        session.note_regions_for_test(&[serde_json::json!({"coordinate": [0, 0, 0]})]);
        assert_eq!(session.focus_sent, 0);
        assert!(session.outbound.is_empty());
    } else {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "unset_environment_disables_focus_by_default"])
            .env(CHILD, "1")
            .env_remove("ROVE_SEND_FOCUS")
            .status()
            .unwrap();
        assert!(status.success());
    }
}

#[test]
fn focus_disabled_even_after_region_and_forced_requests() {
    let mut session = Session::with_focus_enabled(false);
    session.note_regions_for_test(&[serde_json::json!({"coordinate": [0, 0, 0]})]);
    assert!(session.streamer_seen);
    assert_eq!(session.regions, 1);
    assert!(!session.refocus(Some([0.0, 5.0, 0.0])));
    assert!(!session.request_focus([64.0, 5.0, 0.0], true));
    assert!(!session.refocus(None));
    assert_eq!(session.focus_sent, 0);
    assert!(session.outbound.is_empty());
}

#[test]
fn opt_in_still_requires_a_region() {
    let mut session = Session::with_focus_enabled(true);
    assert!(!session.refocus(Some([0.0, 5.0, 0.0])));
    assert!(!session.request_focus([0.0, 5.0, 0.0], true));
    assert_eq!(session.focus_sent, 0);
    assert!(session.outbound.is_empty());
}

#[test]
fn opt_in_region_queues_focus_and_editor_can_refocus() {
    let mut session = Session::with_focus_enabled(true);
    session.note_regions_for_test(&[serde_json::json!({"coordinate": [0, 0, 0]})]);
    assert!(session.streamer_seen);
    assert_eq!(session.focus_position, Some([32.0, 32.0, 32.0]));
    assert_eq!(session.focus_sent, 1);
    assert!(matches!(session.outbound.as_slice(), [Outbound::Reply(REPLICATION_APPLICATION, REPLICATION_CHANNEL, payload)] if !payload.is_empty()));
    assert!(!session.refocus(Some([32.0, 32.0, 32.0])));
    assert!(session.refocus(Some([160.0, 32.0, 32.0])));
    assert_eq!(session.focus_sent, 2);
    assert_eq!(session.outbound.len(), 2);
}
