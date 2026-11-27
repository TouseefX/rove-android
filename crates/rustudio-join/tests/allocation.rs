//! M2a: the allocation, parsed offline.
//!
//! Every case here is a recorded body and no socket. The point is that the
//! parser's decisions are checked against the wire shape rather than against a
//! server that may change its mind: the address it picks, the port it falls
//! back to, the pins it needs, and — the ones that matter most — the responses
//! it must **refuse** to dial.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use serde_json::Value;

use rustudio_join::allocation::{
    classify, parse_allocation, AllocationError, AllocationResult, HttpClient, JoinTarget, Reply,
    Selection,
};

const FIXTURE: &str = include_str!("fixtures/join_game.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("the fixture is JSON")
}

/// The user every fixture joins as.
const USER: u64 = 424_242;
/// The place every fixture joins: Roblox HQ, 1818.
const PLACE: u64 = 1818;

fn target() -> JoinTarget {
    JoinTarget::place(PLACE)
}

fn reply_of(body: &Value) -> Reply {
    Reply {
        http_status: 200,
        challenge_present: false,
        content_type: "application/json; charset=utf-8".into(),
        data: Some(body.clone()),
    }
}

fn ready(name: &str) -> AllocationResult {
    let fixture = fixture();
    let body = &fixture[name];
    parse_allocation(
        200,
        body.as_object(),
        &target(),
        None,
        USER,
        "application/json",
    )
    .unwrap_or_else(|error| panic!("{name} was refused: {error}"))
}

fn issued(name: &str) -> rustudio_join::allocation::IssuedJoinAllocation {
    match ready(name) {
        AllocationResult::Ready(issued) => issued,
        other => panic!("{name} was not ready: {other:?}"),
    }
}

#[test]
fn the_dial_address_is_the_udmux_front_and_the_backend_is_the_machine() {
    let issued = issued("ready");
    // Dial the udmux front from the port the NetStackConfig agrees with.
    assert_eq!(
        issued.endpoint,
        Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(5, 5, 5, 5)), 443)),
        "UdmuxEndpoints[0].Address, on NetStackConfig's port"
    );
    // The routing header carries the machine, which is a *different* address
    // when udmux is in front. Getting these two the same way is the mistake
    // this test exists to catch.
    assert_eq!(
        issued.backend,
        Some(Ipv4Addr::new(7, 7, 7, 7)),
        "MachineAddress is the backend the header names"
    );
    assert!(issued.is_transport_ready());
}

#[test]
fn the_pins_come_from_the_percent_encoded_netstack_config() {
    let issued = issued("ready");
    assert_eq!(issued.certificate_sha256.len(), 2);
    // The fixture's pins are 00..1f and 20..3f: recognizable, and in order.
    assert_eq!(issued.certificate_sha256[0][0], 0x00);
    assert_eq!(issued.certificate_sha256[0][31], 0x1f);
    assert_eq!(issued.certificate_sha256[1][0], 0x20);
    assert_eq!(issued.certificate_sha256[1][31], 0x3f);
}

#[test]
fn a_missing_token_value_is_the_zero_token_path() {
    // The current allocations omit NetStackTokenValue entirely; the header
    // falls back to sixteen zero bytes rather than refusing the join.
    let issued = issued("ready");
    assert_eq!(issued.token(), Some(&[0u8; 16]));
    assert!(issued.is_transport_ready(), "zero token is still a token");
}

#[test]
fn a_server_without_udmux_is_dialled_at_its_machine_address() {
    let issued = issued("machine_only");
    assert_eq!(
        issued.endpoint,
        Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(7, 7, 7, 7)), 443)),
        "with no udmux front, MachineAddress is the address to dial"
    );
    assert_eq!(issued.backend, Some(Ipv4Addr::new(7, 7, 7, 7)));
}

#[test]
fn no_netstack_config_means_no_pins_and_so_no_dial() {
    let issued = issued("no_netstack");
    assert!(issued.certificate_sha256.is_empty());
    // The port is still read from NetStackPort, but without a pin there is
    // nothing to authenticate the peer against, so this is not dialable and
    // says so rather than connecting unpinned.
    assert!(
        !issued.is_transport_ready(),
        "an unpinned endpoint must not be reported as ready"
    );
}

#[test]
fn the_identity_is_the_account_the_request_was_made_for() {
    let issued = issued("ready");
    assert_eq!(issued.user_id, USER);
    assert_eq!(issued.user_name, "SyntheticTester");
    assert_eq!(issued.display_name, "Synthetic Tester");
    assert_eq!(issued.place_id, PLACE);
    assert_eq!(issued.session_id(), "synthetic-session-id");
}

