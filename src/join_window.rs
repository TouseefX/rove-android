//! Joining an official Roblox server: the button, the report, and the world.
//!
//! This is the app's end of [`rustudio_join::join`]. It is deliberately small,
//! because the join itself is not here: the crate owns sign in, allocation, the
//! WebTransport dial, the five login packets and feeding the decoder. What this
//! module adds is the three things a person on a phone needs —
//!
//! 1. **A way to start one.** Start Page → *Play a place*, or `RUSTUDIO_JOIN`.
//! 2. **A report they can read.** Every stage, the one that failed, and the
//!    session's counters, drawn over the editor while it runs. The build's
//!    annotations can only carry what CI can compile; this is what a phone
//!    reports, and the reason the overlay is on.
//! 3. **The place itself.** When a session ends with a world in it, that world
//!    is installed into the editor — the same `Document` a place opened from a
//!    file becomes — so the three hundred things replication created are
//!    Explorer rows rather than a number in a log.
//!
//! Nothing here keeps or prints a credential: the cookie is read from the
//! account, handed to the crate's worker as a closure, and dropped there.

#![cfg(feature = "live")]

use std::time::{Duration, Instant};

use rbx_dom_weak::types::Variant;

use crate::app::Editor;

/// Place 1818 (*Baseplate*): a public place, so a join needs no special access,
/// and the one M2's acceptance names.
pub const DEFAULT_PLACE: u64 = 1818;

/// How long a session keeps reading after it logs in. Long enough to see a
/// world, short enough that a test run ends by itself.
const SESSION_SECONDS: u64 = 30;

/// Whether a finished session's world is worth installing by itself.
///
/// An auto-install of an empty world would replace whatever the person had
/// open with nothing, so this asks for instances; a session that only read a
/// schema and ended is reported, not installed.
fn world_worth_opening(stats: &rustudio_join::join::Stats) -> bool {
    stats.created > 0
}

/// The cookie as the app's own session holds it, for the join's worker.
///
/// Read inside the closure, on the worker thread, so the value exists only for
/// the sign-in step: `rustudio_account` keeps the store (the keyring), and this
/// crate keeps no copy.
fn cookie_source() -> impl FnOnce() -> Option<String> + Send + 'static {
    move || {
        let client = rustudio_core::assets::session_client()?;
        let client = client.lock().ok()?;
        let client = client.as_ref()?;
        Some(client.session.cookie().header())
    }
}

/// A join the person started, and how it is going.
/// One line of the joining cover, and whether it is done, happening, or waiting.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CoverStep {
    Done,
    Now,
    Waiting,
}

/// What the cover draws this frame.
struct CoverPhase {
    steps: Vec<(String, CoverStep)>,
    note: Option<String>,
}

/// One line about `Workspace`: how many parts it has, and how many of them a
/// renderer and a physics solver can actually use.
///
/// The counts are the three ways a streamed part is useless without an error
/// anywhere: no `CFrame` (it is at the origin), no `Size` (it is a point), and
/// `Transparency >= 1` (it is there and invisible). `waiting_for_parent` is the
/// fourth: instances whose create named a parent that has not arrived, which is
/// a model that will never be in the tree.
fn workspace_summary(doc: &rustudio_core::doc::Document) -> String {
    let Some(ws) = doc.workspace() else {
        return "join world: no Workspace in the replicated document".to_string();
    };
    let (mut parts, mut framed, mut no_cframe, mut no_size, mut invisible) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    for i in doc.dom.descendants_of(ws) {
        if !rustudio_core::doc::is_a(&i.class, "BasePart") || i.class == "Terrain" {
            continue;
        }
        parts += 1;
        let has = |name: &str| i.properties.contains_key(&rbx_dom_weak::ustr(name));
        if has("CFrame") {
            framed += 1;
        } else {
            no_cframe += 1;
        }
        if !has("Size") {
            no_size += 1;
        }
        if matches!(
            i.properties.get(&rbx_dom_weak::ustr("Transparency")),
            Some(rbx_dom_weak::types::Variant::Float32(t)) if *t >= 1.0
        ) {
            invisible += 1;
        }
    }
    format!(
        "join world: Workspace has {parts} BasePart(s): {framed} with a CFrame, \
         {no_cframe} without, {no_size} without Size, {invisible} fully transparent"
    )
}

/// The `Place` field's egui id: named so the focus can be given and taken
/// deliberately, which is the difference between a report a phone can read and
/// one with a soft keyboard over it.
const PLACE_FIELD_ID: &str = "join_place_field";

pub struct JoinWindow {
    pub open: bool,
    pub place_text: String,
    join: Option<rustudio_join::join::Join>,
    /// When the join was started.
    started: Option<Instant>,
    /// A world already installed: the overlay then only shows the report, and
    /// closing it leaves the place open.
    pub installed: bool,
    /// The report shown as selectable text (the copy's fallback).
    pub show_text: bool,
    /// The cover is up from the tap until the live client is in.
    pub cover_until_played: bool,
    /// Whether the `Place` field should take focus this frame. `false` while a
    /// join's report is read on a touch device (the soft keyboard would cover
    /// it); set by the field itself being asked for, on a desk.
    pub place_focus: bool,
    /// A Cancel/Stop asked for on the cover, taken on the next frame so the
    /// click handler does not stop a join from inside its own draw.
    pub cancel_requested: bool,
    /// When the report was last copied, for the "copied" note.
    pub copied_at: Option<Instant>,
    /// The places this device has joined, newest first — the home page's cards
    /// (`crate::start_page::home`). Not persisted yet: a device's own record of
    /// what it played is enough for the card to be one tap.
    pub recent: Vec<crate::start_page::home::RecentJoin>,
    /// The session's counters as of the last handover (M3): the overlay draws
    /// these while the place keeps arriving, so a working session and a stalled
    /// one never look the same.
    pub live: Option<rustudio_join::live_world::Counters>,
    /// When `live` was last written to the console: one line a second, not one
    /// a frame.
    live_logged_at: Option<Instant>,
    /// When the world was last seen to change, and whether the load report has
    /// been written yet: the report describes a world that has stopped
    /// arriving, so it waits for a quiet moment rather than counting half a
    /// place.
    live_report_at: Option<Instant>,
    live_reported: bool,
    /// The live handover, kept here (and not on `Join`) so the editor can hold
    /// it after the join object is dropped: a stopped join still has a world.
    live_world: Option<rustudio_join::live_world::LiveWorld>,
    /// The editor's own half of a live session's world (M3): the same `Live`
    /// the session applies with, so a delta means the same thing on both sides.
    /// `None` until a live join starts, and dropped when one stops.
    pub live_local: Option<rustudio_live::live::Live>,
    /// The one reader of rove's tagged values, for the editor's half.
    pub live_converter: rustudio_live::snapshot::ValueConverter,
    /// How many instance writes the editor's half has made: the number that
    /// says the two documents are in step, and that a session is *drawing*
    /// rather than merely receiving.
    pub live_applied: usize,
    /// The editor's own "Show Core UI" setting when a join turned it on, so it
    /// comes back afterwards (a Roblox client always draws CoreGui, but the
    /// editor's own setting is the person's). `None` = the join left it alone.
    pub core_gui_was: Option<bool>,
    /// A world was installed and the session it came from should be *played*:
    /// F5 is pressed on the tick after `set_document`, because Play reads the
    /// document and the join window must not hold the editor mutably while it
    /// asks for it.
    pub play_next_tick: bool,
}

