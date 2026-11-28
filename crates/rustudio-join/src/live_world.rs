//! A joined place that stays joined: the editor's half of a live session.
//!
//! M2 delivered a *bounded* join: sign in, allocate, dial, read for thirty
//! seconds, hand the world over. That is a good test of a protocol and a poor
//! substitute for playing — the world stops arriving the moment the session
//! does, and what a person joined for is the place as it is *now*.
//!
//! ## Why a delta stream, and not the document
//!
//! The session decodes on the join's own thread and the editor draws on the
//! render thread, so the world has to be in two places at once. There are three
//! ways to do that, and two of them are wrong:
//!
//! - **Copy the document each frame.** `Document` owns a `WeakDom`, a journal
//!   and undo history; a joined place is 180 000 instances. The copy would cost
//!   more than the drawing.
//! - **Move the document between the threads.** Tempting — a move is free — but
//!   it cannot be made correct: whoever is *not* holding the world cannot apply
//!   the next delta to it, so the session either blocks on the renderer (a
//!   stalled join) or carries on against a document the editor has already
//!   moved past (a diverged world). Handing a stale document back loses every
//!   instance in it.
//! - **Send what changed.** The session already has it: rove's journal actions
//!   (`crate::session`'s `drain`) are exactly the delta, in receipt order, and
//!   the editor can apply them to its own document with the same
//!   [`rustudio_live::live::Live`] the session uses.
//!
//! So: both sides start from an empty DataModel, both apply the same actions in
//! the same order, and both end up in the same world. What crosses the thread
//! boundary is a batch of actions — small, and proportional to what changed.
//!
//! ## The marks are the point
//!
//! `Scene::sync` learns about changes from `Document::marks_for(Reader::Scene)`
//! and nothing else. A write that issues no mark is a change nothing redraws,
//! which is why a world applied without marks is invisible until something
//! forces a full rebuild. [`rustudio_live::live::Live`] marks every instance it
//! writes, so the editor's half of the world redraws from the deltas alone —
//! and `mark_all()` is never called, because a full rebuild every frame is the
//! opposite of a live session.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value as J;

/// One batch of rove's journal actions, with the counters of the moment.
///
/// The actions are the same `{"kind": "create"|"set"|"remove"|"event", …}`
/// objects `rustudio_live::live::Live::apply` consumes, in the order the journal
/// gave them (which is receipt order, not tree order — `Live` handles a create
/// that arrives before its parent).
#[derive(Clone, Debug, Default)]
pub struct Batch {
    pub actions: Vec<J>,
    pub counters: Counters,
}