#[test]
fn a_secret_is_not_in_the_debug_output() {
    let issued = issued("ready");
    let debug = format!("{issued:?}");
    assert!(!debug.contains("synthetic-session-id"), "{debug}");
    assert!(!debug.contains("synthetic;unused"), "{debug}");
    assert!(!debug.contains("SyntheticTester"), "{debug}");
    assert!(debug.contains("[REDACTED]"));
    // And the display summary names the endpoint without a secret.
    let summary = issued.summary();
    assert!(summary.contains("5.5.5.5:443"), "{summary}");
    assert!(!summary.contains("synthetic-session-id"));
}

#[test]
fn a_loopback_endpoint_is_refused_rather_than_dialled() {
    let fixture = fixture();
    let body = &fixture["rejected"]["loopback_endpoint"];
    let result = parse_allocation(
        200,
        body.as_object(),
        &target(),
        None,
        USER,
        "application/json",
    );
    match result {
        Ok(AllocationResult::Ready(issued)) => {
            assert_eq!(
                issued.endpoint, None,
                "127.0.0.1 is not a game server and must not become an endpoint"
            );
            assert!(!issued.is_transport_ready());
        }
        other => panic!("a loopback endpoint should parse to a non-dialable allocation: {other:?}"),
    }
}

#[test]
fn the_documentation_ranges_are_refused_like_loopback() {
    // 203.0.113.0/24, 198.51.100.0/24 and 192.0.2.0/24 are reserved for
    // documentation and never route. The first fixture written for this test
    // used them, which is how the refusal came up: a join script naming one is
    // not something to dial, and the parser says so by leaving the endpoint
    // empty rather than by connecting and hanging.
    let fixture = fixture();
    let issued = match parse_allocation(
        200,
        fixture["rejected"]["documentation_range"].as_object(),
        &target(),
        None,
        USER,
        "application/json",
    )
    .expect("the body parses; the address is what is refused")
    {
        AllocationResult::Ready(issued) => issued,
        other => panic!("{other:?}"),
    };
    assert_eq!(issued.endpoint, None);
    assert!(!issued.is_transport_ready());
}

#[test]
fn a_response_for_another_place_or_account_is_an_error() {
    let fixture = fixture();
    for (name, expected) in [
        ("wrong_place", "Issued allocation did not match requested place"),
        (
            "wrong_user",
            "Issued allocation did not match authenticated account",
        ),
    ] {
        let body = &fixture["rejected"][name];
        let error = parse_allocation(
            200,
            body.as_object(),
            &target(),
            None,
            USER,
            "application/json",
        )
        .expect_err(name);
        assert_eq!(error.message(), expected, "{name}");
    }
}

#[test]
fn unusable_pins_are_dropped_and_the_allocation_is_not_dialable() {
    let fixture = fixture();
    let body = &fixture["rejected"]["bad_pins"];
    let issued = match parse_allocation(
        200,
        body.as_object(),
        &target(),
        None,
        USER,
        "application/json",
    )
    .expect("the body parses; it is the pins that are unusable")
    {
        AllocationResult::Ready(issued) => issued,
        other => panic!("{other:?}"),
    };
    assert!(issued.certificate_sha256.is_empty());
    assert!(!issued.is_transport_ready(), "nothing to pin the peer to");
}

#[test]
fn a_config_whose_port_disagrees_with_the_script_is_not_trusted() {
    let fixture = fixture();
    let body = &fixture["rejected"]["port_disagrees"];
    let issued = match parse_allocation(
        200,
        body.as_object(),
        &target(),
        None,
        USER,
        "application/json",
    )
    .expect("the body parses")
    {
        AllocationResult::Ready(issued) => issued,
        other => panic!("{other:?}"),
    };
    // NetStackConfig says 443, the script says 8443: neither is trusted, so
    // there is no port and therefore no endpoint.
    assert!(issued.certificate_sha256.is_empty());
    assert_eq!(issued.endpoint, None);
}

#[test]
fn an_html_reply_is_not_a_join() {
    // A challenge page or a captive portal carries 200 and HTML.
    let fixture = fixture();
    let body = &fixture["ready"];
    let error = parse_allocation(
        200,
        body.as_object(),
        &target(),
        None,
        USER,
        "text/html; charset=utf-8",
    )
    .expect_err("html must not be read as a join");
    assert!(error.message().contains("not JSON"), "{error}");
}

