//! The Start Page as a **player's** home, not a Studio one: who you are, one
//! field for a place, and one button that joins it.
//!
//! The Studio-style page (the module above, without the `live` feature) is
//! about opening places to edit: recent files, your experiences, templates,
//! groups. A person who installed this to *join 1818 and see it* is asked for
//! none of that, and the things they do need were two levels down. This page is
//! that person's:
//!
//! ```text
//! ┌───────────────────────────────────────────────────────────────┐
//! │  rustudio                            [avatar] name  [Sign out]│
//! ├───────────────────────────────────────────────────────────────┤
//! │                     Join a place                              │
//! │     [ roblox.com/games/… or a place ID        ]  [   Join  ]  │
//! │                                                               │
//! │  Sign in with the browser (no password typed here)            │
//! │  or paste a .ROBLOSECURITY value    [ paste ]  [ Sign in ]    │
//! │                                                               │
//! │  Recent joins   [card] [card] [card]                          │
//! │  Your places    [card] [card]                                 │
//! │  Open a file…   New place                                     │
//! └───────────────────────────────────────────────────────────────┘
//! ```
//!
//! Everything a join does is in `crate::join_window` (the report, the world);
//! this page only starts one and shows who is signed in. Both ways of signing
//! in are the ones the editor already had — Roblox's browser sign-in (a code,
//! confirmed in the browser) and a pasted cookie — so no new credential path
//! exists here, and the cookie still never reaches a UI module:
//! `rustudio_player::account::Account` holds it.

use egui::{CornerRadius, RichText, Vec2};
use rustudio_engine::theme;

use crate::app::Editor;

/// A join this device has made before, newest first.
#[derive(Clone, Debug, PartialEq)]
pub struct RecentJoin {
    pub place_id: u64,
    /// What the person called it, or the id.
    pub label: String,
    /// Seconds since it was joined.
    pub ago_s: u64,
}

impl RecentJoin {
    /// The line under the card's title.
    fn subtitle(&self) -> String {
        match self.ago_s {
            0 => "just now".to_owned(),
            s if s < 3600 => format!("{} min ago", s / 60),
            s if s < 86_400 => format!("{} h ago", s / 3600),
            s => format!("{} d ago", s / 86_400),
        }
    }
}

/// How much room the page's cards get.
const CARD: Vec2 = Vec2::new(184.0, 108.0);

impl Editor {
    /// Show the player's home page. Returns true while it is up (the caller
    /// draws no editor chrome in that case).
    pub fn player_home_visible(&self) -> bool {
        self.start.visible && self.game.doc.id == self.start.doc_id
    }

    pub fn player_home_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let mut join_place: Option<u64> = None;
        let mut paste_cookie = false;
        let mut sign_in = false;
        let mut sign_out = false;
        let mut open_file = false;
        let mut new_place = false;
        let mut recent_clicked: Option<u64> = None;
        // The field's text, edited inside the closure and put back after.
        let mut place_text = std::mem::take(&mut self.start.url);
        let mut url_error = self.start.url_error.take();

        let signed_in = self.game.account.signed_in();
        let who = self.game.account.status_short();

        egui::Panel::top("home_top")
            .exact_size(56.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::MENU_BAR)
                    .inner_margin(egui::Margin::symmetric(20, 0)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    crate::icons::logo(ui, 28.0);
                    ui.add_space(6.0);
                    ui.label(RichText::new("rustudio").size(19.0).strong().color(theme::TEXT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if signed_in {
                            if ui
                                .add(egui::Button::new(RichText::new("Sign out").color(theme::TEXT)).fill(theme::PANEL_ALT))
                                .clicked()
                            {
                                sign_out = true;
                            }
                            ui.add_space(8.0);
                            ui.label(RichText::new(who.trim_start_matches("Roblox: ")).color(theme::TEXT_DIM));
                        } else if ui
                            .add(egui::Button::new(RichText::new("Sign in").color(theme::TEXT)).fill(theme::PANEL_ALT))
                            .clicked()
                        {
                            sign_in = true;
                        }
                    });
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin { left: 40, right: 40, top: 28, bottom: 20 }),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(6.0);
                    ui.label(RichText::new("Join a place").size(30.0).strong().color(theme::TEXT));
                    ui.label(
                        RichText::new("An official Roblox server, played by this app.")
                            .color(theme::TEXT_DIM),
                    );
                    ui.add_space(14.0);