/// What the overlay shows about the session, as opposed to the world.
#[derive(Clone, Debug, Default)]
pub struct Counters {
    /// The stage the session reached, as a word.
    pub stage: String,
    pub packets: usize,
    pub schema: usize,
    pub globals: usize,
    pub replication: usize,
    pub refused: usize,
    pub held: usize,
    pub released: usize,
    pub dropped: usize,
    pub instances: usize,
    pub other_streams: usize,
    pub pings: usize,
    pub sync_requests: usize,
    pub sync_replies: usize,
    /// Journal rows the session has read.
    pub rows: usize,
    /// Marker round-trips (`132` requests) and the replies written back. A
    /// session with markers and no replies is one the server will throttle, and
    /// the two numbers next to each other are how that is seen.
    pub markers: usize,
    /// The journal sequence acknowledged, and the retained record count. A
    /// session whose retained count climbs while `acked` stands still is one
    /// whose journal is about to refuse packets.
    pub acked: u64,
    /// Streaming: regions received, focus updates sent, and whether the spawn
    /// acknowledgment went. A streaming place sends half a world until that
    /// handshake happens, so these three say whether it did.
    pub regions: usize,
    pub focus_sent: usize,
    pub streaming_ready: bool,
    pub journal: usize,
    pub replies_sent: usize,
    pub replies_failed: usize,
    pub seconds: u64,
    /// **This client's own network scope** — rove's `self.core.scope()`.
    ///
    /// The one id `NetworkOwnerV3` uses to say "this assembly is yours": the
    /// server writes the owner's session scope, and a client that compares
    /// against `1` (the server's) or against a `Player.NetworkId` it guessed at
    /// would conclude it owns nothing and simulate nothing. Zero until the
    /// globals have arrived, which is also the honest answer — before them there
    /// is no scope, so no grant can be recognised.
    pub self_peer: u64,
    /// **Datagram channels the server announced**, and how many datagrams were
    /// routed through them. A live server sends its motion over datagrams; a
    /// client with zero here is one whose world will not move, whatever its
    /// reliable stream says.
    pub datagram_channels: usize,
    /// Datagrams fed into the world (the clock's excluded).
    pub datagrams_fed: u64,
    /// Datagrams naming a `wire_id` no announcement described: the number that
    /// says the *client* was not told, rather than the server not sending.
    pub unknown_datagrams: u64,
    /// `CFrame` acknowledgments written back on `(4, 1)`.
    pub property_acks_sent: u64,
    /// **Character-root physics frames written**, and refused. A joined world
    /// that renders while the character stands still on the server has a zero
    /// here, and that is the number that names the disagreement.
    pub physics_frames_sent: u64,
    pub physics_frames_failed: u64,
    /// Signed-program challenges (`155`) received, and answered. The pair is
    /// the single most important number in a live report: a server that asks and
    /// is not answered holds the character and the map back, and a report with
    /// `12/0` here says why nothing else moved.
    pub program_challenges: u64,
    pub program_answered: u64,
    /// **Pings, as the queue sees them.** `pings_answered` is replies actually
    /// written; `pings_pending` is what is still queued. A session whose
    /// received count climbs while the answered count stands still is one the
    /// server has stopped hearing from, and the two numbers next to each other
    /// are how that is seen on a phone.
    pub pings_answered: u64,
    pub pings_pending: usize,
    /// Requests the responder refused to answer: an unsupported request, a full
    /// queue, or an observation it did not have. Named separately from the
    /// answered count because "no reply" and "a reply that was not sent" are
    /// different faults.
    pub pings_blocked: u64,
    /// **Records the delta normaliser refused.** Non-zero means some batches
    /// reached the bridge in rove's raw wire spelling, where a `Part.CFrame`
    /// is a `ReplicationPV` (type 57) and a `Part.Size` is a lowercase `size`
    /// (type 22) — instances that exist and cannot be placed. The number is on
    /// the overlay because "954 instances and nothing moving" has two causes,
    /// and this one is invisible everywhere else.
    pub normalize_refused: u64,
}

impl Counters {
    /// The one line the overlay shows. Deliberately short: it is read over a 3D
    /// view on a phone, and a wide line is an unreadable one.
    pub fn line(&self) -> String {
        let line = format!(
            "{} · {} pkt ({} rep) · {} inst · {} held→{} · {} refused · {} rows · \
             acked {}/{} held · markers {}/{} · stream {}/{} {} · \
             dgram {}/{} on {} ch · acks {} · ping {}/{} ({} blocked) · phys {} · \
             focus {} · {} s · {}",
            if self.stage.is_empty() { "join" } else { &self.stage },
            self.packets,
            self.replication,
            self.instances,
            self.held,
            self.released,
            self.refused,
            self.rows,
            self.acked,
            self.journal,
            self.markers,
            self.replies_sent,
            self.regions,
            self.focus_sent,
            if self.streaming_ready { "acked" } else { "no-ack" },
            // Datagrams: fed into the world out of how many were routed at all.
            // A session whose world never moves is one where the first number
            // stands still — the packets the motion travels in were dropped
            // before the decoder.
            self.datagrams_fed,
            self.datagrams_fed + self.unknown_datagrams,
            self.datagram_channels,
            self.property_acks_sent,
            // Pings: the overlay used to print `markers sent/replies` here,
            // which is the ping pair a person reads a live session by.
            self.pings_answered,
            self.pings,
            self.pings_blocked,
            self.physics_frames_sent,
            self.focus_sent,
            self.seconds,
            // The first diagnosis, and only when there is one: a challenge the
            // server asked and this client did not answer is the reason a join
            // stops halfway, and it is invisible in every other number here.
            // The rest follow in the order a reader asks about them.
            if self.program_challenges > self.program_answered {
                format!(
                    " · 155 {}/{} answered",
                    self.program_answered, self.program_challenges
                )
            } else if self.normalize_refused > 0 {
                format!(" · {} record(s) unnormalized", self.normalize_refused)
            } else if self.pings_pending > 0 {
                format!(" · {} ping(s) queued", self.pings_pending)
            } else {
                String::new()
            },
            // ... and the second: an unannounced wire id means the client was
            // not told which channel a datagram belongs to, which is a different
            // fault from "the server sent nothing".
            if self.unknown_datagrams > 0 {
                format!(
                    " · {} datagram(s) on unannounced wire ids",
                    self.unknown_datagrams
                )
            } else {
                String::new()
            }
        );
        line
    }

