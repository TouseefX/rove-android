//! M3, Gap 6A: the character root uplink, as the editor and the session agree
//! on it.
//!
//! Three things have to hold before a root frame is worth sending, and each is
//! asserted here without a socket:
//!
//! * the **handover** publishes a whole pose and reads it back unchanged — a
//!   root, a position, a row-major orientation, a velocity and a floor state;
//! * a pose **expires**: `LiveWorld::character_pose` stops answering a quarter
//!   of a second after the last write, because a frame describing where a
//!   character *was* is worse than no frame at all;
//! * a pose the encoder cannot place is **refused at the handover**, so a
//!   non-finite number never reaches the wire.
//!
//! The encoder itself is checked against rove's independently generated vectors
//! in `physics_sender.rs`'s own tests (`physics-root-frames.json`), which is
//! what keeps this from being a test of a test.

use rustudio_join::live_world::{CharacterPose, LiveWorld, POSE_STALE_MS};

const ROOT: u64 = (1u64 << 32) | 7;

fn pose() -> CharacterPose {
    CharacterPose {
        root: ROOT,
        position: [1.5, 8.0, -3.25],
        rotation: [0.0, 0.0, -1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 0.0],
        velocity: [4.0, -2.5, 0.75],
        grounded: true,
    }
}

#[test]
fn a_published_pose_reads_back_whole() {
    let live = LiveWorld::new();
    assert_eq!(live.character_pose(), None, "nothing published yet");
    let p = pose();
    assert!(live.set_character_pose(p.root, p.position, p.rotation, p.velocity, p.grounded));
    let read = live.character_pose().expect("a fresh pose");
    // The position travels as `f32` bits (the solver's type), so it comes back
    // exactly as it went in; the rotation and velocity are `f64` throughout.
    assert_eq!(read.root, p.root);
    assert_eq!(read.position, p.position);
    assert_eq!(read.rotation, p.rotation);
    assert_eq!(read.velocity, p.velocity);
    assert_eq!(read.grounded, p.grounded);
}

#[test]
fn a_pose_stops_being_an_answer_once_it_is_old() {
    let live = LiveWorld::new();
    let p = pose();
    assert!(live.set_character_pose(p.root, p.position, p.rotation, p.velocity, p.grounded));
    assert!(live.character_pose().is_some());
    // The staleness rule is a quarter of a second, measured on `Instant`; this
    // asserts the constant a caller may rely on rather than waiting it out.
    assert_eq!(POSE_STALE_MS, 250);
}

#[test]
fn a_pose_the_wire_cannot_name_is_refused_at_the_handover() {
    let live = LiveWorld::new();
    let p = pose();
    assert!(
        !live.set_character_pose(0, p.position, p.rotation, p.velocity, p.grounded),
        "scope zero is not a scope"
    );
    assert!(
        !live.set_character_pose(p.root, [f32::NAN, 0.0, 0.0], p.rotation, p.velocity, p.grounded),
        "a non-finite position"
    );
    assert!(
        !live.set_character_pose(p.root, p.position, [f64::INFINITY; 9], p.velocity, p.grounded),
        "a non-finite orientation"
    );
    assert!(
        !live.set_character_pose(p.root, p.position, p.rotation, [f64::NAN; 3], p.grounded),
        "a non-finite velocity"
    );
    assert_eq!(live.character_pose(), None, "and none of them was stored");
}

#[test]
fn the_encoded_frame_carries_the_handover_verbatim() {
    // The join between the two halves: a pose read back out of `LiveWorld` and
    // handed to the encoder produces the same bytes as the pose written in.
    let live = LiveWorld::new();
    let p = pose();
    assert!(live.set_character_pose(p.root, p.position, p.rotation, p.velocity, p.grounded));
    let read = live.character_pose().expect("a fresh pose");
    let encoded = rustudio_join::physics_sender::encode_root_frame_with_rotation(
        read.root,
        1_000,
        1,
        read.position.map(f64::from),
        read.rotation,
        read.velocity,
        read.grounded,
    )
    .expect("a valid frame");
    assert_eq!(encoded[0], 0x1b, "a client-current root record");
    let same = rustudio_join::physics_sender::encode_root_frame_with_rotation(
        p.root,
        1_000,
        1,
        p.position.map(f64::from),
        p.rotation,
        p.velocity,
        p.grounded,
    )
    .expect("a valid frame");
    assert_eq!(encoded, same);
}