                    // The one control that matters, full width.
                    ui.horizontal(|ui| {
                        let field = ui.add_sized(
                            [ui.available_width() - 132.0, 40.0],
                            egui::TextEdit::singleline(&mut place_text)
                                .hint_text("roblox.com/games/… or a place ID — 1818")
                                .font(egui::TextStyle::Heading),
                        );
                        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        let join = ui
                            .add_sized([124.0, 40.0], egui::Button::new(RichText::new("Join").strong()))
                            .clicked()
                            || enter;
                        if join {
                            match rustudio_account::parse_place_ref(&place_text) {
                                Ok(p) => join_place = Some(p.place_id),
                                Err(e) => url_error = Some(e),
                            }
                        }
                    });
                    if let Some(e) = &url_error {
                        ui.add_space(4.0);
                        ui.label(RichText::new(e).color(theme::ERROR));
                    }
                    ui.add_space(10.0);

                    // Who is signed in decides what a join would do, so the
                    // sign-in is here, not behind a menu.
                    egui::Frame::new()
                        .fill(theme::PANEL_DARK)
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(egui::Margin::symmetric(16, 12))
                        .show(ui, |ui| {
                            if signed_in {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new("Signed in — a join signs in and allocates with this account.")
                                            .color(theme::TEXT_DIM),
                                    );
                                });
                            } else {
                                ui.label(RichText::new("Sign in to play").strong().color(theme::TEXT));
                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    if ui
                                        .add(egui::Button::new("Sign in with the browser").fill(theme::PANEL_ALT))
                                        .on_hover_text("Roblox shows a code, you confirm it in your browser — no password is typed here")
                                        .clicked()
                                    {
                                        sign_in = true;
                                    }
                                    ui.add_space(8.0);
                                    if ui
                                        .add(egui::Button::new("Paste a cookie").fill(theme::PANEL_ALT))
                                        .on_hover_text("The editor's account window, with the .ROBLOSECURITY field")
                                        .clicked()
                                    {
                                        paste_cookie = true;
                                    }
                                });
                            }
                        });

                    ui.add_space(22.0);
                    // Recent joins: the places this device joined, so the
                    // second join is one tap.
                    if !self.join_win.recent.is_empty() {
                        ui.label(RichText::new("Recent joins").size(18.0).strong().color(theme::TEXT));
                        ui.add_space(8.0);
                        let recent = self.join_win.recent.clone();
                        ui.horizontal_wrapped(|ui| {
                            for r in &recent {
                                let (rect, resp) = ui.allocate_exact_size(CARD, egui::Sense::click());
                                let hovered = resp.hovered();
                                ui.painter().rect_filled(
                                    rect,
                                    CornerRadius::same(8),
                                    if hovered { theme::HOVER_BG } else { theme::PANEL_DARK },
                                );
                                let text = ui.new_child(
                                    egui::UiBuilder::new()
                                        .max_rect(rect.shrink(14.0))
                                        .layout(egui::Layout::top_down(egui::Align::Min)),
                                );
                                text.label(
                                    RichText::new(&r.label).size(16.0).strong().color(theme::TEXT),
                                );
                                text.label(
                                    RichText::new(format!("place {}", r.place_id)).color(theme::TEXT_DIM),
                                );
                                text.label(RichText::new(r.subtitle()).color(theme::ACCENT));
                                if resp.clicked() {
                                    recent_clicked = Some(r.place_id);
                                }
                                ui.add_space(12.0);
                            }
                        });
                        ui.add_space(20.0);
                    }

                    // The editor is still here: a place can be opened to edit
                    // or a new one started, without another way in.
                    ui.horizontal(|ui| {
                        if ui.button("Open a file…").clicked() {
                            open_file = true;
                        }
                        ui.add_space(6.0);
                        if ui.button("New place").clicked() {
                            new_place = true;
                        }
                    });
                });
            });

        self.start.url = place_text;
        self.start.url_error = url_error;

        if let Some(place) = join_place.or(recent_clicked) {
            self.join_place(place);
        }
        if sign_in {
            self.game.account.open = true;
            self.game.account.browser_sign_in();
        }
        if paste_cookie {
            self.game.account.open = true;
            self.game.account.adding = true;
        }
        if sign_out {
            self.game.account.open = true;
        }
        if open_file {
            self.request(crate::app::PendingAction::Open(None));
        }
        if new_place {
            self.request(crate::app::PendingAction::New);
        }
        // The lists behind the cards load on their own thread.
        if self.start.busy() || self.game.account.busy() {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }
    }

    /// Draw the player's home instead of the Studio page, before the editor's
    /// chrome (the same place `start_page_ui` is called from).
    pub fn home_ui(&mut self, ui: &mut egui::Ui) -> bool {
        if !self.player_home_visible() {
            return false;
        }
        self.player_home_ui(ui);
        true
    }
}