    /// Records that bypassed normalization, for the same one-line diagnosis.
    pub fn unnormalized(&self) -> bool {
        self.normalize_refused > 0
    }

    /// The character-root uplink, for a report: frames written and refused.
    pub fn physics_sent(&self) -> u64 {
        self.physics_frames_sent
    }

    /// Packets arrived but built no instance: a session that is reading and not
    /// joining, which must never look like a working one.
    pub fn stalled(&self) -> bool {
        self.packets > 0 && self.instances == 0
    }
}

/// One character root, as the editor last saw it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CharacterPose {
    /// `(scope << 32) | index`, the wire's instance key for the root.
    pub root: u64,
    pub position: [f32; 3],
    /// Row-major 3×3, rove's convention.
    pub rotation: [f64; 9],
    /// Studs per second, the solver's. A root frame's velocity is what the
    /// server propagates to everyone else's interpolation, so a zero here is a
    /// character everybody else sees standing still and then jumping to the next
    /// position — the reason this is carried and not omitted.
    pub velocity: [f64; 3],
    pub grounded: bool,
}

/// The channel between the join's worker thread and the editor.
///
/// Cloneable and `Arc`-shared: the editor holds one, the worker holds one, and
/// neither can outlive the other's data.
#[derive(Clone, Default)]
pub struct LiveWorld {
    inner: Arc<LiveWorldInner>,
}

#[derive(Default)]
struct LiveWorldInner {
    /// Batches waiting for the editor. A queue, unlike a document: two batches
    /// are two *changes*, and dropping one loses an instance the editor will
    /// never be told about again. Bounded (see [`BATCH_BACKLOG`]) so a renderer
    /// that has stopped cannot grow it without limit — what is dropped is
    /// counted, and a non-zero count means the editor fell behind.
    pending: Mutex<std::collections::VecDeque<Batch>>,
    counters: Mutex<Counters>,
    /// Set by the editor to end the session.
    stop: AtomicBool,
    dropped_batches: AtomicU64,
    /// How many stop requests were honoured, for the overlay.
    stopped: AtomicBool,
    /// **Where the editor's view is**, in world studs, as the bits of three
    /// `f32`s. The session's thread reads it to send `ReplicationFocus` — the
    /// packet that tells a streaming server which region to keep sending.
    ///
    /// It lives here, on the shared handover, because the two halves are on
    /// different threads and neither may borrow the other: the renderer knows
    /// where the camera is and the session knows how to ask. `f32` has no atomic
    /// form, so the bits go through a `u64` slot each and a "written" flag says
    /// whether any of them is real (zero is a legitimate coordinate).
    focus: [AtomicU32; 3],
    focus_set: AtomicBool,
    /// **Where our character root is**, for the physics uplink.
    ///
    /// The editor owns the character — it is the one stepping it — and the
    /// session owns the socket. This is where the two meet without either
    /// borrowing the other. It is a lock and not the atomics `focus` uses
    /// because a pose is a *whole* thing (a root, nine rotation components, a
    /// floor state and the moment they were true) and half of one is a frame
    /// that describes nothing; the focus is a point, where any three numbers
    /// that were once real are still a usable answer. Written once a frame and
    /// read once per 20 ms tick, so the lock is not contended.
    ///
    /// The age is kept as an [`Instant`] rather than a number so that the
    /// staleness rule does not depend on two clocks with different origins
    /// agreeing about what "now" is.
    pose: Mutex<Option<(std::time::Instant, CharacterPose)>>,
}

/// How old a character pose may be before the session stops sending it.
///
/// 250 ms, which is `rove`'s own bound (`live_session.rs`): a frame older than a
/// quarter second describes a character that has already moved, and sending it
/// makes the server's reconciliation fight the client's own simulation.
pub const POSE_STALE_MS: u64 = 250;

