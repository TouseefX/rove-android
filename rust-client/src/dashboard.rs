//! Bounded local dashboard control. No sockets, credentials, or world copies.
use crate::runtime_owner::Runtime;
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

pub const MAX_DASHBOARD_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_EVENTS: usize = 128;
const MAX_EVENT_BYTES: usize = 512 * 1024;

enum Command {
    Execute(String),
    Status(u64),
}

#[derive(Default)]
struct State {
    commands: VecDeque<Command>,
    events: VecDeque<(Value, usize)>,
    bytes: usize,
    dropped: u64,
    telemetry: serde_json::Map<String, Value>,
}
#[derive(Default)]
struct Shared {
    leave: AtomicBool,
    ready: AtomicBool,
    state: Mutex<State>,
}

/// A frontend can submit work without ever owning or blocking the VM thread.
#[derive(Clone, Default)]
pub struct DashboardControl(Arc<Shared>);

impl DashboardControl {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn request_leave(&self) {
        self.0.leave.store(true, Ordering::Release);
    }
    pub fn leave_requested(&self) -> bool {
        self.0.leave.load(Ordering::Acquire)
    }
    pub fn ready(&self) -> bool {
        self.0.ready.load(Ordering::Acquire)
    }
    pub fn execute(&self, source: String) -> Result<(), String> {
        if source.is_empty() || source.len() > MAX_DASHBOARD_SOURCE_BYTES {
            return Err("Console source must be 1 byte to 1 MiB".into());
        }
        self.enqueue(Command::Execute(source))
    }
    pub fn status(&self, job: u64) -> Result<(), String> {
        self.enqueue(Command::Status(job))
    }
    fn enqueue(&self, command: Command) -> Result<(), String> {
        if self.leave_requested() {
            return Err("Session is leaving".into());
        }
        if !self.ready() {
            return Err("Luau is waiting for the live world; try again when ready".into());
        }
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| "Console queue unavailable")?;
        // Only four source buffers may wait behind a busy script.
        if state.commands.len() >= 4 {
            return Err("Console queue is full; wait for the current scripts".into());
        }
        state.commands.push_back(command);
        Ok(())
    }
    pub fn update(&self, fields: Value) {
        if let (Some(fields), Ok(mut state)) = (fields.as_object(), self.0.state.lock()) {
            state.telemetry.extend(fields.clone());
        }
    }
    pub fn telemetry(&self) -> Value {
        let mut result = self
            .0
            .state
            .lock()
            .map(|s| Value::Object(s.telemetry.clone()))
            .unwrap_or_else(|_| json!({}));
        result["luau_ready"] = json!(self.ready());
        result
    }
    pub fn emit(&self, mut event: Value) {
        let mut bytes = serde_json::to_vec(&event).map_or(usize::MAX, |v| v.len());
        if bytes > 64 * 1024 {
            event = json!({"kind":"notice","message":"Large console result omitted (over 64 KiB); use :status for the job outcome"});
            bytes = 160;
        }
        if let Ok(mut state) = self.0.state.lock() {
            while state.events.len() >= MAX_EVENTS || state.bytes + bytes > MAX_EVENT_BYTES {
                if let Some((_, size)) = state.events.pop_front() {
                    state.bytes -= size;
                    state.dropped += 1;
                } else {
                    break;
                }
            }
            state.bytes += bytes;
            state.events.push_back((event, bytes));
        }
    }
    pub fn drain_events(&self) -> Vec<Value> {
        let Ok(mut state) = self.0.state.lock() else {
            return vec![];
        };
        let mut events: Vec<_> = state.events.drain(..).map(|(v, _)| v).collect();
        state.bytes = 0;
        if state.dropped != 0 {
            events.insert(0, json!({"kind":"notice","message":format!("{} older console events dropped while the display was busy", state.dropped)}));
            state.dropped = 0;
        }
        events
    }
}

