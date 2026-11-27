//! The whole of a real join, in one place: sign in, allocate, dial, log in,
//! receive — with a [`Report`] that says which of those it reached.
//!
//! This module is the seam between the crate's parts and the app. The parts
//! were each proven offline (the allocation parser against recorded bodies, the
//! five packets against rove's encoder, a session against a recording); what no
//! local test can prove is that Roblox accepts the join, and that is what the
//! caller runs here on a device.
//!
//! Two deliberate shapes:
//!
//! - **The work happens on a thread of its own.** `run` builds a
//!   current-thread Tokio runtime — the same one rove's own live session builds
//!   — and blocks on it, so the caller is free to poll [`Join::report`] from a
//!   render loop. Nothing here touches a UI type.
//! - **The cookie comes from a closure, not from this crate.** The app already
//!   holds the account (its own keyring, its own sign-in). Passing
//!   `FnOnce() -> Option<String>` keeps a second cookie store out of the tree and
//!   keeps the value out of every log: it is turned into an `AccountSession`
//!   immediately and never returned.
//!
//! ## What a report means
//!
//! | last step | what it means |
//! |---|---|
//! | `sign in` | no cookie, or Roblox refused it |
//! | `allocate` | the account cannot join this place (or needs a human challenge) |
//! | `connect` | the issued pins or address did not get a WebTransport session |
//! | `log in` | connected, the five packets were written |
//! | `receive` | logged in and now reading; the world's counters are in [`Join::stats`] |
//!
//! A report that stops at `connect` is a transport or pinning bug; one that
//! stops at `receive` is the server ending the session, and its `Stage` says
//! whether it closed before any packet or after.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::allocation::{
    new_join_attempt_id, request_allocation_until_ready, AllocationError, AllocationResult,
    IssuedJoinAllocation, JoinTarget,
};
use crate::diagnostics::Report;
use crate::http::AccountSession;
use crate::session::{self, Stage};

/// The session's own counters, as an overlay shows them while it runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// The stage the session reached.
    pub stage_tag: u8,
    pub packets: usize,
    pub schema: usize,
    pub globals: usize,
    pub replication: usize,
    pub refused: usize,
    pub journal_rows: usize,
    pub created: usize,
    pub removed: usize,
    pub properties: usize,
    pub elapsed_ms: u64,
}

// The stage is what an overlay draws, and the poller only ever sees a copy:
// storing the `&'static str` reason across threads would need a lock for one
// word. A tag is enough to name the state, and the reason is in the report.
const IDLE: u8 = 0;
const ALLOCATED: u8 = 1;
const CONNECTED: u8 = 2;
const LOGGED_IN: u8 = 3;
const RECEIVING: u8 = 4;
const CLOSED: u8 = 5;

fn tag(stage: &Stage) -> u8 {
    match stage {
        Stage::Idle => IDLE,
        Stage::Allocated => ALLOCATED,
        Stage::Connected => CONNECTED,
        Stage::LoggedIn => LOGGED_IN,
        Stage::Receiving => RECEIVING,
        Stage::Closed(_) => CLOSED,
    }
}

impl Stats {
    /// The stage as a word, for a caller that does not want the tag.
    pub fn stage_name(&self) -> &'static str {
        match self.stage_tag {
            ALLOCATED => "allocated",
            CONNECTED => "connected",
            LOGGED_IN => "logged in",
            RECEIVING => "receiving",
            CLOSED => "closed",
            _ => "idle",
        }
    }
}

struct Shared {
    report: Report,
    stats: Stats,
    /// Whether the session is still running; the poller stops asking when it is
    /// not, and the thread has finished then.
    running: AtomicBool,
}

/// A join in progress (or finished). Poll it; do not block on it.
pub struct Join {
    shared: Arc<Mutex<Shared>>,
    /// The session's document, once there is one. `None` until then, so a
    /// caller can wait for a world before replacing what it shows.
    document: Arc<Mutex<Option<rustudio_core::doc::Document>>>,
}

impl Join {
    /// The report so far: one line per stage reached, the failure named.
    pub fn report(&self) -> Option<Report> {
        self.shared.lock().ok().map(|shared| shared.report.clone())
    }

    /// The overlay's lines, or a single line before the first step.
    pub fn lines(&self) -> Vec<String> {
        match self.shared.lock() {
            Ok(shared) => shared.report.lines(),
            Err(_) => vec!["join: the report is locked".to_owned()],
        }
    }