#[test]
fn a_pending_status_is_pending_and_a_challenge_is_a_challenge() {
    let fixture = fixture();
    let pending = classify(
        reply_of(&fixture["pending"]),
        &target(),
        None,
        USER,
    )
    .expect("a pending body parses");
    assert!(
        matches!(pending, AllocationResult::Pending { status: 1, .. }),
        "{pending:?}"
    );

    let challenge = classify(
        Reply {
            http_status: 403,
            challenge_present: true,
            content_type: "application/json".into(),
            data: None,
        },
        &target(),
        None,
        USER,
    )
    .expect("a challenge is a decision, not an error");
    assert!(
        matches!(challenge, AllocationResult::Challenge { http_status: 403 }),
        "{challenge:?}"
    );
}

#[test]
fn a_rejection_keeps_the_status_it_was_given() {
    let rejected = classify(
        Reply {
            http_status: 403,
            challenge_present: false,
            content_type: "application/json".into(),
            data: Some(serde_json::json!({"status": 8})),
        },
        &target(),
        None,
        USER,
    )
    .expect("a rejection is a decision");
    match rejected {
        AllocationResult::Rejected {
            http_status,
            status,
            message,
        } => {
            assert_eq!(http_status, 403);
            assert_eq!(status, Some(8));
            assert_eq!(message, None, "this body carried no message");
        }
        other => panic!("{other:?}"),
    }
}

/// A client that replays replies and records what it was asked for.
struct Recorded {
    replies: Vec<Reply>,
    asked: Vec<(String, String, String, Option<Value>)>,
}

impl HttpClient for Recorded {
    fn send(
        &mut self,
        host: &str,
        path: &str,
        method: &str,
        payload: Option<&Value>,
    ) -> Result<Reply, AllocationError> {
        self.asked
            .push((host.to_owned(), path.to_owned(), method.to_owned(), payload.cloned()));
        match self.replies.is_empty() {
            true => Ok(Reply {
                // A client that was asked more than it recorded: hand back the
                // least interesting reply rather than failing, so the test's
                // own assertions (about how many requests were made) decide.
                http_status: 500,
                challenge_present: false,
                content_type: "application/json".into(),
                data: None,
            }),
            false => Ok(self.replies.remove(0)),
        }
    }
}

#[test]
fn the_request_is_a_join_game_post_with_the_place_and_a_fresh_attempt_id() {
    use rustudio_join::allocation::{new_join_attempt_id, request_allocation};
    let fixture = fixture();
    let mut http = Recorded {
        replies: vec![reply_of(&fixture["ready"])],
        asked: Vec::new(),
    };
    let attempt = new_join_attempt_id().expect("entropy");
    let result = request_allocation(&mut http, &target(), None, &attempt, USER).expect("a reply");
    assert!(matches!(result, AllocationResult::Ready(_)));
    let (host, path, method, payload) = &http.asked[0];
    assert_eq!(host, "gamejoin.roblox.com");
    assert_eq!(path, "/v1/join-game", "no job id means the matchmaker");
    assert_eq!(method, "POST");
    let payload = payload.as_ref().expect("a body");
    assert_eq!(payload["placeId"], PLACE);
    assert_eq!(payload["gameJoinAttemptId"], attempt.as_str());
    assert_eq!(payload["isTeleport"], false);
    assert_eq!(payload.get("gameId"), None);
}

#[test]
fn a_selected_server_joins_that_instance() {
    use rustudio_join::allocation::request_allocation;
    let fixture = fixture();
    let job = "a1b2c3d4-e5f6-4789-8abc-def012345678";
    let mut http = Recorded {
        replies: vec![reply_of(&fixture["ready"])],
        asked: Vec::new(),
    };
    let attempt = rustudio_join::allocation::new_join_attempt_id().unwrap();
    let target = JoinTarget::server(PLACE, job);
    let result = request_allocation(&mut http, &target, None, &attempt, USER).expect("a reply");
    assert!(matches!(result, AllocationResult::Ready(_)), "{result:?}");
    let (_, path, _, payload) = &http.asked[0];
    assert_eq!(path, "/v1/join-game-instance");
    assert_eq!(payload.as_ref().unwrap()["gameId"], job);
}

#[test]
fn a_malformed_attempt_id_never_reaches_the_network() {
    use rustudio_join::allocation::request_allocation;
    let mut http = Recorded {
        replies: Vec::new(),
        asked: Vec::new(),
    };
    let error = request_allocation(&mut http, &target(), None, "not-a-uuid", USER).unwrap_err();
    assert_eq!(error.message(), "Invalid join attempt identifier");
    assert!(http.asked.is_empty(), "nothing was sent");
}

