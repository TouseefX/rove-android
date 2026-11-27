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
    /// When the report was last copied, for the "copied" note.
    pub copied_at: Option<Instant>,
    /// The places this device has joined, newest first — the home page's cards
    /// (`crate::start_page::home`). Not persisted yet: a device's own record of
    /// what it played is enough for the card to be one tap.
    pub recent: Vec<crate::start_page::home::RecentJoin>,
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
    pub fn new() -> Self {
        Self {
            open: false,
            place_text: DEFAULT_PLACE.to_string(),
            join: None,
            started: None,
            installed: false,
            show_text: false,
            copied_at: None,
            recent: Vec::new(),
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
                self.start_play(crate::runtime::RunMode::Play);
                self.info("join: playing the joined place (Shift+F5 stops it)");
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

        // `self.join_win` is not touched inside the closure: everything the
        // window draws was taken above, and what it asks for is applied below.
        egui::Window::new("Join a place")
            .open(&mut open)
            .resizable(true)
            .default_width(560.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Place");
                    ui.add(
                        egui::TextEdit::singleline(&mut place_text)
                            .desired_width(120.0)
                            .hint_text("1818"),
                    );
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
        self.join_win.open = open && (running || !close);
        if start {
            match self.join_win.place() {
                Some(place) => self.start_join(place),
                None => self.join_win.open = true,
            }
        }
        if install {
            self.install_join_world();
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
        if has_world && world_worth_opening(stats.as_ref()) {
            self.install_join_world();
            // A joined place is played, not opened as a project: the person
            // pressed Join on someone else's place, and the thing that says the
            // join worked is their character standing in it.
            self.join_win.play_next_tick = true;
        }
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
        self.join_win.open = true;
        self.join_win.join = Some(rustudio_join::join::start(
            place,
            SESSION_SECONDS,
            cookie_source(),
        ));
        self.join_win.started = Some(Instant::now());
        self.join_win.installed = false;
        self.info(format!("join {place}: signing in"));
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