/// Lives on the VM owner thread. Both local sandbox and live mode use this.
pub struct DashboardPump {
    control: DashboardControl,
    cursor: u64,
}
impl DashboardPump {
    pub fn new(control: DashboardControl) -> Self {
        Self { control, cursor: 0 }
    }
    pub fn poll(&mut self, vm: &mut Runtime, ready: bool) -> Result<(), String> {
        let ready = ready && !self.control.leave_requested();
        self.control.0.ready.store(ready, Ordering::Release);
        if ready {
            let command = self
                .control
                .0
                .state
                .lock()
                .map_err(|_| "Console queue unavailable")?
                .commands
                .pop_front();
            if let Some(command) = command {
                match command {
                    Command::Execute(source) => {
                        // One normal authored job per turn; yielding still uses the live scheduler.
                        let response = vm.call(json!({"op":"execute", "source":source, "timeout_ms":50, "resume_across_frames":true}))?;
                        if response["state_lost"] == true {
                            return Err("Console script lost VM state".into());
                        }
                    }
                    Command::Status(id) => {
                        let reply = vm
                            .status(id)
                            .unwrap_or_else(|e| json!({"ok":false,"error":e}));
                        self.control.emit(json!({"kind":"job","reply":reply}));
                    }
                }
            }
        }
        let events = vm.events(self.cursor);
        self.cursor = events["cursor"].as_u64().unwrap_or(self.cursor);
        if events["overrun"] == true {
            self.control.emit(json!({"kind":"notice","message":"VM console history overran; older output omitted"}));
        }
        if let Some(rows) = events["events"].as_array() {
            for row in rows {
                self.control.emit(json!({"kind":"runtime","reply":row}));
            }
        }
        let summary = vm.summary();
        self.control.update(json!({"active_jobs":summary["active_jobs"],"vm_requests":summary["vm_requests"],"pending_tasks":events["pending_tasks"]}));
        Ok(())
    }
}
impl Drop for DashboardPump {
    fn drop(&mut self) {
        self.control.0.ready.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dashboard_rejects_early_and_oversized_commands_and_bounds_backlog() {
        let c = DashboardControl::new();
        assert!(c.execute("return 1".into()).is_err());
        c.0.ready.store(true, Ordering::Release);
        assert!(c
            .execute("x".repeat(MAX_DASHBOARD_SOURCE_BYTES + 1))
            .is_err());
        for _ in 0..4 {
            c.execute("return 1".into()).unwrap();
        }
        assert!(c.execute("return 2".into()).is_err());
        c.request_leave();
        assert!(c.status(1).is_err());
    }
    #[test]
    fn dashboard_reports_output_loss_and_retains_bounded_events() {
        let c = DashboardControl::new();
        for n in 0..300 {
            c.emit(json!({"kind":"notice", "message":n}));
        }
        let events = c.drain_events();
        assert_eq!(events.len(), MAX_EVENTS + 1);
        assert!(events[0]["message"].as_str().unwrap().contains("172"));
        assert!(c.drain_events().is_empty());
    }
    #[test]
    fn dashboard_runs_real_luau_and_keeps_errors_local_to_the_job() {
        let fixture = serde_json::from_str(include_str!(
            "../../tools/fixtures/rust-rewrite/compact/snapshot.json"
        ))
        .unwrap();
        let mut vm = Runtime::new(fixture, 32).unwrap();
        let c = DashboardControl::new();
        let mut pump = DashboardPump::new(c.clone());
        pump.poll(&mut vm, true).unwrap();
        c.execute("print('dashboard hello'); return 6*7".into())
            .unwrap();
        pump.poll(&mut vm, true).unwrap();
        let output = c.drain_events();
        assert!(
            output.iter().any(|r| r["reply"]["values"] == json!([42.0])),
            "{output:?}"
        );
        c.execute("error('console failure')".into()).unwrap();
        pump.poll(&mut vm, true).unwrap();
        assert!(c.drain_events().iter().any(|r| r["reply"]["ok"] == false));
        c.execute("return 'still alive'".into()).unwrap();
        pump.poll(&mut vm, true).unwrap();
        assert!(c
            .drain_events()
            .iter()
            .any(|r| r["reply"]["values"] == json!(["still alive"])));
    }
}
