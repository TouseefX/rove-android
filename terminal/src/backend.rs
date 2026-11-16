use crate::search::Game;
#[cfg(feature = "embedded-worker")]
use headless_luau::{
    account_auth::AccountSession,
    dashboard::{DashboardControl, DashboardPump},
    join_coordinator::{self, CancellationToken, JoinOutcome},
    live_runtime::RuntimeConfig,
    runtime_owner::Runtime,
};
use serde_json::{json, Value};
#[cfg(feature = "embedded-worker")]
use std::{
    io,
    sync::{mpsc::TryRecvError, Arc},
};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Options {
    pub credential_file: Option<PathBuf>,
    pub seconds: u64,
    pub memory_mib: u32,
    pub physics: bool,
    pub lightweight: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            credential_file: std::env::var_os("ROVE_COOKIE_FILE").map(PathBuf::from),
            seconds: 3600,
            memory_mib: 32,
            physics: true,
            lightweight: false,
        }
    }
}
#[derive(Clone)]
pub enum Target {
    Game(Game),
    Sandbox,
}
impl Target {
    pub fn name(&self) -> &str {
        match self {
            Self::Game(g) => &g.name,
            Self::Sandbox => "local Luau sandbox",
        }
    }
    pub fn place_id(&self) -> Option<u64> {
        match self {
            Self::Game(g) => Some(g.place_id),
            Self::Sandbox => None,
        }
    }
}

pub struct Backend {
    pub child: Child,
    input: ChildStdin,
    pub events: Receiver<Value>,
    pub stopping: Option<Instant>,
}
impl Backend {
    pub fn start(target: &Target, options: &Options) -> Result<Self, String> {
        let executable = worker_executable()?;
        let mut command = Command::new(executable);
        command.arg("--worker").args([
            "--seconds",
            &options.seconds.to_string(),
            "--lua-memory-mib",
            &options.memory_mib.to_string(),
        ]);
        match target {
            Target::Game(g) => {
                command.args(["--place-id", &g.place_id.to_string()]);
            }
            Target::Sandbox => {
                command.arg("--offline");
            }
        }
        if let Some(path) = &options.credential_file {
            command.arg("--credential-file").arg(path);
        }
        if !options.physics {
            command.arg("--no-physics");
        }
        if options.lightweight {
            command.arg("--lightweight");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| "Could not start the Rust client worker")?;
        let input = child.stdin.take().ok_or("Worker input unavailable")?;
        let output = child.stdout.take().ok_or("Worker output unavailable")?;
        let errors = child
            .stderr
            .take()
            .ok_or("Worker error stream unavailable")?;
        let (tx, events) = mpsc::sync_channel(64);
        let errors_tx = tx.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(output);
            loop {
                match read_frame(&mut reader) {
                    Ok(Some(frame)) => {
                        if tx.send(frame).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = tx.send(json!({"kind":"error","message":error}));
                        break;
                    }
                }
            }
        });
        thread::spawn(move || {
            let mut reader = BufReader::new(errors);
            loop {
                let mut line = String::new();
                match reader.by_ref().take(16 * 1024).read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if errors_tx
                            .send(json!({"kind":"notice","message":line}))
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
        Ok(Self {
            child,
            input,
            events,
            stopping: None,
        })
    }
    pub fn send(&mut self, value: Value) -> Result<(), String> {
        serde_json::to_writer(&mut self.input, &value).map_err(|_| "Client input closed")?;
        self.input
            .write_all(b"\n")
            .and_then(|_| self.input.flush())
            .map_err(|_| "Client input closed".into())
    }
    pub fn stop(&mut self) {
        if self.stopping.is_none() {
            let _ = self.send(json!({"op":"leave"}));
            self.stopping = Some(Instant::now());
        }
    }
    pub fn reap_if_finished(&mut self) -> bool {
        if self
            .stopping
            .is_some_and(|at| at.elapsed() > Duration::from_secs(6))
        {
            let _ = self.child.kill();
        }
        self.child.try_wait().ok().flatten().is_some()
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn worker_executable() -> Result<PathBuf, String> {
    let current = std::env::current_exe().map_err(|_| "Cannot find the terminal executable")?;
    #[cfg(feature = "embedded-worker")]
    {
        Ok(current)
    }
    #[cfg(not(feature = "embedded-worker"))]
    {
        let path = std::env::var_os("ROVE_WORKER_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                current.with_file_name(if cfg!(windows) {
                    "rove-session-worker.exe"
                } else {
                    "rove-session-worker"
                })
            });
        if !path.is_file() {
            return Err("Client worker is missing. Run terminal/run.sh --rebuild-worker (run.ps1 on Windows).".into());
        }
        if path.canonicalize().ok() == current.canonicalize().ok() {
            return Err(
                "ROVE_WORKER_BIN must point to the embedded session worker, not the dashboard."
                    .into(),
            );
        }
        Ok(path)
    }
}