/// How many action batches may wait for the editor. Each is one journal read
/// (`DRAIN_BATCH` rows), so this is a few thousand rows of margin — enough to
/// ride out a stutter, small enough that a wedged renderer is noticed as a count
/// rather than as memory.
pub const BATCH_BACKLOG: usize = 64;

impl LiveWorld {
    pub fn new() -> Self {
        Self::default()
    }

    /// This client's own session scope (`Counters::self_peer`), published with
    /// every batch: the document needs it to tell a `NetworkOwnerV3` that names
    /// *us* from one that names the server or another player.
    pub fn self_peer(&self) -> Option<u64> {
        Some(self.counters().self_peer).filter(|v| *v != 0)
    }

    /// Publish a batch of actions for the editor to apply.
    ///
    /// Called from the session's thread after each drain. Cheap: a `Vec` moves
    /// and the queue takes a lock for the length of a push.
    pub fn publish(&self, actions: Vec<J>, counters: Counters) {
        if actions.is_empty() {
            // Still republish the counters: the overlay must move even during a
            // second in which the world did not change, or a healthy session
            // looks stuck.
            self.set_counters(counters);
            return;
        }
        if let Ok(mut queue) = self.inner.pending.lock() {
            if queue.len() >= BATCH_BACKLOG {
                // Drop the *oldest*, not the newest: the newest is the state a
                // person is looking at, and the next `create` for anything
                // dropped is not coming. Counted, because this is a real loss
                // and the report must say so rather than present a world that
                // is quietly missing a piece of itself.
                queue.pop_front();
                self.inner.dropped_batches.fetch_add(1, Ordering::Relaxed);
            }
            queue.push_back(Batch {
                actions,
                counters: counters.clone(),
            });
        }
        self.set_counters(counters);
    }

    /// Take every batch waiting, oldest first, so the editor applies them in the
    /// order the journal gave them.
    pub fn take(&self) -> Vec<Batch> {
        let Ok(mut queue) = self.inner.pending.lock() else {
            return Vec::new();
        };
        queue.drain(..).collect()
    }

    /// How many batches are waiting. Zero means the editor has caught up.
    pub fn pending(&self) -> usize {
        self.inner
            .pending
            .lock()
            .map(|queue| queue.len())
            .unwrap_or(0)
    }

    pub fn counters(&self) -> Counters {
        self.inner
            .counters
            .lock()
            .map(|c| c.clone())
            .unwrap_or_default()
    }

    pub fn set_counters(&self, counters: Counters) {
        if let Ok(mut slot) = self.inner.counters.lock() {
            *slot = counters;
        }
    }

    /// Batches dropped because the editor fell `BATCH_BACKLOG` behind.
    pub fn dropped(&self) -> u64 {
        self.inner.dropped_batches.load(Ordering::Relaxed)
    }

    /// **Tell the session where the view is.** Called by the editor each frame
    /// with the camera's (or the character's) position in studs; the session's
    /// thread turns it into `ReplicationFocus`, which is the packet a streaming
    /// server needs before it will send anything but the spawn region.
    pub fn set_focus(&self, position: [f32; 3]) {
        for (slot, value) in self.inner.focus.iter().zip(position) {
            slot.store(value.to_bits(), Ordering::Relaxed);
        }
        self.inner.focus_set.store(true, Ordering::Relaxed);
    }

    /// The last focus the editor gave, or `None` before the first one.
    pub fn focus(&self) -> Option<[f32; 3]> {
        if !self.inner.focus_set.load(Ordering::Relaxed) {
            return None;
        }
        let mut out = [0f32; 3];
        for (o, slot) in out.iter_mut().zip(&self.inner.focus) {
            *o = f32::from_bits(slot.load(Ordering::Relaxed));
        }
        out.iter().all(|v| v.is_finite()).then_some(out)
    }

