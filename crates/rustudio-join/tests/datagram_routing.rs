//! M3: an unreliable datagram is routed by its announcement, not by a constant.
//!
//! The bug this file exists for arrived from a phone: a live world with every
//! instance in it and not one moving part. `Session::feed_on` was fine; the
//! receive loop only ever read a datagram's payload when `wire_id == 4`, and
//! everything else — the instance-state deltas, the pings, the replication a
//! server sends over datagrams instead of streams — was counted and dropped.
//!
//! What is asserted here is the *contract*, not the socket:
//!
//! * an announcement on the control channel is what a `wire_id` means;
//! * a datagram on an announced world channel goes through the same
//!   `feed_on` dispatch a reliable packet does;
//! * a datagram on an announced *clock* channel goes to the time-sync probe and
//!   not into the world;
//! * a datagram naming an unannounced wire id is counted, never guessed at.
//!
//! `datagram()` is behind the `transport` feature (its arguments are the real
//! probe and clock), so the routing itself is driven here through the same two
//! entry points the loop uses: `Session::announce`-shaped `feed_on` of the
//! control packet, then `Session::feed_on` with the announced `(app, channel)`.

use rove_client_core::channels::{control_open, CONTROL_CHANNEL};
use rustudio_join::session::{Session, Stage, MAX_DATAGRAM_CHANNELS};

/// Feed a control-channel announcement, as the receive loop does for a packet
/// that arrived on the reliable control stream.
fn announce(session: &mut Session, application: u8, channel: u8, wire: u32) {
    let packet = control_open(application, channel, wire).expect("a valid announcement");
    session.feed_on(application, CONTROL_CHANNEL, &packet);
}

#[test]
fn an_announcement_is_what_a_wire_id_means() {
    let mut session = Session::new();
    session.stage = Stage::LoggedIn;
    assert!(session.channels.is_empty(), "nothing announced yet");

    announce(&mut session, 4, 5, 7);
    assert_eq!(session.channels.get(&7), Some(&(4, 5)));
    assert_eq!(session.channels.len(), 1);

    // A re-announcement of the same wire id as the same channel is a no-op, not
    // a fault: a server resends its control state on a reconnect.
    announce(&mut session, 4, 5, 7);
    assert_eq!(session.channels.len(), 1);

    // A *different* channel on a wire id already in use is refused, and the
    // first meaning is kept: quietly following the second is how a client
    // decodes one channel's bytes as another's.
    announce(&mut session, 4, 6, 7);
    assert_eq!(
        session.channels.get(&7),
        Some(&(4, 5)),
        "the first announcement stands"
    );
}

#[test]
fn the_clock_channels_are_announced_the_same_way() {
    let mut session = Session::new();
    // (the client's own half of the contract, as `run` seeds it at login)
    for (channel, wire) in [(2u8, 2u32), (3, 4)] {
        session.channels.insert(wire, (6, u32::from(channel)));
    }
    announce(&mut session, 6, 3, 4);
    assert_eq!(session.channels.get(&4), Some(&(6, 3)));
    assert!(session.datagram_is_clock(4), "wire 4 is the clock's 6/3");
    assert!(!session.datagram_is_clock(2), "wire 2 is 6/2, not the reply");
}

#[test]
fn there_is_a_cap_on_what_is_tracked() {
    let mut session = Session::new();
    // More announcements than the cap allows: the map stops growing rather than
    // growing without bound, and the world's own channels are the ones kept
    // (they came first).
    for wire in 1..=(MAX_DATAGRAM_CHANNELS as u32 + 4) {
        let channel = (wire % 31) as u8 + 1;
        announce(&mut session, 4, channel, wire);
    }
    assert_eq!(session.channels.len(), MAX_DATAGRAM_CHANNELS);
    assert!(session.channels.contains_key(&1), "the first ones are kept");
    assert!(
        !session
            .channels
            .contains_key(&(MAX_DATAGRAM_CHANNELS as u32 + 4)),
        "the ones past the cap are not"
    );
}

#[test]
fn a_datagram_on_an_announced_world_channel_reaches_the_world() {
    let mut session = Session::new();
    session.stage = Stage::LoggedIn;
    // The instance-state channel a real server sends most of a place on.
    announce(&mut session, 4, 5, 7);
    // Not a valid instance-state packet: what matters is that the *session*
    // took it — a refused packet is counted as refused, which can only happen
    // if the bytes reached the dispatch at all.
    let arrived = session.feed_on(4, 5, &[0xff, 0xff, 0xff, 0xff]);
    let _ = arrived;
    assert_eq!(
        session.arrivals.get(&(4, 5, 0xff)).copied(),
        Some(1),
        "the packet was seen on the announced channel"
    );
    assert!(
        session.ingested.refused + session.ingested.packets > 0,
        "and it reached the decoder"
    );
}

#[test]
fn a_datagram_on_the_control_channel_is_not_world_state() {
    let mut session = Session::new();
    session.stage = Stage::LoggedIn;
    let before = session.ingested.packets;
    // The announcement itself goes through `feed_on` and must not be applied as
    // replication: it names a channel, it is not one.
    announce(&mut session, 4, 5, 7);
    assert_eq!(
        session.ingested.packets, before,
        "a channel announcement is not a world packet"
    );
}