// Keep the JSON worker entry point available to protocol clients and tests.
#[cfg(not(feature = "embedded-worker"))]
pub fn worker(_target: Target, _options: Options) -> Result<(), String> {
    let mut command = Command::new(worker_executable()?);
    command.args(std::env::args_os().skip(1));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(format!("Could not start client worker: {}", command.exec()))
    }
    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .map_err(|e| format!("Could not start client worker: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("Client worker exited with {status}"))
        }
    }
}

pub fn read_frame(reader: &mut impl BufRead) -> Result<Option<Value>, String> {
    let mut bytes = Vec::new();
    reader
        .take(2 * 1024 * 1024 + 1)
        .read_until(b'\n', &mut bytes)
        .map_err(|_| "Worker output interrupted")?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > 2 * 1024 * 1024 {
        return Err("Worker message exceeded 2 MiB".into());
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| "Worker sent an invalid message".into())
}
#[cfg(feature = "embedded-worker")]
fn send_stdout(event: &Value) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, event)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

#[cfg(feature = "embedded-worker")]
pub fn worker(target: Target, options: Options) -> Result<(), String> {
    if options.lightweight {
        std::env::set_var("ROVE_LIGHTWEIGHT", "1");
        if std::env::var_os("ROVE_PROPERTY_CACHE_MIB").is_none() {
            std::env::set_var("ROVE_PROPERTY_CACHE_MIB", "1");
        }
    }
    let control = DashboardControl::new();
    let cancellation = CancellationToken::new();
    let input_control = control.clone();
    let input_cancel = cancellation.clone();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        loop {
            let result = match read_frame(&mut stdin) {
                Ok(Some(value)) => match value["op"].as_str() {
                    Some("execute") => value["source"]
                        .as_str()
                        .ok_or_else(|| "Missing source".to_owned())
                        .and_then(|s| input_control.execute(s.to_owned())),
                    Some("status") => value["job_id"]
                        .as_u64()
                        .ok_or_else(|| "Missing job ID".to_owned())
                        .and_then(|id| input_control.status(id)),
                    Some("leave") => break,
                    _ => Err("Unknown console request".into()),
                },
                _ => break,
            };
            if let Err(error) = result {
                input_control.emit(json!({"kind":"error","message":error}));
            }
        }
        input_control.request_leave();
        input_cancel.cancel();
    });
    let run_control = control.clone();
    let run_cancel = cancellation.clone();
    let (tx, rx) = mpsc::sync_channel(1);
    let session = thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match target {
            Target::Sandbox => sandbox(&options, &run_control),
            Target::Game(game) => join(game.place_id, options, &run_control, &run_cancel),
        }))
        .unwrap_or_else(|_| Err("The client worker stopped unexpectedly".into()));
        let _ = tx.send(result);
    });
    let start = Instant::now();
    let mut peak = 0;
    let mut ram_budget = headless_luau::memory_budget::GrowingMemoryBudget::default();
    loop {
        for event in control.drain_events() {
            if send_stdout(&event).is_err() {
                control.request_leave();
                cancellation.cancel();
                return Err("Terminal closed".into());
            }
        }
        let rss = ram_budget.sample_process();
        peak = peak.max(rss.unwrap_or(0));
        let mut sample = control.telemetry();
        sample["elapsed_seconds"] = json!(start.elapsed().as_secs_f64());
        sample["rss_bytes"] = json!(rss);
        sample["peak_sampled_rss_bytes"] = json!(peak);
        sample["ram_budget_bytes"] = json!(ram_budget.limit_bytes());
        sample["ram_buffer_bytes"] = json!(ram_budget.buffer_bytes());
        sample["ram_budget_scope"] = json!("client_process");
        sample["ram_budget_is_soft"] = json!(true);
        if send_stdout(&json!({"kind":"telemetry","data":sample})).is_err() {
            control.request_leave();
            cancellation.cancel();
            return Err("Terminal closed".into());
        }
        match rx.try_recv() {
            Ok(result) => {
                for event in control.drain_events() {
                    let _ = send_stdout(&event);
                }
                let mut report = result.unwrap_or_else(|e| json!({"ok":false,"error":e}));
                ram_budget.observe(peak);
                report["ram_budget_bytes"] = json!(ram_budget.limit_bytes());
                report["ram_buffer_bytes"] = json!(ram_budget.buffer_bytes());
                report["ram_budget_scope"] = json!("client_process");
                report["ram_budget_is_soft"] = json!(true);
                let _ = send_stdout(&json!({"kind":"finished","report":report}));
                let _ = session.join();
                return Ok(());
            }
            Err(TryRecvError::Disconnected) => return Err("Session worker disconnected".into()),
            Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(250)),
        }
    }
}