impl Default for JoinWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl JoinWindow {
    /// **Where this client stands**, for `ReplicationFocus`.
    ///
    /// The local character's root part, if a character has resolved: a
    /// streaming server sends the parts around this point, so following the
    /// character is what keeps a walked-away client from being streamed the
    /// region it left. `None` before there is one, and the camera answers then.
    ///
    /// Read from the live document rather than the gameplay state because this
    /// is the same `Document` the session's deltas are applied to: the root's
    /// `CFrame` is exactly what the server last said, or what the local
    /// simulation last wrote.
    fn character_focus(&self) -> Option<glam::Vec3> {
        character_focus_in(&self.game.doc)
    }

    pub fn new() -> Self {
        Self {
            open: false,
            place_text: DEFAULT_PLACE.to_string(),
            join: None,
            started: None,
            installed: false,
            show_text: false,
            cover_until_played: false,
            place_focus: !cfg!(target_os = "android"),
            cancel_requested: false,
            copied_at: None,
            recent: Vec::new(),
            live: None,
            live_logged_at: None,
            live_report_at: None,
            live_reported: false,
            live_world: None,
            live_local: None,
            live_converter: Default::default(),
            live_applied: 0,
            core_gui_was: None,
            play_next_tick: false,
        }
    }

    pub fn running(&self) -> bool {
        self.join.as_ref().is_some_and(|join| join.running())
    }

    /// The place id typed into the window.
    fn place(&self) -> Option<u64> {
        self.place_text.trim().parse().ok()
    }
}

impl Editor {
    /// Keep a live session's world in front of the person who joined (M3).
    ///
    /// Called every frame from the editor's own frame, whether or not the join
    /// window is open — closing the window must not stop the place arriving, and
    /// a world that only moved while a window was drawn would freeze the moment
    /// someone started playing.
    ///
    /// What arrives is a batch of rove's journal actions, and it is applied here
    /// with the same `Live` the session applies to its own copy. That is the
    /// whole design: two documents, one delta stream, one implementation of what
    /// a delta means. The instances written here are **marked**, so `Scene::sync`
    /// redraws them and nothing else. `mark_all()` is never called on this path:
    /// a full rebuild every frame is the opposite of a live session.
    pub fn live_join_frame(&mut self) {
        // The channel is cloned out in its own statement, so the borrow of
        // `join_win` ends here: everything below writes to the editor, and a
        // live borrow of the window would forbid it.
        let Some(live) = self
            .join_win
            .join
            .as_ref()
            .and_then(|join| join.live())
        else {
            return;
        };
        // **Whose simulation is whose.** The join's own session scope — rove's
        // `self.core.scope()`, published with every batch — is the id the server
        // writes into a part's `NetworkOwnerV3` when it hands that assembly to
        // this client. The physics reads it from the document, so it is taken
        // here, before the world: a document with the peer and no world, or the
        // world and no peer, cannot answer "do I simulate this?" in either
        // direction.
        let self_peer = live.self_peer();
        let staged = match (self_peer, self.game.doc.self_peer) {
            (Some(peer), seen) if seen != Some(peer) => Some(peer),
            _ => None,
        };
        if let Some(peer) = staged {
            self.game.doc.self_peer = Some(peer);
        }
        // Nothing waiting: the common case, and the reason this is cheap enough
        // to ask every frame.
        if live.pending() == 0 {
            self.join_win.live = Some(live.counters());
            return;
        }
        // Taken in one go rather than batched by hand: the queue is already in
        // journal order, and `Live` handles a create whose parent has not
        // arrived (`attach_waiting`).
        let new_world = !self.join_win.installed;
        let batches = live.take();
        // The editor's own `Live`, made once per session: it holds the id map
        // between rove's `scope:index` names and this document's refs, and a
        // fresh one per batch would lose every id it has already seen.
        //
        // Taken out of the window for the length of the loop and put back, so
        // the borrow checker sees one mutable borrow of the editor at a time:
        // `game.doc` is written while the `Live` that writes into it is in hand.
        let mut live_local = self.join_win.live_local.take().unwrap_or_default();
        let mut converter = std::mem::take(&mut self.join_win.live_converter);
        let mut written = 0usize;
        for batch in batches {
            let stats = live_local.apply(&mut self.game.doc, &batch.actions, &mut converter);
            written += stats.created + stats.properties + stats.removed;
            self.join_win.live = Some(batch.counters);
        }
        self.join_win.live_local = Some(live_local);
        self.join_win.live_converter = converter;
        self.join_win.live_applied += written;
        // The report is written once the world has stopped changing, not as it
        // arrives: a per-service count taken mid-stream is a count of half a
        // place, and the refusal list is not sorted until there is something to
        // sort. `live_world_report_at` is the point after which it is printed.
        if !self.join_win.live_reported {
            let settled = self
                .join_win
                .live_report_at
                .is_some_and(|at| at.elapsed() >= Duration::from_secs(3));
            if settled || new_world {
                // A quiet three seconds with the batches drained *now*, or the
                // first world arriving at all: either way this is the world as
                // it stands, not half of one.
                self.join_win.live_reported = true;
                self.join_win.live_report_at = None;
                self.live_datamodel_report();
            } else if self.join_win.live_report_at.is_none() {
                // The clock starts at the first batch and is *not* restarted by
                // later ones: a stream that never pauses must still be reported.
                self.join_win.live_report_at = Some(Instant::now());
            }
        }
        if self.join_win.live_reported {
            // The first report fires within seconds, when the world is half
            // arrived: a class the server names later — a rig's Motor6Ds, a
            // tool's welds — is not in it. Re-reporting every twenty seconds
            // keeps the same numbers available for a session that keeps
            // arriving, which is what a live join does.
            let due = self
                .join_win
                .live_report_at
                .is_some_and(|at| at.elapsed() >= Duration::from_secs(20));
            if due {
                self.join_win.live_report_at = Some(Instant::now());
                self.live_datamodel_report();
            } else if self.join_win.live_report_at.is_none() {
                self.join_win.live_report_at = Some(Instant::now());
            }
        }
        if !self.join_win.installed {
            // The first instances are in the editor: from here on this is a
            // place being played, not a join being watched.
            self.join_win.installed = true;
            if let Some(place) = self.join_win.place() {
                self.remember_join(place, true);
            }
            self.info(format!(
                "join: the world is here ({} instance(s)); it keeps arriving while you play",
                self.game.doc.dom.len()
            ));
            // **And enter it.** Installing the world is not playing it: the
            // document sits in the editor looking like an opened `.rbxl`, with
            // the explorer, the tabs and no camera bound to a character, which
            // is exactly what a person who joined someone's place must not see.
            // `play_next_tick` was only ever set by the *button*, so an
            // automatic join arrived at the world and stopped there.
            //
            // Deferred by a frame for the same reason the button's path is: the
            // install gave `set_document` a document, and Play reads it.
            if self.game.play.is_none() {
                self.join_win.play_next_tick = true;
            }
        }
        self.live_join_overlay();
    }