    /// **Tell the session where our character root is.**
    ///
    /// Written by the editor every frame the local character exists; read by the
    /// session's 20 ms tick and encoded into a `(4, 6)` root frame.
    ///
    /// `rotation` is rove's row-major 3×3, the same convention `Xform::from_cframe`
    /// reads. `root` is `(scope << 32) | index` because that is how the wire names
    /// an instance. A pose with a zero root or a non-finite number is refused
    /// here and never reaches the encoder: a frame the server cannot place is
    /// worse than no frame, because it moves a character to a nonsense position
    /// instead of leaving it where it was.
    pub fn set_character_pose(
        &self,
        root: u64,
        position: [f32; 3],
        rotation: [f64; 9],
        velocity: [f64; 3],
        grounded: bool,
    ) -> bool {
        if root == 0
            || !position.iter().all(|v| v.is_finite())
            || !rotation.iter().all(|v| v.is_finite())
            || !velocity.iter().all(|v| v.is_finite())
        {
            return false;
        }
        let pose = CharacterPose {
            root,
            position,
            rotation,
            velocity,
            grounded,
        };
        match self.inner.pose.lock() {
            Ok(mut slot) => {
                *slot = Some((std::time::Instant::now(), pose));
                true
            }
            Err(_) => false,
        }
    }

    /// The last pose, if one is younger than [`POSE_STALE_MS`].
    ///
    /// `None` before the editor has written one, and after it stops writing:
    /// a session that kept sending an old pose would be telling the server about
    /// a character that is no longer where the frame says it is, which is the
    /// one thing a client's own root frame must not do.
    pub fn character_pose(&self) -> Option<CharacterPose> {
        let slot = self.inner.pose.lock().ok()?;
        let (at, pose) = (*slot)?;
        (at.elapsed() <= std::time::Duration::from_millis(POSE_STALE_MS)).then_some(pose)
    }

    pub fn request_stop(&self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        self.inner.stopped.store(true, Ordering::Relaxed);
    }

    pub fn stop_requested(&self) -> bool {
        self.inner.stop.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for LiveWorld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveWorld")
            .field("pending", &self.pending())
            .field("dropped", &self.dropped())
            .field("stopped", &self.stop_requested())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(id: &str) -> J {
        serde_json::json!({
            "kind": "create",
            "id": id,
            "node": {"class_name": "Part", "properties": {"Name": id}}
        })
    }

    fn counters(instances: usize) -> Counters {
        Counters {
            stage: "receive".into(),
            packets: 10,
            instances,
            ..Default::default()
        }
    }

    #[test]
    fn batches_come_back_in_the_order_they_went_in() {
        let live = LiveWorld::new();
        live.publish(vec![create("a")], counters(1));
        live.publish(vec![create("b")], counters(2));
        let taken = live.take();
        assert_eq!(taken.len(), 2);
        assert_eq!(taken[0].actions[0]["id"], "a");
        assert_eq!(taken[1].actions[0]["id"], "b");
    }

    #[test]
    fn taking_empties_the_queue_and_the_second_take_is_empty() {
        let live = LiveWorld::new();
        live.publish(vec![create("a")], counters(1));
        assert_eq!(live.pending(), 1);
        assert_eq!(live.take().len(), 1);
        assert_eq!(live.pending(), 0);
        assert!(live.take().is_empty());
    }

    #[test]
    fn a_batch_with_no_actions_still_moves_the_counters() {
        let live = LiveWorld::new();
        live.publish(Vec::new(), counters(7));
        assert_eq!(live.pending(), 0, "nothing to apply");
        assert_eq!(live.counters().instances, 7, "but the overlay moved");
    }

    #[test]
    fn a_renderer_that_falls_behind_is_counted_not_grown() {
        let live = LiveWorld::new();
        for i in 0..(BATCH_BACKLOG + 3) {
            live.publish(vec![create(&format!("p{i}"))], counters(i));
        }
        assert_eq!(live.pending(), BATCH_BACKLOG, "the queue is bounded");
        assert_eq!(live.dropped(), 3, "and the loss is counted");
        // The newest survived: dropping the oldest keeps what is on screen.
        let taken = live.take();
        let last = taken.last().expect("a batch");
        assert_eq!(last.actions[0]["id"], format!("p{}", BATCH_BACKLOG + 2));
    }

    #[test]
    fn stop_is_asked_and_answered_across_threads() {
        let live = LiveWorld::new();
        let worker_side = live.clone();
        assert!(!worker_side.stop_requested());
        live.request_stop();
        assert!(worker_side.stop_requested());
    }

    #[test]
    fn a_stalled_session_is_named_as_one() {
        let stalled = Counters {
            stage: "receive".into(),
            packets: 1353,
            instances: 0,
            ..Default::default()
        };
        assert!(stalled.stalled());
        assert!(stalled.line().contains("1353"), "{}", stalled.line());
        assert!(!counters(1).stalled(), "a world with instances is not stalled");
    }
}
