use crate::memory::current_process_rss_bytes;
use crate::memory_budget::GrowingMemoryBudget;
use crate::{
    actions::{self, Action, Palette, View},
    backend::{Backend, Options, Target},
    editor::Editor,
    search::{self, Game},
    ui,
};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use ratatui::{backend::CrosstermBackend, Terminal};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    collections::VecDeque,
    fs::File,
    io::{self, Read, Write},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

pub struct Log {
    pub at: f64,
    pub level: &'static str,
    pub text: String,
}
#[derive(Clone)]
pub struct Sample {
    pub time: f64,
    pub client_bytes: u64,
    pub ui_bytes: u64,
    pub rtt_ms: Option<f64>,
}
pub struct Picker {
    pub input: Editor,
    pub filter: Editor,
    pub filtering: bool,
    pub expanded_preview: bool,
    pub results: Vec<Game>,
    pub selected: usize,
    pub searching: bool,
    pub searched_for: String,
    pub message: String,
    pub feedback: Option<String>,
    response: Option<Receiver<(String, Result<Vec<Game>, String>)>>,
    requested: String,
    edited: Instant,
}
impl Default for Picker {
    fn default() -> Self {
        Self {
            input: Editor::default(),
            filter: Editor::default(),
            filtering: false,
            expanded_preview: false,
            results: vec![],
            selected: 0,
            searching: false,
            searched_for: String::new(),
            message: "Type a game name, place ID, or Roblox game link.".into(),
            feedback: None,
            response: None,
            requested: String::new(),
            edited: Instant::now(),
        }
    }
}
impl Picker {
    pub fn matches(&self) -> Vec<(usize, Vec<usize>)> {
        let mut rows: Vec<_> = self
            .results
            .iter()
            .enumerate()
            .filter_map(|(i, game)| {
                let query = if self.filtering {
                    self.filter.text.as_str()
                } else {
                    ""
                };
                crate::fuzzy::match_text(&format!("{} {}", game.name, game.creator), query)
                    .map(|(score, positions)| (i, score, positions))
            })
            .collect();
        if self.filtering && !self.filter.text.is_empty() {
            rows.sort_by_key(|(i, score, _)| (std::cmp::Reverse(*score), *i));
        }
        rows.into_iter()
            .map(|(i, _, positions)| (i, positions))
            .collect()
    }
    pub fn selected_game(&self) -> Option<&Game> {
        self.matches()
            .get(self.selected)
            .and_then(|(i, _)| self.results.get(*i))
    }
    fn paste(&mut self, text: &str) {
        if self.expanded_preview {
            return;
        }
        let input = if self.filtering {
            &mut self.filter
        } else {
            &mut self.input
        };
        input.insert(&text.replace(['\n', '\r'], " "));
        self.changed();
    }
    fn changed(&mut self) {
        self.edited = Instant::now();
        self.selected = 0;
        self.feedback = None;
    }
    fn should_search(&self) -> bool {
        let query = self.input.text.trim();
        !self.searching
            && !self.filtering
            && query != self.requested
            && (2..=200).contains(&query.len())
            && !query.contains("://")
            && !query.bytes().all(|c| c.is_ascii_digit())
            && self.edited.elapsed() >= Duration::from_millis(600)
    }
    fn request(&mut self) {
        if self.searching {
            return;
        }
        let text = self.input.text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        let (tx, rx) = mpsc::sync_channel(1);
        self.response = Some(rx);
        self.requested = text.clone();
        self.searching = true;
        self.message = "Searching Roblox…".into();
        thread::spawn(move || {
            let results = search::search(&text);
            let _ = tx.send((text, results));
        });
    }
}
pub struct App {
    pub images: RefCell<crate::images::Images>,
    pub options: Options,
    pub target: Option<Target>,
    pub telemetry: Value,
    pub samples: VecDeque<Sample>,
    pub logs: VecDeque<Log>,
    pub input: Editor,
    pub picker: Option<Picker>,
    pub help: bool,
    pub palette: Option<Palette>,
    pub view: View,
    pub theme: usize,
    pub graph_window: usize,
    pub log_filter: usize,
    pub toast: Option<(String, Instant)>,
    pub recent: VecDeque<Game>,
    pub log_scroll: usize,
    pub started: Instant,
    pub ui_rss: Option<u64>,
    pub ram_budget: GrowingMemoryBudget,
    pub quitting: bool,
    pub running: bool,
    pub final_report: Option<Value>,
    backend: Option<Backend>,
    pending: Option<Target>,
    exit_seen: Option<Instant>,
    history: VecDeque<String>,
    history_index: Option<usize>,
    draft: String,
}
impl App {
    pub fn new(options: Options, target: Option<Target>) -> Self {
        let mut app = Self {
            images: RefCell::new(crate::images::Images::default()),
            options,
            target: None,
            telemetry: json!({"phase":"idle"}),
            samples: VecDeque::new(),
            logs: VecDeque::new(),
            input: Editor::default(),
            picker: Some(Picker::default()),
            help: false,
            palette: None,
            view: View::Overview,
            theme: 0,
            graph_window: 1,
            log_filter: 0,
            toast: None,
            recent: VecDeque::new(),
            log_scroll: 0,
            started: Instant::now(),
            ui_rss: None,
            ram_budget: GrowingMemoryBudget::default(),
            quitting: false,
            running: false,
            final_report: None,
            backend: None,
            pending: None,
            exit_seen: None,
            history: VecDeque::new(),
            history_index: None,
            draft: String::new(),
        };
        app.log(
            "info",
            "Ready. Find a game with F2, or type :sandbox to try Luau locally.",
        );
        if let Some(target) = target {
            app.start(target);
        }
        app
    }
    pub fn log(&mut self, level: &'static str, text: &str) {
        for line in text.lines().take(80) {
            if self.logs.len() == 400 {
                self.logs.pop_front();
            }
            self.logs.push_back(Log {
                at: self.started.elapsed().as_secs_f64(),
                level,
                text: search::clean(line),
            });
        }
    }
    pub fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
        let mut last_layout = None;
        loop {
            self.poll();
            let layout = (
                terminal.size()?,
                self.view,
                self.theme,
                self.picker.is_some(),
                self.picker.as_ref().is_some_and(|p| p.expanded_preview),
                self.palette.is_some(),
                self.help,
            );
            if last_layout.as_ref() != Some(&layout) {
                terminal.clear()?;
                last_layout = Some(layout);
            }
            terminal.draw(|frame| ui::draw(frame, self))?;
            if self.quitting && self.backend.is_none() {
                return Ok(());
            }
            if event::poll(Duration::from_millis(80))? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => self.key(key),
                    Event::Paste(text) => {
                        if let Some(p) = self.palette.as_mut() {
                            p.input.insert(&text.replace(['\n', '\r'], " "));
                            p.selected = 0;
                        } else if let Some(p) = self.picker.as_mut() {
                            p.paste(&text);
                        } else {
                            self.input.insert(&text);
                        }
                    }
                    Event::Mouse(mouse) => self.mouse(mouse),
                    _ => {}
                }
            }
        }
    }
    pub fn poll(&mut self) {
        {
            let mut images = self.images.borrow_mut();
            if images.enabled() {
                let selected = if let Some(picker) = &self.picker {
                    picker.selected_game()
                } else if let Some(Target::Game(game)) = &self.target {
                    Some(game)
                } else {
                    None
                };
                images.art.select(selected);
                images.poll();
            }
        }
        self.ui_rss = current_process_rss_bytes().ok();
        if !self.running {
            if let Some(ui) = self.ui_rss {
                self.ram_budget.observe(ui);
            }
        }
        if let Some(picker) = self.picker.as_mut() {
            if let Some(result) = picker.response.as_ref().and_then(|rx| rx.try_recv().ok()) {
                picker.searching = false;
                picker.response = None;
                picker.searched_for = result.0;
                match result.1 {
                    Ok(games) => {
                        picker.message = if games.is_empty() {
                            "No games found. Try another name or paste a place ID.".into()
                        } else {
                            format!("{} games · ↑↓ select · Enter join", games.len())
                        };
                        picker.results = games;
                        picker.selected = 0;
                    }
                    Err(error) => {
                        picker.message = error;
                        picker.results.clear();
                    }
                }
            }
            if picker.should_search() {
                picker.request();
            }
        }
        let events: Vec<_> = self
            .backend
            .as_ref()
            .map(|b| b.events.try_iter().take(128).collect())
            .unwrap_or_default();
        for event in events {
            self.observe(event);
        }
        if let Some(backend) = self.backend.as_mut() {
            if backend.reap_if_finished() {
                let at = self.exit_seen.get_or_insert_with(Instant::now);
                // Let the pipe reader deliver the last report after process exit.
                if at.elapsed() > Duration::from_millis(250) {
                    self.backend.take();
                    self.running = false;
                    self.exit_seen = None;
                    self.telemetry["luau_ready"] = json!(false);
                    if self.final_report.is_none() {
                        self.log("warn", "Client process ended without a final report.");
                        self.telemetry["phase"] = json!("ended");
                    }
                    if !self.quitting {
                        if let Some(target) = self.pending.take() {
                            self.start(target);
                        }
                    }
                }
            }
        }
    }
    fn observe(&mut self, event: Value) {
        match event["kind"].as_str().unwrap_or("") {
            "telemetry" => {
                let mut data = event["data"].clone();
                // The TUI is part of the client too. Sum the two measured
                // processes, without confusing the Luau cap with total RAM.
                let total = data["rss_bytes"]
                    .as_u64()
                    .zip(self.ui_rss)
                    .and_then(|(client, ui)| client.checked_add(ui));
                if let Some(total) = total {
                    self.ram_budget.observe(total);
                }
                data["total_rss_bytes"] = json!(total);
                data["ram_budget_bytes"] = json!(self.ram_budget.limit_bytes());
                data["ram_buffer_bytes"] = json!(self.ram_budget.buffer_bytes());
                data["ram_budget_scope"] = json!("client_and_dashboard");
                data["ram_budget_is_soft"] = json!(true);
                if let (Some(time), Some(bytes), Some(ui)) = (
                    data["elapsed_seconds"].as_f64(),
                    data["rss_bytes"].as_u64(),
                    self.ui_rss,
                ) {
                    if self.samples.len() == 14_400 {
                        self.samples.pop_front();
                    }
                    self.samples.push_back(Sample {
                        time,
                        client_bytes: bytes,
                        ui_bytes: ui,
                        rtt_ms: data["rtt_ms"].as_f64(),
                    });
                }
                self.telemetry = data;
            }
            "runtime" | "job" => self.runtime_event(&event["reply"]),
            "error" => self.log("error", event["message"].as_str().unwrap_or("Client error")),
            "notice" => self.log("info", event["message"].as_str().unwrap_or("")),
            "finished" => {
                let report = event["report"].clone();
                self.telemetry["phase"] = json!(if report["ok"] == true {
                    "ended"
                } else {
                    "error"
                });
                self.telemetry["luau_ready"] = json!(false);
                if let Some(error) = report["error"].as_str() {
                    self.log("error", error);
                }
                if let Some(reason) = report["close_kind"].as_str() {
                    self.log(
                        "warn",
                        &format!("Server close: {reason} · code {}", report["close_code"]),
                    );
                }
                if !report["application_close"].is_null() {
                    self.log(
                        "warn",
                        &format!("Application close: {}", report["application_close"]),
                    );
                }
                self.log(
                    "info",
                    &format!(
                        "Session ended · {} · samples kept for :save",
                        report["stage"].as_str().unwrap_or("join ended")
                    ),
                );
                self.final_report = Some(report);
            }
            _ => {}
        }
    }
    fn runtime_event(&mut self, reply: &Value) {
        if let Some(output) = reply["output"].as_str().filter(|s| !s.is_empty()) {
            self.log("print", output);
        }
        if let Some(error) = reply["error"].as_str() {
            self.log("error", error);
        }
        if let Some(console) = reply["console_events"].as_array() {
            for event in console {
                if let Some(text) = event["text"].as_str().or_else(|| event["message"].as_str()) {
                    self.log("print", text);
                }
            }
        }
        if let Some(errors) = reply["task_errors"].as_array() {
            for error in errors {
                self.log("error", error.as_str().unwrap_or(&error.to_string()));
            }
        }
        if let Some(values) = reply["values"].as_array().filter(|v| !v.is_empty()) {
            self.log("result", &Value::Array(values.clone()).to_string());
        }
        if let Some(id) = reply["job_id"].as_u64() {
            self.log(
                "job",
                &format!(
                    "#{id} · {}",
                    reply["state"]
                        .as_str()
                        .unwrap_or(if reply["completed"] == true {
                            "complete"
                        } else {
                            "submitted"
                        })
                ),
            );
        }
        if let Some(completed) = reply["completed_jobs"].as_array() {
            for job in completed {
                // An immediate completion appears both at the top level and
                // in completed_jobs. Show its result only once.
                if reply["job_id"].is_number() && job["job_id"] == reply["job_id"] {
                    continue;
                }
                self.runtime_event(job);
            }
        }
        if reply["output_truncated"] == true || reply["details_omitted"] == true {
            self.log("warn", "Some large script output was omitted.");
        }
    }
    fn start(&mut self, target: Target) {
        self.picker = None;
        self.palette = None;
        if self.backend.is_some() {
            self.pending = Some(target);
            self.leave();
            return;
        }
        self.samples.clear();
        self.ram_budget = GrowingMemoryBudget::default();
        self.final_report = None;
        self.exit_seen = None;
        self.telemetry = json!({"phase":"starting"});
        if let Target::Game(game) = &target {
            self.recent.retain(|g| g.place_id != game.place_id);
            self.recent.push_front(game.clone());
            self.recent.truncate(6);
        }
        self.log("info", &format!("Starting {}…", target.name()));
        match Backend::start(&target, &self.options) {
            Ok(backend) => {
                self.backend = Some(backend);
                self.running = true;
            }
            Err(error) => {
                self.log("error", &error);
                self.telemetry["phase"] = json!("error");
            }
        }
        self.target = Some(target);
    }
    fn leave(&mut self) {
        if let Some(backend) = self.backend.as_mut() {
            backend.stop();
            self.telemetry["phase"] = json!("leaving");
            self.log("info", "Leaving the session…");
        }
    }
    fn key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('k' | 'p') => {
                    self.help = false;
                    self.picker = None;
                    self.palette = if self.palette.is_some() {
                        None
                    } else {
                        Some(Palette::default())
                    };
                    return;
                }
                KeyCode::Char('t') => {
                    self.action(Action::Theme);
                    return;
                }
                KeyCode::Char('e') => {
                    self.action(Action::Filter);
                    return;
                }
                KeyCode::Char('q' | 'c') => {
                    self.quitting = true;
                    self.pending = None;
                    self.leave();
                    return;
                }
                KeyCode::Char('d') => {
                    self.pending = None;
                    self.leave();
                    return;
                }
                KeyCode::Char('l') => {
                    self.logs.clear();
                    self.log_scroll = 0;
                    return;
                }
                _ => {}
            }
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            let view = match key.code {
                KeyCode::Char('1') => Some(View::Overview),
                KeyCode::Char('2') => Some(View::Console),
                KeyCode::Char('3') => Some(View::Network),
                _ => None,
            };
            if let Some(view) = view {
                self.action(Action::View(view));
                return;
            }
        }
        match key.code {
            KeyCode::F(1) => {
                self.help = !self.help;
                return;
            }
            KeyCode::F(2) => {
                self.help = false;
                self.palette = None;
                if self.picker.is_none() {
                    self.picker = Some(Picker::default());
                } else {
                    self.picker = None;
                }
                return;
            }
            KeyCode::F(3) if !self.running => {
                self.options.physics = !self.options.physics;
                return;
            }
            KeyCode::F(4) if !self.running => {
                self.options.lightweight = !self.options.lightweight;
                return;
            }
            KeyCode::F(8) => {
                self.action(Action::Window);
                return;
            }
            _ => {}
        }
        if self.help {
            if key.code == KeyCode::Esc {
                self.help = false;
            }
            return;
        }
        if self.palette.is_some() {
            self.palette_key(key);
            return;
        }
        if self.picker.is_some() {
            self.picker_key(key);
            return;
        }
        match key.code {
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => self.input.insert("\n"),
            KeyCode::Enter => {
                let text = self.input.take();
                self.history_index = None;
                if !text.trim().is_empty() {
                    if self.history.len() == 64 {
                        self.history.pop_front();
                    }
                    // A large pasted script should not be retained a second time.
                    if text.len() <= 16 * 1024 {
                        self.history.push_back(text.clone());
                    }
                    self.command(&text);
                }
            }
            KeyCode::Up => {
                if !self.history.is_empty() {
                    if self.history_index.is_none() {
                        self.draft = self.input.text.clone();
                    }
                    let n = self
                        .history_index
                        .unwrap_or(self.history.len())
                        .saturating_sub(1);
                    self.history_index = Some(n);
                    self.input.set(self.history[n].clone());
                }
            }
            KeyCode::Down => {
                if let Some(n) = self.history_index {
                    if n + 1 < self.history.len() {
                        self.history_index = Some(n + 1);
                        self.input.set(self.history[n + 1].clone());
                    } else {
                        self.history_index = None;
                        self.input.set(self.draft.clone());
                    }
                }
            }
            KeyCode::PageUp => {
                self.log_scroll =
                    (self.log_scroll + 8).min(self.visible_logs().len().saturating_sub(1))
            }
            KeyCode::PageDown => self.log_scroll = self.log_scroll.saturating_sub(8),
            KeyCode::Esc => self.input.set(String::new()),
            _ => edit(&mut self.input, key),
        }
    }
    fn picker_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::F(6) {
            let picker = self.picker.as_mut().unwrap();
            if picker.selected_game().is_some() {
                picker.expanded_preview = !picker.expanded_preview;
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Left => {
                    self.images.borrow_mut().art.step(-1);
                    return;
                }
                KeyCode::Right => {
                    self.images.borrow_mut().art.step(1);
                    return;
                }
                _ => {}
            }
        }
        if key.code == KeyCode::Esc {
            if self.picker.as_ref().is_some_and(|p| p.expanded_preview) {
                self.picker.as_mut().unwrap().expanded_preview = false;
                return;
            }
            if self.picker.as_ref().is_some_and(|p| p.filtering) {
                let p = self.picker.as_mut().unwrap();
                p.filtering = false;
                p.selected = 0;
                p.feedback = None;
                return;
            }
            self.picker = None;
            return;
        }
        let picker = self.picker.as_mut().unwrap();
        if picker.expanded_preview {
            match key.code {
                KeyCode::Left => {
                    self.images.borrow_mut().art.step(-1);
                    return;
                }
                KeyCode::Right => {
                    self.images.borrow_mut().art.step(1);
                    return;
                }
                KeyCode::Up | KeyCode::Down | KeyCode::Enter => {}
                _ => return,
            }
        }
        match key.code {
            KeyCode::Tab => {
                picker.filtering = !picker.filtering;
                picker.selected = 0;
                picker.feedback = None;
            }
            KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Down => {
                picker.selected =
                    (picker.selected + 1).min(picker.matches().len().saturating_sub(1))
            }
            KeyCode::Enter => {
                let text = picker.input.text.trim().to_owned();
                if let Some(game) = search::direct(&text) {
                    self.start(Target::Game(game));
                    return;
                }
                if text.contains("://") || text.bytes().all(|c| c.is_ascii_digit()) {
                    picker.message =
                        "Use a positive place ID or https://www.roblox.com/games/ID link.".into();
                    return;
                }
                if text == picker.searched_for {
                    if let Some(game) = picker.selected_game().cloned() {
                        self.start(Target::Game(game));
                        return;
                    }
                }
                if picker.searching {
                    return;
                }
                if picker.filtering {
                    picker.feedback =
                        Some("No matching result selected. Tab returns to web search.".into());
                } else {
                    picker.request();
                }
            }
            _ => {
                edit(
                    if picker.filtering {
                        &mut picker.filter
                    } else {
                        &mut picker.input
                    },
                    key,
                );
                picker.changed();
            }
        }
    }
    fn command(&mut self, text: &str) {
        self.log_scroll = 0;
        if let Some(command) = text.strip_prefix(':') {
            let (name, arg) = command
                .split_once(char::is_whitespace)
                .unwrap_or((command, ""));
            let arg = arg.trim();
            match name {
                "quit" | "q" => {
                    self.quitting = true;
                    self.pending = None;
                    self.leave();
                }
                "leave" => {
                    self.pending = None;
                    self.leave();
                }
                "help" => self.help = true,
                "commands" => self.palette = Some(Palette::default()),
                "theme" => self.action(Action::Theme),
                "window" => self.action(Action::Window),
                "filter" => self.action(Action::Filter),
                "view" => match arg {
                    "overview" => self.action(Action::View(View::Overview)),
                    "console" => self.action(Action::View(View::Console)),
                    "network" => self.action(Action::View(View::Network)),
                    _ => self.log(
                        "error",
                        "Use :view overview, :view console, or :view network",
                    ),
                },
                "clear" => self.logs.clear(),
                "sandbox" => self.start(Target::Sandbox),
                "join" => {
                    if let Some(game) = search::direct(arg) {
                        self.start(Target::Game(game));
                    } else {
                        self.log("error", "Use :join PLACE_ID or :join ROBLOX_GAME_URL");
                    }
                }
                "search" => {
                    let mut picker = Picker::default();
                    picker.input.set(arg.into());
                    self.picker = Some(picker);
                    if !arg.is_empty() {
                        self.picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                    }
                }
                "status" if arg.is_empty() => self.log(
                    "info",
                    &format!(
                        "{} · {} ping replies / {} requests · {} active jobs",
                        self.telemetry["phase"].as_str().unwrap_or("idle"),
                        display(&self.telemetry["ping_replies"]),
                        display(&self.telemetry["ping_requests"]),
                        display(&self.telemetry["active_jobs"])
                    ),
                ),
                "status" => match arg.parse::<u64>() {
                    Ok(id) => self.send(json!({"op":"status","job_id":id})),
                    Err(_) => self.log("error", "Use :status JOB_ID"),
                },
                "run" => match read_script(arg) {
                    Ok(source) => {
                        self.log("info", "Submitting script file…");
                        self.send(json!({"op":"execute","source":source}));
                    }
                    Err(e) => self.log("error", &e),
                },
                "save" => match self.save(arg) {
                    Ok(()) => self.log("info", "Memory samples saved as CSV."),
                    Err(e) => self.log("error", &e),
                },
                _ => self.log("error", "Unknown command. Press F1 for help."),
            }
        } else {
            self.log(
                "input",
                &format!("> {}", text.chars().take(1024).collect::<String>()),
            );
            self.send(json!({"op":"execute","source":text}));
        }
    }
    fn send(&mut self, command: Value) {
        let result = self
            .backend
            .as_mut()
            .ok_or_else(|| "Join a game first, or use :sandbox for local Luau".to_owned())
            .and_then(|b| b.send(command));
        if let Err(error) = result {
            self.log("error", &error);
        }
    }
    pub fn visible_logs(&self) -> Vec<&Log> {
        self.logs
            .iter()
            .filter(|row| match self.log_filter {
                1 => matches!(row.level, "input" | "print" | "result" | "job" | "error"),
                2 => matches!(row.level, "error" | "warn"),
                _ => true,
            })
            .collect()
    }
    pub fn action(&mut self, action: Action) {
        self.palette = None;
        match action {
            Action::Search => {
                self.help = false;
                self.picker = Some(Picker::default());
            }
            Action::View(view) => {
                self.view = view;
                self.help = false;
                self.picker = None;
            }
            Action::Theme => {
                self.theme = (self.theme + 1) % crate::theme::THEMES.len();
                self.toast = Some((
                    format!("theme · {}", crate::theme::THEMES[self.theme].name),
                    Instant::now(),
                ));
            }
            Action::Window => {
                self.graph_window = (self.graph_window + 1) % 3;
                self.toast = Some((
                    format!("graph · {} seconds", [30, 120, 600][self.graph_window]),
                    Instant::now(),
                ));
            }
            Action::Filter => {
                self.log_filter = (self.log_filter + 1) % 3;
                self.log_scroll = 0;
                self.toast = Some((
                    format!(
                        "activity · {}",
                        ["all events", "scripts", "problems"][self.log_filter]
                    ),
                    Instant::now(),
                ));
            }
            Action::Physics | Action::Lightweight if self.running => {
                self.toast = Some((
                    "leave the current session before changing launch options".into(),
                    Instant::now(),
                ));
            }
            Action::Physics => self.options.physics = !self.options.physics,
            Action::Lightweight => self.options.lightweight = !self.options.lightweight,
            Action::Sandbox => self.start(Target::Sandbox),
            Action::Load => {
                self.picker = None;
                self.input.set(":run ".into());
                self.view = View::Console;
            }
            Action::Export => {
                self.picker = None;
                self.input.set(":save ".into());
            }
            Action::Clear => {
                self.logs.clear();
                self.log_scroll = 0;
            }
            Action::Leave => {
                self.pending = None;
                self.leave();
            }
            Action::Help => self.help = true,
            Action::Quit => {
                self.quitting = true;
                self.pending = None;
                self.leave();
            }
        }
    }
    fn palette_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Esc {
            self.palette = None;
            return;
        }
        let palette = self.palette.as_mut().unwrap();
        let rows = actions::matching(&palette.input.text);
        match key.code {
            KeyCode::Up => palette.selected = palette.selected.saturating_sub(1),
            KeyCode::Down => {
                palette.selected = (palette.selected + 1).min(rows.len().saturating_sub(1))
            }
            KeyCode::Enter => {
                if let Some(index) = rows.get(palette.selected) {
                    self.action(actions::ITEMS[*index].action);
                }
            }
            _ => {
                edit(&mut palette.input, key);
                palette.selected = 0;
            }
        }
    }
    fn mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        use crossterm::event::MouseButton;
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            let down = mouse.kind == MouseEventKind::ScrollDown;
            if self.palette.is_some() {
                self.palette_key(KeyEvent::new(
                    if down { KeyCode::Down } else { KeyCode::Up },
                    KeyModifiers::NONE,
                ));
            } else if self.picker.is_some() {
                self.picker_key(KeyEvent::new(
                    if down { KeyCode::Down } else { KeyCode::Up },
                    KeyModifiers::NONE,
                ));
            } else if down {
                self.log_scroll = self.log_scroll.saturating_sub(3);
            } else {
                self.log_scroll =
                    (self.log_scroll + 3).min(self.visible_logs().len().saturating_sub(1));
            }
            return;
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return;
        }
        let Ok((w, h)) = crossterm::terminal::size() else {
            return;
        };
        match ui::hit(
            self,
            ratatui::layout::Rect::new(0, 0, w, h),
            mouse.column,
            mouse.row,
        ) {
            Some(ui::Hit::Action(action)) => self.action(action),
            Some(ui::Hit::Palette(index)) => self.action(actions::ITEMS[index].action),
            Some(ui::Hit::Game(index)) => {
                if let Some(picker) = self.picker.as_mut() {
                    picker.selected = index;
                }
            }
            Some(ui::Hit::Preview) => {
                self.picker_key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE))
            }
            Some(ui::Hit::Picture(index)) => self.images.borrow_mut().art.picture = index,
            Some(ui::Hit::Join) => {
                self.picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            }
            Some(ui::Hit::Query(query)) => self.command(&format!(":search {query}")),
            Some(ui::Hit::Commands) => {
                self.help = false;
                self.picker = None;
                self.palette = Some(Palette::default());
            }
            _ => {}
        }
    }
    fn save(&self, path: &str) -> Result<(), String> {
        if path.is_empty() {
            return Err("Use :save /path/to/memory.csv (creates a new file)".into());
        }
        if self.samples.is_empty() {
            return Err("No memory samples yet".into());
        }
        let mut file = File::options()
            .create_new(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("Cannot create CSV: {e}"))?;
        writeln!(
            file,
            "elapsed_seconds,client_rss_bytes,dashboard_rss_bytes,total_rss_bytes,rtt_ms"
        )
        .map_err(|e| e.to_string())?;
        for s in &self.samples {
            writeln!(
                file,
                "{:.3},{},{},{},{}",
                s.time,
                s.client_bytes,
                s.ui_bytes,
                s.client_bytes + s.ui_bytes,
                s.rtt_ms.map(|v| format!("{v:.3}")).unwrap_or_default()
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}
pub fn display(value: &Value) -> String {
    if value.is_null() {
        "—".into()
    } else {
        value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string())
    }
}
fn edit(input: &mut Editor, key: KeyEvent) {
    match key.code {
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            input.set(String::new())
        }
        KeyCode::Char(c)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            input.insert(&c.to_string())
        }
        KeyCode::Tab => input.insert("    "),
        KeyCode::Backspace => input.backspace(),
        KeyCode::Delete => input.delete(),
        KeyCode::Left => input.left(),
        KeyCode::Right => input.right(),
        KeyCode::Home => input.cursor = 0,
        KeyCode::End => input.cursor = input.text.len(),
        _ => {}
    }
}
fn read_script(path: &str) -> Result<String, String> {
    let file =
        File::open(path.trim_matches('"')).map_err(|e| format!("Cannot open script: {e}"))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("Script path must be a regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 1024 * 1024 {
        return Err("Dashboard scripts are limited to 1 MiB per submission".into());
    }
    String::from_utf8(bytes).map_err(|_| "Script is not UTF-8".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_search_waits_for_typing_and_does_not_repeat_failed_queries() {
        let mut p = Picker::default();
        p.input.set("obby".into());
        p.changed();
        assert!(!p.should_search());
        p.edited = Instant::now() - Duration::from_secs(1);
        assert!(p.should_search());
        p.requested = "obby".into();
        assert!(
            !p.should_search(),
            "an error must not trigger automatic retry loops"
        );
        for text in [
            "189707",
            "https://www.roblox.com/games/189707/name",
            "x",
            "",
        ] {
            p.input.set(text.into());
            assert!(!p.should_search(), "direct targets wait for Enter: {text}");
        }
    }

    #[test]
    fn result_refinement_selects_the_filtered_game_and_preserves_the_web_query() {
        let mut app = App::new(Options::default(), None);
        let p = app.picker.as_mut().unwrap();
        p.input.set("disaster".into());
        p.searched_for = "disaster".into();
        p.results=search::parse_results(&json!({"searchResults":[{"contentGroupType":"Game","contents":[
            {"rootPlaceId":1,"name":"Disaster Arena"},
            {"rootPlaceId":189707,"name":"Natural Disaster Survival","creatorName":"Stickmasterluke"}
        ]}]})).unwrap();
        app.picker_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.picker.as_mut().unwrap().filter.set("nds".into());
        assert_eq!(app.picker.as_ref().unwrap().matches().len(), 1);
        assert_eq!(
            app.picker
                .as_ref()
                .unwrap()
                .selected_game()
                .unwrap()
                .place_id,
            189707
        );
        app.picker.as_mut().unwrap().filter.set("zzzzzz".into());
        app.picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.running);
        assert!(app.backend.is_none());
        assert!(app.picker.as_ref().unwrap().feedback.is_some());
        app.picker_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.picker.as_ref().unwrap().filtering);
        assert!(app.picker.as_ref().unwrap().feedback.is_none());
        assert_eq!(app.picker.as_ref().unwrap().input.text, "disaster");
    }

    #[test]
    fn expanded_gallery_preserves_search_and_escape_returns_to_results() {
        let mut app = App::new(Options::default(), None);
        app.picker_key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE));
        assert!(!app.picker.as_ref().unwrap().expanded_preview);
        let p = app.picker.as_mut().unwrap();
        p.input.set("disaster".into());
        p.searched_for = "disaster".into();
        p.results.push(search::direct("189707").unwrap());
        app.picker_key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE));
        assert!(app.picker.as_ref().unwrap().expanded_preview);
        app.picker_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        app.picker_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.picker.as_mut().unwrap().paste("hidden paste");
        assert_eq!(app.picker.as_ref().unwrap().input.text, "disaster");
        assert!(!app.picker.as_ref().unwrap().filtering);
        assert!(!app.running && app.backend.is_none());
        app.picker_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.picker.as_ref().unwrap().expanded_preview);
        assert_eq!(
            app.picker
                .as_ref()
                .unwrap()
                .selected_game()
                .unwrap()
                .place_id,
            189707
        );
        app.picker_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.picker.is_none());
    }

    #[test]
    fn stale_results_cannot_join_while_a_new_search_is_pending() {
        let mut app = App::new(Options::default(), None);
        let p = app.picker.as_mut().unwrap();
        p.input.set("new query".into());
        p.searched_for = "old query".into();
        p.searching = true;
        p.results.push(search::direct("189707").unwrap());
        app.picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.running);
        assert!(app.backend.is_none());
    }
    #[test]
    fn whole_client_ram_budget_includes_the_dashboard_and_grows_at_118_mb() {
        let mut app = App::new(Options::default(), None);
        app.ui_rss = Some(18_000_000);
        for (client, expected) in [
            (99_999_999, 128_000_000),
            (100_000_000, 138_000_000),
            (110_000_000, 148_000_000),
            (200_000_000, 238_000_000),
            (10_000_000, 238_000_000),
        ] {
            app.observe(
                json!({"kind":"telemetry","data":{"rss_bytes":client,"elapsed_seconds":1.0}}),
            );
            assert_eq!(app.telemetry["total_rss_bytes"], client + 18_000_000u64);
            assert_eq!(app.telemetry["ram_budget_bytes"], expected);
            assert_eq!(app.telemetry["ram_buffer_bytes"], 10_000_000);
            assert_eq!(app.telemetry["ram_budget_scope"], "client_and_dashboard");
            assert_eq!(app.telemetry["ram_budget_is_soft"], true);
        }
        assert_eq!(app.options.memory_mib, 32, "the Luau cap is separate");
    }

    #[test]
    fn missing_ram_measurements_do_not_invent_a_total() {
        let mut app = App::new(Options::default(), None);
        app.observe(json!({"kind":"telemetry","data":{"rss_bytes":140_000_000}}));
        assert!(app.telemetry["total_rss_bytes"].is_null());
        assert_eq!(app.ram_budget.limit_bytes(), 128_000_000);
        app.ui_rss = Some(18_000_000);
        app.observe(json!({"kind":"telemetry","data":{"rss_bytes":null}}));
        assert!(app.telemetry["total_rss_bytes"].is_null());
        assert!(app.samples.is_empty());
    }

    #[test]
    fn command_palette_changes_views_without_running_or_losing_the_draft() {
        let mut app = App::new(Options::default(), None);
        app.picker = None;
        app.input.set("return 'unsent draft'".into());
        app.key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        app.palette.as_mut().unwrap().input.set("network".into());
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.view, View::Network);
        assert_eq!(app.input.text, "return 'unsent draft'");
        assert!(app.backend.is_none());
        app.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.theme, 1);
    }
    #[test]
    fn activity_filter_and_graph_controls_do_not_discard_measurements() {
        let mut app = App::new(Options::default(), None);
        app.log("print", "normal output");
        app.log("error", "script failure");
        app.action(Action::Filter);
        app.action(Action::Filter);
        assert_eq!(app.visible_logs().len(), 1);
        assert_eq!(app.visible_logs()[0].level, "error");
        let count = app.logs.len();
        app.action(Action::Window);
        assert_eq!(app.graph_window, 2);
        assert_eq!(app.logs.len(), count);
    }
    #[test]
    fn immediate_completion_is_not_printed_twice() {
        let mut app = App::new(Options::default(), None);
        app.runtime_event(&json!({"job_id":1,"state":"completed","values":[42],
            "completed_jobs":[{"job_id":1,"state":"completed","values":[42]}]}));
        assert_eq!(
            app.logs.iter().filter(|row| row.level == "result").count(),
            1
        );
        assert_eq!(app.logs.iter().filter(|row| row.level == "job").count(), 1);
    }
}