    /// What the world is made of, and what was refused building it (M3).
    ///
    /// Printed once a live session has settled, and on demand: a joined world
    /// that is missing pieces needs to say *which* pieces, and "{} instances"
    /// does not. The per-service counts say whether the place arrived whole
    /// (Workspace, Players, Lighting, ReplicatedStorage); the refusal list says
    /// which `Class.Property` the converter could not read, most frequent
    /// first, because a DataModel with holes in it is almost always one
    /// property in one class.
    fn live_datamodel_report(&mut self) {
        // Everything is gathered into owned strings first, then printed: the
        // ledger lives inside `self.join_win` and `self.info` takes `&mut self`,
        // so a `&str` borrowed from the ledger would hold `self` immutably for
        // the whole body. Nothing here needs to borrow across a print.
        let lines: Vec<String> = {
            let Some(live) = self.join_win.live_local.as_ref() else {
                return;
            };
            let doc = &self.game.doc;
            let root = doc.dom.root_ref();
            let mut lines: Vec<String> = Vec::with_capacity(5);
            // The services a place is made of, as they stand now.
            let mut services: Vec<(String, usize)> = Vec::new();
            let children = doc
                .dom
                .get_by_ref(root)
                .map(|i| i.children().to_vec())
                .unwrap_or_default();
            for child in children {
                // `children()` was taken before this loop and the loop does not
                // write, so every ref is still present — but `descendants_of`
                // panics rather than returning empty if that ever stops being
                // true, and a report must not be able to kill the editor.
                let Some(instance) = doc.dom.get_by_ref(child) else {
                    continue;
                };
                let count = doc.dom.descendants_of(child).count();
                services.push((instance.class.to_string(), count));
            }
            services.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let made: Vec<String> = services
                .iter()
                .take(8)
                .map(|(class, count)| format!("{class} {count}"))
                .collect();
            lines.push(format!(
                "join world: {} instance(s) in {} service(s) — {}",
                doc.dom.len(),
                services.len(),
                made.join(", ")
            ));
            // **Whether the map is a map.** Instances and properties written
            // say the stream is alive; they do not say whether anything in
            // `Workspace` can be drawn or stood on. A part with no `CFrame` is
            // at the origin, one with no `Size` is a point, and one with
            // `Transparency >= 1` is invisible — three different reasons a
            // phone shows "the world arrived and I cannot see it", which the
            // top-ten lists below cannot tell apart.
            lines.push(workspace_summary(doc));
            // What *is* arriving, not only what is not. A report that lists
            // refusals and nothing else cannot tell "properties are being
            // written" from "nothing has been written since the world came".
            let written = live.written_properties();
            if written.is_empty() {
                lines.push(
                    "join world: no property has been written — the world arrived and stopped"
                        .to_string(),
                );
            } else {
                let top: Vec<String> = written
                    .iter()
                    .take(10)
                    .map(|(name, count)| format!("{name} ×{count}"))
                    .collect();
                lines.push(format!(
                    "join world: {} propert{} written — {}",
                    written.len(),
                    if written.len() == 1 { "y" } else { "ies" },
                    top.join(", ")
                ));
            }
            let refused = live.refused_properties();
            if refused.is_empty() {
                lines.push("join world: no property was refused".to_string());
            } else {
                let top: Vec<String> = refused
                    .iter()
                    .take(10)
                    .map(|(name, count)| format!("{name} ×{count}"))
                    .collect();
                lines.push(format!(
                    "join world: {} refused propert{} — {}",
                    refused.len(),
                    if refused.len() == 1 { "y" } else { "ies" },
                    top.join(", ")
                ));
            }
            // **By class**, when there is more than one property in one: the
            // difference between "`Part.Shape` is not converted" and "nothing
            // about `Model` converts" is a different bug and a different fix,
            // and the top-ten list above cannot tell them apart.
            let by_class = live.refused_properties_by_class();
            if by_class.len() > 1 || by_class.first().is_some_and(|(_, n)| *n > 2) {
                let top: Vec<String> = by_class
                    .iter()
                    .take(8)
                    .map(|(name, count)| format!("{name} ×{count}"))
                    .collect();
                lines.push(format!(
                    "join world: refusals by class — {}",
                    top.join(", ")
                ));
            }
            // References: bound late (the target arrived after the property),
            // and still waiting. Both are normal in a live stream — a
            // `Model.PrimaryPart` is set in the delta that creates the part —
            // so a join with a hundred of them is a healthy one, and the
            // number to worry about is `dangling`, the ones nothing can bind.
            if live.refs_bound() > 0 || live.refs_pending() > 0 {
                lines.push(format!(
                    "join world: {} ref(s) bound late, {} still waiting for their target",
                    live.refs_bound(),
                    live.refs_pending()
                ));
            }
            let dangling = live.dangling_refs();
            if !dangling.is_empty() {
                let top: Vec<String> = dangling
                    .iter()
                    .take(6)
                    .map(|(name, count)| format!("{name} ×{count}"))
                    .collect();
                lines.push(format!(
                    "join world: {} dangling ref(s) — {}",
                    dangling.iter().map(|(_, c)| c).sum::<usize>(),
                    top.join(", ")
                ));
            }
            // **The handshake numbers**, which are a different question from
            // what the world is made of: `155` answered out of received, pings
            // answered out of received, markers drained, and `CFrame` acks
            // written. A live report used to list the world and stop there, so a
            // join held back by an unanswered challenge looked exactly like a
            // join whose map was arriving slowly.
            // The counters the handover last published, not a fresh read: the
            // report describes the same moment the overlay line does.
            let counters = self.join_win.live.clone();
            if let Some(counters) = counters.as_ref() {
                lines.push(format!(
                    "join live: 155 {}/{} answered · ping {}/{} ({} blocked) · \
                     markers {} · acks {} · regions {} · focus {}",
                    counters.program_answered,
                    counters.program_challenges,
                    counters.pings_answered,
                    counters.pings,
                    counters.pings_blocked,
                    counters.markers,
                    counters.property_acks_sent,
                    counters.regions,
                    counters.focus_sent,
                ));
                if counters.program_challenges > counters.program_answered {
                    lines.push(format!(
                        "join live: the server asked {} signed-program challenge(s) this \
                         client did not answer — it holds the character and the map back \
                         until one is",
                        counters.program_challenges - counters.program_answered
                    ));
                }
            }
            let classes = live.refused_classes();
            if !classes.is_empty() {
                // Sorted, because the point is the top of the list: a world of
                // Models with no parts in it is usually one class repeated.
                let mut classes = classes;
                classes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                let top: Vec<String> = classes
                    .iter()
                    .take(12)
                    .map(|(name, count)| format!("{name} ×{count}"))
                    .collect();
                lines.push(format!(
                    "join world: {} class(es) the database does not know, or a create with none — {}",
                    classes.len(),
                    top.join(", ")
                ));
            }
            lines
        };
        for line in lines {
            self.info(line);
        }
    }