#[test]
fn the_poll_gives_up_after_eight_pending_replies() {
    use rustudio_join::allocation::request_allocation_until_ready_with;
    let fixture = fixture();
    let mut http = Recorded {
        replies: (0..8).map(|_| reply_of(&fixture["pending"])).collect(),
        asked: Vec::new(),
    };
    let attempt = rustudio_join::allocation::new_join_attempt_id().unwrap();
    let mut waits = 0;
    let result = request_allocation_until_ready_with(
        &mut http,
        &target(),
        None,
        &attempt,
        USER,
        |_| waits += 1,
    )
    .expect("the last pending is still a pending");
    assert!(matches!(result, AllocationResult::Pending { .. }), "{result:?}");
    assert_eq!(http.asked.len(), 8, "eight POSTs");
    assert_eq!(waits, 7, "a wait between them and none after the last");
}

#[test]
fn a_challenge_stops_the_poll_at_once() {
    use rustudio_join::allocation::request_allocation_until_ready_with;
    let mut http = Recorded {
        replies: vec![Reply {
            http_status: 403,
            challenge_present: true,
            content_type: "application/json".into(),
            data: None,
        }],
        asked: Vec::new(),
    };
    let attempt = rustudio_join::allocation::new_join_attempt_id().unwrap();
    let result = request_allocation_until_ready_with(
        &mut http,
        &target(),
        None,
        &attempt,
        USER,
        |_| panic!("a challenge must not wait"),
    )
    .expect("a challenge is a decision");
    assert!(matches!(result, AllocationResult::Challenge { .. }));
    assert_eq!(http.asked.len(), 1, "one request, no retries");
}

#[test]
fn a_private_server_needs_its_invite_and_matches_it() {
    use rustudio_join::allocation::{request_allocation, PrivateInvite};
    let fixture = fixture();
    let mut http = Recorded {
        replies: vec![reply_of(&fixture["ready"])],
        asked: Vec::new(),
    };
    let attempt = rustudio_join::allocation::new_join_attempt_id().unwrap();
    let mut target = JoinTarget::place(PLACE);
    target.selection = Selection::UserSelectedPrivateServer;
    // No invite is a refusal, before anything is sent.
    let error = request_allocation(&mut http, &target, None, &attempt, USER).unwrap_err();
    assert_eq!(
        error.message(),
        "Private invite does not match selected place"
    );
    assert!(http.asked.is_empty());

    // An invite for another place is refused too.
    let invite = PrivateInvite::new(999, 1, 7, "link-code").unwrap();
    let error = request_allocation(&mut http, &target, Some(&invite), &attempt, USER).unwrap_err();
    assert_eq!(
        error.message(),
        "Private invite does not match selected place"
    );
}

#[test]
fn an_attempt_id_is_a_canonical_v4_uuid() {
    use rustudio_join::allocation::new_join_attempt_id;
    let id = new_join_attempt_id().expect("entropy");
    assert_eq!(id.len(), 36);
    let bytes: Vec<&str> = id.split('-').collect();
    assert_eq!(
        bytes.iter().map(|part| part.len()).collect::<Vec<_>>(),
        [8, 4, 4, 4, 12]
    );
    assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    assert_eq!(&id[14..15], "4", "version 4");
    assert!(
        matches!(&id[19..20], "8" | "9" | "a" | "b"),
        "variant bits: {id}"
    );
    assert_ne!(id, new_join_attempt_id().unwrap(), "a fresh one each time");
}

#[test]
fn a_refusal_keeps_the_sentence_that_says_which_refusal_it_is() {
    // The case a phone hit: HTTP 200, status 12, and only the body says why.
    // A status number on its own cannot tell "this identity is refused" from
    // "this place does not exist" from "playtesters only".
    let rejected = classify(
        Reply {
            http_status: 200,
            challenge_present: false,
            content_type: "application/json".into(),
            data: Some(serde_json::json!({
                "status": 12,
                "message": "You are not allowed to play this game",
                "joinScript": null,
            })),
        },
        &target(),
        None,
        USER,
    )
    .expect("a refusal is a decision, not an error");
    match rejected {
        AllocationResult::Rejected { status, message, .. } => {
            assert_eq!(status, Some(12));
            assert_eq!(message.as_deref(), Some("You are not allowed to play this game"));
        }
        other => panic!("{other:?}"),
    }
}
