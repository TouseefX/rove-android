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
        Some(client.session.cookie().to_header())
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
        let mut open = self.join_win.open;
        let mut start = false;
        let mut install = false;
        let mut close = false;
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
                    ui.horizontal(|ui| {
                        if ui.button("Copy the report").clicked() {
                            ctx.copy_text(lines.join("\n"));
                        }
                        if stats.packets > 0 && !installed {
                            if ui
                                .button("Open the place")
                                .on_hover_text("Install the world the session built into the editor")
                                .clicked()
                            {
                                install = true;
                            }
                        }
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    });
                }
                if started {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "stage {} · {} packet(s) · {} created · {} removed",
                            stats.stage_name(),
                            stats.packets,
                            stats.created,
                            stats.removed
                        ))
                        .color(rustudio_engine::theme::TEXT_DIM),
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
        // A running join repaints itself; a finished one stops asking.
        if self.join_win.running() {
            ctx.request_repaint_after(Duration::from_millis(150));
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
        self.info(format!(
            "join: {} instance(s) created, {} removed, {} propert{} written",
            stats.created,
            stats.removed,
            stats.properties,
            if stats.properties == 1 { "y" } else { "ies" }
        ));
    }
}