    /// The overlay's line while a session runs: packets, instances, rows.
    ///
    /// Drawn into the console so it is in the log a person copies, once a second
    /// — the console is a scrolling log, not a progress bar.
    fn live_join_overlay(&mut self) {
        let Some(counters) = self.join_win.live.clone() else {
            return;
        };
        // Only while a join is in flight. A session that has ended leaves its
        // last counters behind (the window keeps them for the report), and a
        // line a second about a session that is over is noise in the log a
        // person is asked to copy.
        if !self.join_win.join.as_ref().is_some_and(|j| j.running()) {
            return;
        }
        let due = self
            .join_win
            .live_logged_at
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(1));
        if !due {
            return;
        }
        self.join_win.live_logged_at = Some(Instant::now());
        // **Where the view is, for the streamer.** A streaming server sends the
        // parts around this client's `ReplicationFocus` and nothing else, so the
        // camera's position is the one thing the session's thread cannot know by
        // itself: it is on this side of the handover, and it is what decides
        // which region the map arrives from. Sent once a second with the overlay
        // (the session only sends when the focus moved or lapsed).
        if let Some(live) = self.join_win.live_world.as_ref() {
            // **The character's root while it exists, the camera before it
            // does.** A streaming server sends the parts around this client's
            // `ReplicationFocus`; the camera's position is the one thing the
            // session's thread cannot know by itself. Where the character stands
            // is the better answer once there is one — a person who walked two
            // hundred studs should be streamed the parts around *them*, not
            // around a camera that is still in its default place — and the
            // camera is what there is until then.
            let at = self
                .join_win
                .character_focus()
                .or_else(|| self.game.cam.focus)
                .unwrap_or(self.game.cam.pos);
            live.set_focus([at.x, at.y, at.z]);
        }
        let dropped = self
            .join_win
            .live_world
            .as_ref()
            .map(|live| live.dropped())
            .unwrap_or(0);
        self.info(format!(
            "join live: {} · {} written here{}",
            counters.line(),
            self.join_win.live_applied,
            if dropped > 0 {
                format!(" · {dropped} batch(es) dropped")
            } else {
                String::new()
            }
        ));
    }

    /// A full-screen "Joining…" cover, drawn *over* the editor from the moment
    /// Play is tapped until the live client is in.
    ///
    /// What it hides is not cosmetic. Between the tap and the first replicated
    /// frame, the editor is still showing whatever was last open: a bare
    /// empty-skybox viewport with the explorer, the ribbon and a toolbar. On a
    /// phone that reads as "the app opened an .rbxl file and froze", which is
    /// exactly the report it kept getting.
    ///
    /// It is a *cover*, not a gate: it draws while `join_cover()` says a join is
    /// in flight, and it never blocks on the whole map. The three lines it shows
    /// are the three things a person waits for, and the last one clears as soon
    /// as the character is bound — streaming continues behind a game that is
    /// already playable, which is what Roblox does.
    ///
    /// The session's own numbers stay reachable: the small `report` line at the
    /// bottom opens the existing window, so a stall is diagnosable from the
    /// phone rather than by reading logcat.
    pub fn join_cover(&mut self, ctx: &egui::Context) {
        // Gone the moment the game is playable: a cover over a running game
        // would be the same problem in a different colour.
        if self.join_win.cover_until_played && self.live_client_ready() {
            self.join_win.cover_until_played = false;
        }
        // And gone if the join died under it. A cover with nothing behind it is
        // worse than the editor: the takeover that made it is over, and what is
        // on screen now is a place that will never arrive. It comes down and
        // says so, and the session's own "closed" reason is left in Output.
        let session_over = self
            .join_win
            .join
            .as_ref()
            .is_some_and(|join| !join.running())
            && self.game.play.is_none();
        if self.join_win.cover_until_played && session_over {
            self.join_win.cover_until_played = false;
            self.info("join: the session ended before the world arrived");
        }
        let Some(phase) = self.join_cover_phase() else {
            return;
        };
        // **Past the point of the cover: a banner, not a wall.** The full-screen
        // cover is what hides the editor before there is a game behind it. Once
        // Play has started on the replicated world, the thing behind it is a
        // running place — CoreGui, the camera, the touch controls, the map
        // streaming in — and a sheet of opaque pixels over it that also swallows
        // every tap is the same "the app froze" report in a different colour.
        // From here the wait is one line at the top of the screen, and the game
        // underneath takes input.
        if self.cover_wants_banner() {
            self.join_banner(ctx, &phase);
            return;
        }
        // The report window is drawn from the editor's frame (`join_window`),
        // which runs *before* this and is an `egui::Window`. A cover painted
        // over it hid the numbers a person had just asked for, so while the
        // report is open the cover steps out of its way entirely: it is the
        // same join underneath, and the window's own Close puts the cover back
        // (the window is *only* reachable from this cover — `start_join` closes
        // it on the tap — so "the window is open" always means "the person is
        // reading the report of the join the cover belongs to").
        if self.join_win.open {
            return;
        }
        // Not `screen.min`-sized with the painter: the buttons below are real
        // widgets (`ui.allocate_rect(…, Sense::click())`), which is what makes a
        // *touch* tap a tap. The old version read
        // `ctx.input(|i| i.pointer.any_click())` and compared it with the press
        // position, and a finger on Android produces a click egui only
        // synthesises at the end of a gesture — so Report and Cancel did
        // nothing at all on the phone.
        let screen = ctx.content_rect();
        let mut open_report = false;
        let mut cancel = false;
        egui::Area::new(egui::Id::new("join_cover"))
            .fixed_pos(screen.min)
            .order(egui::Order::Middle)
            .interactable(true)
            .show(ctx, |ui| {
                // The whole screen is the cover: everything under it stays
                // drawn (the world is arriving behind this), but no tap in it
                // reaches the editor.
                let (rect, response) = ui.allocate_exact_size(screen.size(), egui::Sense::click());
                let painter = ui.painter_at(rect).with_clip_rect(rect);
                painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(18, 20, 26));
                let _ = response;
                // The three steps, as a list with the current one lit. Drawn
                // rather than laid out with widgets so it cannot scroll.
                let centre = rect.center();
                let mut y = centre.y - 46.0;
                for (label, state) in phase.steps.iter() {
                    let colour = match state {
                        CoverStep::Done => egui::Color32::from_rgb(120, 190, 140),
                        CoverStep::Now => egui::Color32::WHITE,
                        CoverStep::Waiting => egui::Color32::from_rgb(110, 116, 130),
                    };
                    let mark = match state {
                        CoverStep::Done => "✓",
                        CoverStep::Now => "•",
                        CoverStep::Waiting => "·",
                    };
                    painter.text(
                        egui::Pos2::new(centre.x, y),
                        egui::Align2::CENTER_CENTER,
                        format!("{mark}  {label}"),
                        egui::FontId::proportional(if *state == CoverStep::Now { 21.0 } else { 18.0 }),
                        colour,
                    );
                    y += 30.0;
                }
                if let Some(note) = &phase.note {
                    painter.text(
                        egui::Pos2::new(centre.x, y + 12.0),
                        egui::Align2::CENTER_CENTER,
                        note,
                        egui::FontId::proportional(13.0),
                        egui::Color32::from_rgb(140, 146, 160),
                    );
                }
                // The way out of a stall: the numbers, and a cancel. Sized for
                // a finger (Roblox's own controls are 44 points) and laid out
                // with `allocate_ui_at_rect` so egui's own click detection runs
                // for them, which is the difference between a button and a
                // picture of one on a touchscreen.
                let button_y = rect.max.y - 64.0;
                let report = egui::Rect::from_center_size(
                    egui::Pos2::new(centre.x - 76.0, button_y),
                    egui::vec2(140.0, 44.0),
                );
                let leave = egui::Rect::from_center_size(
                    egui::Pos2::new(centre.x + 76.0, button_y),
                    egui::vec2(140.0, 44.0),
                );
                for (button, text) in [(report, "Report"), (leave, "Cancel")] {
                    let response = ui.allocate_rect(button, egui::Sense::click());
                    ui.painter().rect_filled(
                        button,
                        8.0,
                        if response.is_pointer_button_down_on() {
                            egui::Color32::from_rgb(64, 70, 84)
                        } else if response.hovered() {
                            egui::Color32::from_rgb(48, 53, 66)
                        } else {
                            egui::Color32::from_rgb(36, 40, 50)
                        },
                    );
                    ui.painter().text(
                        button.center(),
                        egui::Align2::CENTER_CENTER,
                        text,
                        egui::FontId::proportional(16.0),
                        egui::Color32::from_rgb(220, 224, 232),
                    );
                    if response.clicked() {
                        if text == "Report" {
                            open_report = true;
                        } else {
                            cancel = true;
                        }
                    }
                }
                // Whatever is under the cover must keep being drawn: the world
                // is arriving while this is up.
                ctx.request_repaint();
            });
        if open_report {
            self.join_win.open = true;
            self.join_win.show_text = true;
            // The window is opened after the editor's frame has already drawn
            // it this frame, so ask for another: otherwise the tap opens
            // nothing until the world sends another packet.
            ctx.request_repaint();
        }
        if cancel {
            // Same path as the window's own Stop: a join the person gave up on
            // must leave nothing half-made.
            self.join_win.cancel_requested = true;
        }
        if std::mem::take(&mut self.join_win.cancel_requested) {
            self.abandon_join();
        }
    }

    /// Whether the cover has become a banner: Play has started on the
    /// replicated world, so there is a game under it to see and to touch.
    ///
    /// Two ways in, because a join can arrive in either order: the world's
    /// services and a `LocalPlayer` (the ordinary case), or CoreGui live from
    /// the CoreScripts (a place whose first frames are CoreGui-driven). Neither
    /// waits for the character: that is exactly what the banner is *about*.
    fn cover_wants_banner(&self) -> bool {
        if !self.join_win.cover_until_played {
            return false;
        }
        let Some(play) = self.game.play.as_ref() else {
            return false;
        };
        if play.mode != rustudio_engine::runtime::RunMode::Client {
            return false;
        }
        if play.game.core_scripts_active {
            return true;
        }
        let doc = &self.game.doc;
        let root = doc.dom.root_ref();
        let workspace = doc.workspace().is_some();
        let me = doc
            .find_child(root, "Players")
            .and_then(|players| match doc.prop(players, "LocalPlayer") {
                Some(Variant::Ref(p)) if doc.exists(p) => Some(p),
                _ => None,
            });
        workspace && me.is_some()
    }

    /// The cover as a strip: what is still being waited for, over a game that is
    /// already playable.
    ///
    /// It allocates *its own* rectangle and nothing else, so every tap outside
    /// it reaches the game — which is the whole difference from the cover. The
    /// steps are the same three the cover draws, minus the two that are done:
    /// what remains is the character, or a note that the map is still streaming.
    fn join_banner(&mut self, ctx: &egui::Context, phase: &CoverPhase) {
        let waiting: Vec<String> = phase
            .steps
            .iter()
            .filter(|(_, state)| *state == CoverStep::Now)
            .map(|(label, _)| label.clone())
            .collect();
        let text = match waiting.first() {
            Some(label) => format!("{label}…"),
            None => "Still streaming the rest of the place…".to_string(),
        };
        let screen = ctx.content_rect();
        let mut open_report = false;
        egui::Area::new(egui::Id::new("join_banner"))
            .fixed_pos(egui::Pos2::new(screen.center().x - 132.0, screen.min.y + 52.0))
            .order(egui::Order::Middle)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(18, 20, 26))
                    .corner_radius(10)
                    .inner_margin(egui::Margin::symmetric(14, 9))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new(&text)
                                    .color(egui::Color32::from_rgb(214, 219, 230))
                                    .size(15.0),
                            );
                            // The report is the only button: a Cancel would be a
                            // 44-point target over a running game's controls,
                            // and Stop (Shift+F5, or the editor's own) is
                            // already there and already reachable.
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("Report").size(14.0),
                                    )
                                    .min_size(egui::vec2(0.0, 32.0)),
                                )
                                .clicked()
                            {
                                open_report = true;
                            }
                        });
                    });
            });
        if open_report {
            self.join_win.open = true;
            self.join_win.show_text = true;
            // (the editor drew its windows before this frame's banner, so the
            // window has to be asked for again or the tap appears to do nothing)
            ctx.request_repaint();
        }
        // The game under the banner keeps drawing; the wait is a moving thing.
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }

    /// Whether the live client is far enough in to take the cover down:
    /// a session playing, `Workspace`, `LocalPlayer`, and a character bound.
    ///
    /// Deliberately *not* "the map has finished streaming". That is a much
    /// longer wait, and a game someone can already walk around in does not need
    /// to be hidden while the rest of the place arrives behind them.
    fn live_client_ready(&self) -> bool {
        let Some(play) = self.game.play.as_ref() else {
            return false;
        };
        if play.mode != rustudio_engine::runtime::RunMode::Client {
            return false;
        }
        let doc = &self.game.doc;
        let root = doc.dom.root_ref();
        let workspace = doc.workspace().is_some();
        let me = doc
            .find_child(root, "Players")
            .and_then(|players| match doc.prop(players, "LocalPlayer") {
                Some(Variant::Ref(p)) if doc.exists(p) => Some(p),
                _ => None,
            });
        let character = play.character.is_some();
        // `Player.CharacterAutoLoads == false` means the server will not make
        // one until a script asks: a cover that waits for a character that is
        // never coming is a cover that never comes down. (Roblox's own loading
        // screen ends the same way — at the game, not at the character.)
        let auto = match me.and_then(|p| doc.prop(p, "CharacterAutoLoads")) {
            Some(Variant::Bool(auto)) => auto,
            _ => true,
        };
        // A join whose streaming handshake was refused, or that has been
        // waiting a long time, is not one to keep hidden: the game is in and
        // whoever is holding the phone can see what it looks like.
        let stalled = self
            .join_win
            .live
            .as_ref()
            .is_some_and(|c| c.seconds >= 45);
        workspace && me.is_some() && (character || !auto || stalled)
    }

    /// Put the editor's own Show Core UI setting back after a join turned it on
    /// (`start_play` for `RunMode::Client`), once per session.
    pub fn restore_core_gui(&mut self) {
        // (`Some(false)`: the join turned it on over a saved "off"; `Some(true)`
        // is what a person already had, and restoring is a no-op then.)
        if let Some(was) = self.join_win.core_gui_was.take() {
            self.ui.show_core_gui = was;
            self.game.show_core_gui = was;
        }
    }

    /// Give up on a join: drop the session and put the editor back.
    ///
    /// A join the person cancelled must leave nothing half-made — no cover, no
    /// session, and Stop taken so the world it had started building is gone.
    pub fn abandon_join(&mut self) {
        self.join_win.join = None;
        self.join_win.live_world = None;
        self.join_win.live = None;
        self.join_win.cover_until_played = false;
        self.join_win.installed = false;
        self.join_win.live_reported = false;
        self.join_win.live_report_at = None;
        if self.game.play.is_some() {
            self.request_stop();
        }
        self.restore_core_gui();
        self.info("join: cancelled");
    }

    /// What the cover should say, or `None` when no join is in flight.
    ///
    /// The three steps are the three waits, in order, and each is *observed*
    /// rather than timed: a cover that guesses its phase is a cover that lies
    /// about a stall.
    fn join_cover_phase(&self) -> Option<CoverPhase> {
        // A join the person started and the editor has not finished installing.
        let joining = self.join_win.join.as_ref().is_some_and(|j| j.running());
        let playing = self.game.play.is_some();
        if !joining && !playing && !self.join_win.cover_until_played {
            return None;
        }
        let has_world = self.join_win.installed || self.game.doc.dom.len() > 8;
        let stats = self.join_win.join.as_ref().map(|j| j.stats());
        let stage = stats
            .as_ref()
            .map(|s| s.stage_name())
            .unwrap_or("starting");
        let bound = self
            .game
            .play
            .as_ref()
            .and_then(|s| s.character.as_ref())
            .is_some();
        // A session whose world is playing is past the handshake whatever its
        // stage name still says: the stage is the *session's*, and the session
        // keeps streaming long after Play started. (Before this, the cover went
        // on saying "Joining" with a world on screen behind it.)
        let signed_in = playing || !matches!(stage, "idle" | "allocated");
        let mut steps = Vec::with_capacity(3);
        steps.push((
            "Joining".to_string(),
            if signed_in { CoverStep::Done } else { CoverStep::Now },
        ));
        steps.push((
            "Loading world".to_string(),
            match (signed_in, has_world) {
                (true, true) => CoverStep::Done,
                (true, false) => CoverStep::Now,
                _ => CoverStep::Waiting,
            },
        ));
        steps.push((
            "Waiting for your character".to_string(),
            match (has_world, bound) {
                (true, true) => CoverStep::Done,
                (true, false) => CoverStep::Now,
                _ => CoverStep::Waiting,
            },
        ));
        // The note says whether the handshake went, because it is the one
        // number that explains a join that has instances and no character:
        // `stream` is what tells a streaming server the spawn region arrived.
        let note = self.join_win.live.as_ref().map(|c| {
            let mut line = format!(
                "{} instances · {} packets · stream {} · {} s",
                c.instances,
                c.packets,
                if c.streaming_ready { "acked" } else { "no-ack" },
                c.seconds
            );
            if playing {
                line.push_str(" · the rest of the place streams in as you play");
            }
            line
        });
        Some(CoverPhase { steps, note })
    }

    /// The join window, drawn over the editor while it is open.
    pub fn join_window(&mut self, ctx: &egui::Context) {
        // A joined world is installed on one frame and played on the next: F5
        // reads the document, and `set_document` has to have finished with it
        // before Play copies it as its server DataModel.
        if std::mem::take(&mut self.join_win.play_next_tick) {
            if self.game.play.is_some() {
                self.info("join: a session is already running (Shift+F5 stops it)");
            } else if self.place_loading() {
                // A place still loading would defer the Play and lose it.
                self.join_win.play_next_tick = true;
            } else {
                // `RunMode::Client`, not `RunMode::Play`. `Play` is Play *Solo*:
                // it clones the edit DataModel into a local server, runs that
                // server's `Script`s and spawns a character from the place's own
                // SpawnLocation. A person who joined someone else's place is not
                // running that place — a server somewhere else is — and a local
                // copy of it is a second world that nothing replicates to. The
                // native client has no local server at all; it draws the
                // replicated DataModel and binds to the character the server
                // makes.
                //
                // (`rustudio_engine::runtime::RunMode`, not `crate::runtime`'s:
                // `start_play` and `play_when_loaded` use the engine's enum, and
                // the two are separate types with the same variant names.)
                self.start_play(rustudio_engine::runtime::RunMode::Client);
                // **The check the reviewer asked for**: after Play has had the
                // document, does the join's own id map still name this
                // document's instances? If it does not, every later delta is
                // written into refs that belong to nothing — the world arrives
                // once and then stands still while packets keep coming. That is
                // what `entered the placed live` followed by a frozen game was,
                // so it is counted and said out loud rather than assumed.
                let matched = self
                    .join_win
                    .live_local
                    .as_ref()
                    .map(|live| live.matches(&self.game.doc));
                let mapped = self.join_win.live_local.as_ref().map_or(0, |l| l.len());
                match matched {
                    Some(true) => self.info(format!(
                        "join: playing the replicated place — {} instance id(s) still bound to this document; streaming continues",
                        mapped
                    )),
                    Some(false) => self.error(format!(
                        "join: the live id map does not name this document ({} id(s)): later deltas would be lost. Stop and join again.",
                        mapped
                    )),
                    None => {}
                }
                self.info("join: entered the placed live (Shift+F5 leaves)");
            }
        }
        let mut open = self.join_win.open;
        let mut start = false;
        let mut install = false;
        let mut play = false;
        let mut close = false;
        let mut copy_report: Option<String> = None;
        let mut show_text = self.join_win.show_text;
        if !open {
            return;
        }
        let running = self.join_win.running();
        let lines = self
            .join_win
            .join
            .as_ref()
            .map(|join| join.lines())
            .unwrap_or_else(|| vec!["join: not started".to_owned()]);
        let stats = self
            .join_win
            .join
            .as_ref()
            .map(|join| join.stats())
            .unwrap_or_default();
        let verdict = self
            .join_win
            .join
            .as_ref()
            .and_then(|join| join.report())
            .and_then(|report| report.verdict());
        let finished = self.join_win.join.is_some() && !running;
        let installed = self.join_win.installed;
        // A world exists as soon as the session created an instance; it is
        // handed over by `take_document` (once), after which `installed` is
        // true and this stops mattering.
        let has_world = stats.created > 0 || stats.packets > 0;
        let waited = self
            .join_win
            .started
            .map(|at| at.elapsed().as_secs())
            .unwrap_or(0);
        let started = self.join_win.join.is_some();
        let mut place_text = std::mem::take(&mut self.join_win.place_text);
        // (taken out for the same reason: the window's closure holds `self`
        // immutably, and this is written by it.)
        let mut place_focus = std::mem::take(&mut self.join_win.place_focus);

        // `self.join_win` is not touched inside the closure: everything the
        // window draws was taken above, and what it asks for is applied below.
        egui::Window::new("Join a place")
            .open(&mut open)
            .resizable(true)
            .default_width(560.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Place");
                    // **Focus is given, not taken.** On Android a focused text
                    // field *is* the soft keyboard: opening the report from the
                    // join cover put a keyboard over the report, and the taps
                    // meant for its buttons went to the keyboard. So the field
                    // holds the focus only after a tap on it (`place_focus`,
                    // which the window clears when it closes), and a focus it
                    // acquired without one is released here on the frame it
                    // appeared. A desk keeps the old behaviour: the field is
                    // ready to type into as soon as the window opens.
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut place_text)
                            .id(egui::Id::new(PLACE_FIELD_ID))
                            .desired_width(120.0)
                            .hint_text("1818"),
                    );
                    if response.clicked() {
                        place_focus = true;
                    }
                    if !place_focus
                        && ui.memory(|m| m.has_focus(egui::Id::new(PLACE_FIELD_ID)))
                    {
                        ui.memory_mut(|m| m.surrender_focus(egui::Id::new(PLACE_FIELD_ID)));
                    }
                    if ui
                        .add_enabled(!running, egui::Button::new("Join"))
                        .on_hover_text("Sign in, allocate a server, dial it and play")
                        .clicked()
                    {
                        start = true;
                    }
                    if running {
                        ui.spinner();
                        ui.label(
                            egui::RichText::new(format!("joining… {waited}s"))
                                .color(rustudio_engine::theme::TEXT_DIM),
                        );
                    }
                });
                ui.add_space(6.0);
                // The report: one line per stage, the failure named, the stage
                // it reached and the total. This is what a phone cannot
                // otherwise hand back.
                for line in &lines {
                    ui.monospace(line);
                }
                if finished {
                    ui.add_space(6.0);
                    ui.separator();
                    match verdict.clone() {
                        Some(why) => {
                            ui.colored_label(rustudio_engine::theme::ERROR, &why);
                            ui.label(
                                egui::RichText::new(
                                    "Send this window's text back: each named stage is a different \
                                     bug (docs/JOIN-M2.md).",
                                )
                                .color(rustudio_engine::theme::TEXT_DIM),
                            );
                        }
                        None => {
                            ui.colored_label(
                                rustudio_engine::theme::SELECT,
                                "the session ran: the world was replicated",
                            );
                        }
                    }
                    if let Some(at) = self.join_win.copied_at {
                        if at.elapsed().as_secs() < 4 {
                            ui.label(
                                egui::RichText::new("copied — paste it back (a chat, a note)")
                                    .color(rustudio_engine::theme::SELECT),
                            );
                        }
                    }
                    ui.horizontal(|ui| {
                        if ui
                            .button("Copy the report")
                            .on_hover_text("Puts the report on the system clipboard")
                            .clicked()
                        {
                            copy_report = Some(lines.join("\n"));
                        }
                        if ui
                            .button("Show as text")
                            .on_hover_text("If a copy does not reach the clipboard, select this by hand")
                            .clicked()
                        {
                            show_text = !show_text;
                        }
                        // Offered whenever the session built *any* world — a
                        // join that replicated a handful of instances and then
                        // ended is still a join, and the world it built is
                        // what says so. (Before this, only a session with
                        // packets *and* a finished report offered it, so a
                        // session still finishing showed nothing at all.)
                        if has_world && !installed {
                            if ui
                                .button("Open the place")
                                .on_hover_text("Install the world the session built into the editor")
                                .clicked()
                            {
                                install = true;
                            }
                            // The difference is what a joined place is for:
                            // opening installs it as an editable document,
                            // playing runs it with a character, which is what
                            // a person who pressed Join on someone else's
                            // place expects to get.
                            if ui
                                .button("Play the place")
                                .on_hover_text(
                                    "Install the world and start it: server and client, with your character",
                                )
                                .clicked()
                            {
                                install = true;
                                play = true;
                            }
                        }
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    });
                }
                if show_text {
                    ui.add_space(6.0);
                    // Not wrapped, not truncated: a plain box whose text can be
                    // selected, for the case where the clipboard does not take
                    // the copy. Scrolls sideways, as a terminal would.
                    let mut text = lines.join("\n");
                    ui.add(
                        egui::TextEdit::multiline(&mut text)
                            .desired_rows(8)
                            .code_editor()
                            .desired_width(f32::INFINITY),
                    );
                }
                if started {
                    ui.add_space(4.0);
                    // The counters move as packets arrive, so a join that is
                    // working looks different from one that is not; the
                    // deadline says how much longer it will try.
                    let left = SESSION_SECONDS.saturating_sub(waited);
                    let mut line = format!(
                        "stage {} · {} stream(s) · {} packet(s) · {} created · sync {}/{} · {} unhandled",
                        stats.stage_name(),
                        stats.streams,
                        stats.packets,
                        stats.created,
                        stats.sync_replies,
                        stats.sync_requests,
                        stats.unhandled
                    );
                    if running {
                        line.push_str(&format!(" · {} s left", left));
                    }
                    ui.label(
                        egui::RichText::new(line).color(rustudio_engine::theme::TEXT_DIM),
                    );
                }
            });

        // The field's text goes back, and the window's own ✕ is honoured.
        self.join_win.place_text = place_text;
        self.join_win.place_focus = place_focus;
        // Close goes back to the cover while the join is still loading — it
        // opened the window from the cover, and the window closing must not put
        // the editor back. Once nothing is loading it closes for good.
        if close {
            // The window steps aside; the cover (still up, because the join is
            // still loading) takes the screen back on this same frame.
            self.join_win.open = false;
            self.join_win.show_text = false;
        } else {
            self.join_win.open = open && (running || !close);
        }
        if start {
            match self.join_win.place() {
                Some(place) => self.start_join(place),
                None => self.join_win.open = true,
            }
        }
        if install {
            // A button press on a session that is *live* means "show me the
            // world now", and the world is already in the editor: the swap in
            // `live_join_frame` put it there the moment the session had one.
            if !self.join_win.installed {
                self.install_join_world();
            }
        }
        if play {
            // Deferred for the same reason the automatic path is: the install
            // below calls `set_document`, and Play copies the document.
            self.join_win.play_next_tick = true;
        }
        self.join_win.show_text = show_text;
        if let Some(report) = copy_report.clone() {
            // Straight to the system clipboard, not through egui's output
            // commands: the window that runs this is drawn after the frame's
            // copy hook, and the report is the one thing on this page that a
            // person has to be able to get out of the app.
            #[cfg(target_os = "android")]
            crate::android::clipboard::set(report.clone());
            ctx.copy_text(report.clone());
            self.join_win.copied_at = Some(Instant::now());
            self.info(format!(
                "join report copied ({} characters)",
                report.chars().count()
            ));
        }
        // A running join repaints itself; a finished one stops asking.
        if self.join_win.running() {
            ctx.request_repaint_after(Duration::from_millis(150));
            return;
        }
        if !finished || installed {
            return;
        }
        // A finished session that built instances **installs them by itself**:
        // the world is the evidence that the join worked, and before this the
        // only sign of a successful session was a number in a report — the
        // place stayed empty until a button was found. The report still says
        // what happened, and the window stays up over the world.
        // A live session has been showing the place since its first instance:
        // there is nothing to install, and `installed` is already true.
        if !self.join_win.installed && has_world && world_worth_opening(stats.as_ref()) {
            self.install_join_world();
        }
        // A *bounded* join (M2) is played when it finishes: the place arrived in
        // one piece and the session is over, so F5 on the result is what shows
        // it working. A live session is not played that way — the world it draws
        // is the server's, still changing under it, and pressing F5 would copy
        // it into a *local* server DataModel that the server knows nothing
        // about. A live join keeps the world in the editor and keeps it live.
        if let Some(place) = self.join_win.place() {
            if !self.join_win.recent.iter().any(|r| r.place_id == place) {
                self.remember_join(place, self.join_win.installed);
            }
        }
    }

    /// Press Join for `place` (Start Page, the ribbon, `RUSTUDIO_JOIN`).
    pub fn join_place(&mut self, place: u64) {
        self.start_join(place);
    }

    /// Start the join the window's own **Join** button asks for.
    fn start_join(&mut self, place: u64) {
        self.join_win.place_text = place.to_string();
        // **The window closes on the tap, and the cover takes its place.** The
        // window is the *editor's* chrome: leaving it up meant the tap on Join
        // put a window over a Studio 3D view, which is the "it opened the
        // editor's loading chrome" report. A person who joins a game gets a
        // loading screen and then the game; the window with the numbers comes
        // back only if they ask for it (the cover's Report button, below).
        self.join_win.open = false;
        self.join_win.show_text = false;
        // A live session: no deadline (`0`), and a world handover so the place
        // is drawn from the first instance the server sends rather than after a
        // session has ended. M2's bounded join — thirty seconds, then the world
        // — is still reachable for a harness (`RUSTUDIO_JOIN_SECONDS`), and is
        // what a report is taken from.
        let seconds = std::env::var("RUSTUDIO_JOIN_SECONDS")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let live = rustudio_join::live_world::LiveWorld::new();
        self.join_win.live_world = Some(live.clone());
        self.join_win.live = None;
        self.join_win.live_logged_at = None;
        self.join_win.live_report_at = None;
        self.join_win.live_reported = false;
        // A joined place fills the screen, as it does on Roblox: the game is
        // the whole window, not the dock's 3D view with Studio around it. That
        // is also what makes the joining cover a cover — and it is what
        // `play_full_frame` (no Stop or Studio buttons for a live client) draws.
        self.ui.play_full = true;
        self.play_full_exit = false;
        // The cover goes up now, on the tap, not when the first packet lands:
        // what it is hiding is the *editor*, and the editor is what is on
        // screen between those two moments.
        self.join_win.cover_until_played = true;
        // A new session's deltas describe a world built from nothing, so the
        // editor's half starts from nothing too. Carrying the id map across
        // would bind this session's `scope:index` to the previous one's refs.
        self.join_win.live_local = Some(Default::default());
        self.join_win.live_converter = Default::default();
        self.join_win.live_applied = 0;
        // The world the deltas are applied to has to be the one they were built
        // against: a session starts from an empty DataModel, so the editor's
        // document is emptied the same way. `Document::empty()` and not
        // `set_document`, which also tears down the project, the tabs and the
        // selection — a join is not a file being opened, and the place it
        // replaces is a scratch one until the server's arrives.
        self.game.doc = rustudio_core::doc::Document::empty();
        self.join_win.join = Some(rustudio_join::join::start_live(
            place,
            seconds,
            cookie_source(),
            Some(live),
        ));
        self.join_win.started = Some(Instant::now());
        self.join_win.installed = false;
        self.info(format!(
            "join {place}: signing in (the place arrives while it runs{})",
            if seconds == 0 {
                String::new()
            } else {
                format!(", for at most {seconds} s")
            }
        ));
    }

    /// Remember a place this device joined, newest first, one entry per place.
    fn remember_join(&mut self, place: u64, world_installed: bool) {
        let _ = world_installed;
        let entries = &mut self.join_win.recent;
        entries.retain(|r| r.place_id != place);
        entries.insert(
            0,
            crate::start_page::home::RecentJoin {
                place_id: place,
                label: format!("Place {place}"),
                ago_s: 0,
            },
        );
        entries.truncate(8);
    }

    /// Install the world a finished session built into the editor.
    ///
    /// The end of a *bounded* join (M2): the session collected the place and
    /// hands it over once. A live session's world is already being drawn — its
    /// handover has been swapping it in every frame — so this is only reached
    /// when there is nothing to swap and the session has ended, which is the
    /// one case where `take_document` still has a place in it.
    fn install_join_world(&mut self) {
        // Both taken before anything else touches `self`: `join` borrows the
        // window, and `set_document` needs the editor mutably.
        let Some((world, stats)) = self
            .join_win
            .join
            .as_ref()
            .map(|join| (join.take_document(), join.stats()))
        else {
            return;
        };
        let Some(world) = world else {
            self.info("join: the session built no world to open");
            return;
        };
        self.set_document(world);
        self.join_win.installed = true;
        if let Some(place) = self.join_win.place() {
            self.remember_join(place, true);
        }
        self.info(format!(
            "join: {} instance(s) created, {} removed, {} propert{} written",
            stats.created,
            stats.removed,
            stats.properties,
            if stats.properties == 1 { "y" } else { "ies" }
        ));
    }
}