#[cfg(feature = "embedded-worker")]
fn sandbox(options: &Options, control: &DashboardControl) -> Result<Value, String> {
    let snapshot = serde_json::from_str(include_str!(
        "../../tools/fixtures/rust-rewrite/compact/snapshot.json"
    ))
    .map_err(|_| "Local sandbox fixture is invalid")?;
    let mut runtime = Runtime::new(snapshot, options.memory_mib)?;
    let mut pump = DashboardPump::new(control.clone());
    control.update(json!({"phase":"sandbox","network_used":false}));
    control.emit(json!({"kind":"notice","message":"Local sandbox ready. Official Luau VM; fixture world; no Roblox connection."}));
    let start = Instant::now();
    while !control.leave_requested() && start.elapsed().as_secs() < options.seconds {
        pump.poll(&mut runtime, true)?;
        runtime.poll()?;
        thread::sleep(Duration::from_millis(20));
    }
    pump.poll(&mut runtime, false)?;
    Ok(
        json!({"ok":true,"stage":"sandbox_ended","network_used":false,"elapsed_seconds":start.elapsed().as_secs_f64(),"stopped_by_user":control.leave_requested()}),
    )
}

#[cfg(feature = "embedded-worker")]
fn join(
    place_id: u64,
    options: Options,
    control: &DashboardControl,
    cancel: &CancellationToken,
) -> Result<Value, String> {
    control.update(json!({"phase":"authenticating"}));
    control.emit(json!({"kind":"notice","message":"Checking the saved Roblox session…"}));
    let mut account = match options.credential_file {
        Some(path) => AccountSession::from_private_file(path),
        None => AccountSession::from_default_project(),
    }
    .map_err(|e| e.to_string())?;
    let identity = account.validate().map_err(|e| e.to_string())?;
    let user_id = identity.user_id();
    control.update(json!({"account":identity.name(), "phase":"allocating"}));
    if cancel.is_cancelled() {
        return Ok(json!({"ok":true,"stopped_by_user":true}));
    }
    let (mut account, _) = account.into_parts();
    let target = join_coordinator::lookup_target(&mut account, Some(place_id), None, user_id)
        .map_err(|e| e.to_string())?;
    let outcome = join_coordinator::request_until_ready(&mut account, &target, None, user_id, cancel, |progress| {
        control.emit(json!({"kind":"notice","message":format!("Join attempt {} · {:?} · HTTP {}", progress.attempt, progress.state, progress.http_status)}));
    }).map_err(|e| e.to_string())?;
    match outcome {
        JoinOutcome::Ready { allocation, .. } => {
            if cancel.is_cancelled() { return Ok(json!({"ok":true,"stopped_by_user":true})); }
            control.update(json!({"phase":"connecting"}));
            let config = RuntimeConfig::interactive(options.memory_mib, control.clone())?.with_animation_backend(Arc::new(headless_luau::animation_assets::AnimationAssetBackend::authenticated(headless_luau::engine_services::local_text::default_content_root(), account)));
            let report = headless_luau::live_session::run_with_options(&allocation, options.seconds, Some(config), options.physics)?;
            let mut public = json!({});
            for key in ["ok", "stage", "error", "transport_error", "stopped_by_user", "connected_seconds", "elapsed_seconds", "close_kind", "close_code", "application_close", "receive_failure", "player_spawned", "application_messages_received", "application_ping_requests_received", "application_ping_replies_sent", "peak_kernel_resident_bytes", "owned_part_physics", "physics_contact_packets_sent", "character_physics", "remote_client_replies_sent", "remote_server_calls_sent", "remote_server_results_received", "remote_server_errors_received", "remote_rpc_pending", "remote_rpc_pending_details", "remote_receive_error", "engine_error", "streaming_ready_sent", "instance_state_replication", "unknown_datagrams", "fragmented_datagrams", "unhandled_game_packets", "unreliable_packets_received", "quic_datagram_frames_received", "unreliable_channels_received", "unreliable_channel_mappings", "world_stage_timings", "attribute_receipts", "archivable_receipts", "time_sync", "remote_client_callback_successes", "remote_client_callback_errors", "last_client_callback_error"] {
                if let Some(value) = report.get(key) { public[key] = value.clone(); }
            }
            public["script_catalog"] = report["engine_runtime"]["script_catalog"].clone();
            public["delivery_stage_timings"] = report["engine_runtime"]["delivery_stage_timings"].clone();
            for key in ["frame_stage_timings", "actor_stage_timings"] { public[key] = report["engine_runtime"]["runtime"]["diagnostics"][key].clone(); }
            public["host_stage_timings"] = report["engine_runtime"]["runtime"]["diagnostics"]["host_stage_timings"].clone();
            public["vm_stage_timings"] = report["engine_runtime"]["vm_stage_timings"].clone();
            public["script_runtime"] = report["engine_runtime"]["runtime"]["diagnostics"]["script_runtime"].clone();
            public["engine_runtime_error"] = report["engine_runtime"]["error"].clone();
            Ok(public)
        }
        JoinOutcome::Cancelled { .. } => Ok(json!({"ok":true,"stopped_by_user":true})),
        JoinOutcome::Challenge { http_status, .. } => Err(format!("Roblox requires an account check (HTTP {http_status}); complete it in Roblox before retrying")),
        JoinOutcome::Rejected { http_status, status, .. } => Err(format!("Roblox rejected this join (HTTP {http_status}, status {status:?})")),
        JoinOutcome::PendingExhausted { .. } => Err("Roblox kept the join pending; try again shortly".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protocol_handles_eof_bad_json_and_long_frames() {
        assert!(read_frame(&mut &b""[..]).unwrap().is_none());
        assert_eq!(
            read_frame(&mut &b"{\"op\":\"leave\"}\n"[..])
                .unwrap()
                .unwrap()["op"],
            "leave"
        );
        assert!(read_frame(&mut &b"not json\n"[..]).is_err());
        assert!(read_frame(&mut vec![b'x'; 2 * 1024 * 1024 + 2].as_slice()).is_err());
    }
}
