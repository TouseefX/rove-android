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
    /// Signed-program challenges (`155`) received, and answered.
    pub program_challenges: u64,
    /// `ReplicationFocus` packets sent: what a streaming server needs to know
    /// which parts to keep sending.
    pub focus_updates: u64,
}

impl Counters {
    /// The one line the overlay shows. Deliberately short: it is read over a 3D
    /// view on a phone, and a wide line is an unreadable one.
    pub fn line(&self) -> String {
        format!(
            "{} · {} pkt ({} rep) · {} inst · {} held→{} · {} refused · {} rows · \
             acked {}/{} held · markers {}/{} · stream {}/{} {} · dgram {}/{} on {} ch{} · \
             acks {} · focus {}{} · {} s",
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
            // Datagrams: fed into the world out of how many arrived on a channel
            // the server announced. A session whose world never moves is one
            // where the second number is zero — the packets the motion travels
            // in were dropped before the decoder.
            self.datagrams_fed,
            self.datagrams_fed + self.unknown_datagrams,
            self.datagram_channels,
            self.property_acks_sent,
            self.focus_updates,
            if self.unknown_datagrams > 0 {
                format!(" · {} datagram(s) on unannounced wire ids", self.unknown_datagrams)
            } else {
                String::new()
            },
            self.seconds
        )
    }

    /// Packets arrived but built no instance: a session that is reading and not
    /// joining, which must never look like a working one.
    pub fn stalled(&self) -> bool {
        self.packets > 0 && self.instances == 0
    }
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
}

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