/// The local character's root position in one document, or `None`.
///
/// Free rather than a method so it can be tested against a document with no
/// editor around it: the lookup is four properties and a geometry read, and the
/// only thing that can go wrong is doing it against the wrong player.
fn character_focus_in(doc: &rustudio_core::doc::Document) -> Option<glam::Vec3> {
    // `Players.LocalPlayer` names our Player, and `Player.Character` names
    // the model: the same pair `run`'s character binding uses, so this can
    // never point at somebody else's character.
    let players = doc.dom.get_by_ref(doc.dom.root_ref()).and_then(|root| {
        root.children()
            .iter()
            .find(|r| {
                doc.dom
                    .get_by_ref(**r)
                    .is_some_and(|i| i.class.as_str() == "Players")
            })
            .copied()
    })?;
    let local = match doc.prop(players, "LocalPlayer") {
        Some(Variant::Ref(r)) if !r.is_none() => r,
        _ => return None,
    };
    let character = match doc.prop(local, "Character") {
        Some(Variant::Ref(r)) if !r.is_none() => r,
        _ => return None,
    };
    // The root: `PrimaryPart` when it is named, else a child called
    // `HumanoidRootPart`, else the character's own `CFrame`.
    let root = match doc.prop(character, "PrimaryPart") {
        Some(Variant::Ref(r)) if !r.is_none() => Some(r),
        _ => doc.dom.get_by_ref(character).and_then(|i| {
            i.children().iter().copied().find(|child| {
                doc.dom
                    .get_by_ref(*child)
                    .is_some_and(|c| c.name.as_str() == "HumanoidRootPart")
            })
        }),
    };
    let target = root.unwrap_or(character);
    let frame_of = |r| match doc.prop(r, "CFrame") {
        Some(Variant::CFrame(frame)) => Some(frame),
        _ => None,
    };
    let frame = match frame_of(target) {
        Some(frame) => frame,
        // A `Model` has no `CFrame`; take the first child that has one, as
        // the scene does when it has to place a model.
        _ => {
            let child = doc
                .dom
                .get_by_ref(target)?
                .children()
                .iter()
                .copied()
                .find(|c| frame_of(*c).is_some())?;
            frame_of(child)?
        }
    };
    let p = frame.position;
    (p.x.is_finite() && p.y.is_finite() && p.z.is_finite())
        .then(|| glam::Vec3::new(p.x, p.y, p.z))
    }