    pub fn stats(&self) -> Stats {
        self.shared.lock().map(|shared| shared.stats).unwrap_or_default()
    }

    /// True while the worker is still going.
    pub fn running(&self) -> bool {
        self.shared
            .lock()
            .map(|shared| shared.running.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    /// The world the session built, if it has one worth showing.
    ///
    /// Taken rather than borrowed: the caller installs it into its editor, and
    /// a live session's document keeps growing while it is open, so the caller
    /// re-takes it each time it wants the current state.
    pub fn take_document(&self) -> Option<rustudio_core::doc::Document> {
        self.document.lock().ok().and_then(|mut slot| slot.take())
    }
}

/// Join `place_id` as the account behind `cookie`, for at most `seconds` of
/// replication, and report every stage.
///
/// `cookie` is called on the worker thread and its value is used and dropped:
/// nothing here returns it, prints it, or keeps it past the sign-in step.
pub fn start(
    place_id: u64,
    seconds: u64,
    cookie: impl FnOnce() -> Option<String> + Send + 'static,
) -> Join {
    let shared = Arc::new(Mutex::new(Shared {
        report: Report::begin(place_id),
        stats: Stats::default(),
        running: AtomicBool::new(true),
    }));
    let document = Arc::new(Mutex::new(None));
    let worker_shared = Arc::clone(&shared);
    let worker_document = Arc::clone(&document);

    std::thread::spawn(move || {
        let outcome = run(place_id, seconds, cookie, &worker_shared, &worker_document);
        let mut shared = match worker_shared.lock() {
            Ok(shared) => shared,
            Err(_) => return,
        };
        // A step that ended without saying so is a bug, not a silent success:
        // name it, so a report never stops in the middle with no reason.
        if let Err(why) = outcome {
            if !shared.report.failed_any() {
                shared.report.failed("join", why);
            }
        }
        shared.running.store(false, Ordering::Relaxed);
        log::info!("join {}: {}", place_id, shared.report.stage);
    });

    Join { shared, document }
}

fn run(
    place_id: u64,
    seconds: u64,
    cookie: impl FnOnce() -> Option<String>,
    shared: &Mutex<Shared>,
    document: &Mutex<Option<rustudio_core::doc::Document>>,
) -> Result<(), String> {
    // 1. Sign in. The value is used here and nowhere else.
    let mut account = match cookie() {
        // A value that cannot go into a header is a sign-in problem, not a
        // network one: say so here rather than let it fall out of `?` and be
        // reported against the join as a whole.
        Some(raw) => match AccountSession::new(&raw) {
            Ok(account) => account,
            Err(error) => {
                let why = describe(&error);
                settle(shared, |report| report.failed("sign in", why));
                return Ok(());
            }
        },
        None => {
            settle(shared, |report| {
                report.failed("sign in", "no account is signed in");
            });
            return Ok(());
        }
    };
    let (user_id, user_name, _display) = match account.authenticated_user() {
        Ok(user) => user,
        Err(error) => {
            let why = describe(&error);
            settle(shared, |report| report.failed("sign in", why));
            return Ok(());
        }
    };
    settle(shared, |report| {
        report.user_id = user_id;
        report.ok("sign in", format!("user {user_id} ({user_name})"));
    });

    // 2. Allocate. The matchmaker decides which server; this asks until it
    //    has one, the same eight tries with a 3 s wait rove's client makes.
    let attempt_id = new_join_attempt_id().map_err(|_| "no entropy for an attempt id".to_owned())?;
    let target = JoinTarget::place(place_id);
    let result = request_allocation_until_ready(
        &mut account,
        &target,
        None,
        &attempt_id,
        user_id,
    );
    let issued: IssuedJoinAllocation = match result {
        Ok(AllocationResult::Ready(issued)) => issued,
        Ok(AllocationResult::Pending { status, http_status }) => {
            settle(shared, |report| {
                report.failed(
                    "allocate",
                    format!("the matchmaker is still placing us (status {status}, http {http_status})"),
                )
            });
            return Ok(());
        }
        Ok(AllocationResult::Challenge { http_status }) => {
            settle(shared, |report| {
                report.failed(
                    "allocate",
                    format!("the account needs a challenge (http {http_status})"),
                )
            });
            return Ok(());
        }
        Ok(AllocationResult::Rejected { http_status, status, message }) => {
            settle(shared, |report| {
                // Both halves: the number is what Roblox's own documentation
                // indexes, and the sentence is what says which refusal it is.
                report.failed(
                    "allocate",
                    format!(
                        "refused (http {http_status}, status {}): {}",
                        status.map_or("none".to_owned(), |s| s.to_string()),
                        message.unwrap_or_else(|| "no message".to_owned())
                    ),
                )
            });
            return Ok(());
        }
        Err(error) => {
            let why = describe(&error);
            settle(shared, |report| report.failed("allocate", why));
            return Ok(());
        }
    };
    let endpoint = issued
        .endpoint
        .map(|address| address.to_string())
        .unwrap_or_else(|| "no endpoint".to_owned());
    if !issued.is_transport_ready() {
        settle(shared, |report| {
            report.failed(
                "allocate",
                format!(
                    "{endpoint} but not transport-ready: endpoint {}, backend {}, token {}, {} pin(s)",
                    yes(issued.endpoint.is_some()),
                    yes(issued.backend.is_some()),
                    yes(issued.token().is_some()),
                    issued.certificate_sha256.len()
                ),
            )
        });
        return Ok(());
    }
    settle(shared, |report| {
        report.ok(
            "allocate",
            format!(
                "{endpoint}, {} pin(s), token yes",
                issued.certificate_sha256.len()
            ),
        )
    });

    // 3–5. Dial, log in, read. On its own runtime, as rove's live session does:
    // a current-thread executor driving one connection.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "the transport runtime could not be started".to_owned())?;
    let mut world = session::Session::new();
    let ran = runtime.block_on(async {
        // `session::run` reports the stage it reached even when it fails, so
        // the report below can say connect-vs-log-in-vs-receive exactly.
        session::run(&issued, &mut world, seconds).await
    });
    let stage = world.stage.clone();
    match tag(&stage) {
        ALLOCATED => settle(shared, |report| report.failed("connect", "the dial did not start")),
        CONNECTED => settle(shared, |report| {
            report.failed("log in", "connected, but the login packets were not all written")
        }),
        LOGGED_IN => {
            settle(shared, |report| report.ok("log in", "5 packets"));
            settle(shared, |report| {
                report.failed("receive", format!("the server closed before any packet ({stage})"))
            });
        }
        RECEIVING | CLOSED => {
            let ingested = world.ingested;
            settle(shared, |report| report.ok("log in", "5 packets"));
            let note = format!(
                "{} packet(s): {} schema, {} globals, {} replication, {} refused",
                ingested.packets,
                ingested.schema,
                ingested.globals,
                ingested.replication,
                ingested.refused
            );
            if ingested.packets == 0 {
                settle(shared, |report| {
                    report.failed("receive", format!("no packet arrived ({stage})"))
                });
            } else {
                settle(shared, |report| report.ok("receive", note));
            }
        }
        _ => settle(shared, |report| {
            report.failed("connect", format!("the session did not reach a stage ({stage})"))
        }),
    }
    let _ = ran;

    // Copied, not moved: `Session` implements `Drop` (it releases a journal
    // lease), and a partly-moved value cannot be dropped.
    let applied = world.applied.clone();
    let ingested = world.ingested;
    let journal_rows = world.journal_rows();
    let stage_tag = tag(&world.stage);
    let elapsed_ms = elapsed_ms(shared);
    // The world is handed over before the counters: a caller that sees a
    // finished report can then take a consistent document.
    let place = world.take_document();
    if let Ok(mut slot) = document.lock() {
        *slot = Some(place);
    }
    if let Ok(mut shared) = shared.lock() {
        shared.stats = Stats {
            stage_tag,
            packets: ingested.packets,
            schema: ingested.schema,
            globals: ingested.globals,
            replication: ingested.replication,
            refused: ingested.refused,
            journal_rows,
            created: applied.created,
            removed: applied.removed,
            properties: applied.properties,
            elapsed_ms,
        };
    }
    Ok(())
}

fn elapsed_ms(shared: &Mutex<Shared>) -> u64 {
    shared
        .lock()
        .map(|shared| shared.report.elapsed().as_millis() as u64)
        .unwrap_or(0)
}

fn settle(shared: &Mutex<Shared>, change: impl FnOnce(&mut Report)) {
    if let Ok(mut shared) = shared.lock() {
        change(&mut shared.report);
    }
}

fn yes(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

/// An allocation error's message, which is a wire shape and never a secret.
fn describe(error: &AllocationError) -> String {
    error.to_string()
}
