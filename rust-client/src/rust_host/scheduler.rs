//! Runs Luau tasks in small slices so one busy script doesn't hog the VM.

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    rc::Rc,
    time::{Duration, Instant},
};

use mlua::{thread::ThreadStatus, Function, Lua, MultiValue, Result, Table, Thread, Value};

#[path = "deadline_interrupt.rs"]
mod deadline_interrupt;

pub type SchedulerRef = Rc<RefCell<Scheduler>>;

const MAX_PENDING: usize = 16_384;
const MAX_WAIT_SECONDS: f64 = 86_400.0;
const NONYIELDING_LIMIT: Duration = Duration::from_secs(2);

/// Per-coroutine CPU budget. `total` accumulates active VM time across frames;
/// waits do not consume CPU budget. `slice` bounds each scheduler resumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskPolicy {
    pub slice: Duration,
    pub total: Duration,
    pub resume_across_frames: bool,
    pub non_yielding_limit: Duration,
}

impl TaskPolicy {
    pub fn new(slice: Duration, total: Duration, resume_across_frames: bool) -> Result<Self> {
        if slice.is_zero() || total.is_zero() {
            return Err(mlua::Error::RuntimeError(
                "task budgets must be positive".into(),
            ));
        }
        Ok(Self {
            slice: slice.min(total),
            total,
            resume_across_frames,
            non_yielding_limit: NONYIELDING_LIMIT,
        })
    }
}

impl Default for TaskPolicy {
    fn default() -> Self {
        Self {
            slice: Duration::from_millis(5),
            total: Duration::from_millis(250),
            resume_across_frames: true,
            non_yielding_limit: NONYIELDING_LIMIT,
        }
    }
}

/// Per-thread result envelope; tuples remain MLua values and keep trailing
/// nils, tables, functions, and userdata intact until the host consumes them.
#[derive(Debug, Clone)]
pub enum TaskOutcome {
    Completed(MultiValue),
    Yielded(MultiValue),
    Error(String),
    TimedOut(String),
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStateKind {
    Ready,
    Running,
    Sleeping,
    Waiting,
    Completed,
    Error,
    Cancelled,
}

impl TaskStateKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Sleeping => "sleeping",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TaskDiagnostics {
    pub state: Option<TaskStateKind>,
    pub resumes: u64,
    pub yields: u64,
    pub slice_interruptions: u64,
    pub execution_ms: f64,
}

/// Point-in-time queue census. This scheduler does not assign work to script
/// owners, so owner attribution is reported as unavailable rather than guessed.
#[derive(Debug, Clone, Default)]
pub struct SchedulerSnapshot {
    pub active_tasks: usize,
    pub pending_tasks: usize,
    pub ready: usize,
    pub sleeping: usize,
    pub waiting: usize,
    pub running: usize,
    pub cancelled: usize,
    pub done: usize,
    pub done_completed: usize,
    pub done_errors: usize,
    pub done_cancelled: usize,
    pub ready_queue_entries: usize,
    pub sleeping_queue_entries: usize,
    pub queue_limit: usize,
    pub active_task_limit: usize,
    pub outcome_records: usize,
    pub outcome_limit: usize,
    pub diagnostic_records: usize,
    pub diagnostic_history_limit: usize,
    pub ready_owner_attribution_available: bool,
}

#[derive(Default)]
pub struct Scheduler {
    ready: VecDeque<Pending>,
    sleeping: Vec<Pending>,
    tasks: HashMap<usize, TaskState>,
    outcomes: HashMap<usize, TaskOutcome>,
    outcome_order: VecDeque<usize>,
    diagnostics: HashMap<usize, TaskDiagnostics>,
    diagnostic_order: VecDeque<usize>,
    running: std::collections::HashSet<usize>,
    next_sequence: u64,
    last_interactive_sequence: Option<u64>,
    marker: Option<Table>,
    preempt_marker: Option<Table>,
    errors: VecDeque<String>,
    total_resumes: u64,
    total_preemptions: u64,
    active_policy: Option<TaskPolicy>,
    contexts: Option<Table>,
}

struct Pending {
    thread: Thread,
    args: MultiValue,
    due: Option<Instant>,
    wake: Option<Wake>,
    sequence: u64,
}

enum Wake {
    Wait { started: Instant },
    Delay,
}

struct TaskState {
    cancelled: bool,
    parked: bool,
    uninterrupted: Duration,
    cpu_elapsed: Duration,
    policy: TaskPolicy,
    retain_outcome: bool,
    interactive_eligible: bool,
    identity: i64,
    owner: String,
}

impl Default for TaskState {
    fn default() -> Self {
        Self {
            cancelled: false,
            parked: false,
            uninterrupted: Duration::ZERO,
            cpu_elapsed: Duration::ZERO,
            policy: TaskPolicy::default(),
            retain_outcome: false,
            interactive_eligible: false,
            identity: 8,
            owner: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct StepReport {
    pub execution_time: Duration,
    pub resumed: usize,
    pub completed: usize,
    pub preempted: usize,
    pub sleeping: usize,
    pub waiting: usize,
    pub ready: usize,
    pub errors: Vec<String>,
}

impl Scheduler {
    pub fn new() -> Self {
        Self::default()
    }

    fn ensure_marker(&mut self, lua: &Lua) -> Result<Table> {
        if let Some(marker) = &self.marker {
            return Ok(marker.clone());
        }
        let marker = lua.create_table()?;
        self.marker = Some(marker.clone());
        Ok(marker)
    }

    fn ensure_contexts(&mut self, lua: &Lua) -> Result<Table> {
        if let Some(contexts) = &self.contexts {
            return Ok(contexts.clone());
        }
        let metatable = lua.create_table()?;
        metatable.set("__mode", "k")?;
        let contexts = lua.create_table()?;
        contexts.set_metatable(Some(metatable))?;
        self.contexts = Some(contexts.clone());
        Ok(contexts)
    }

    fn enqueue(
        &mut self,
        thread: Thread,
        args: MultiValue,
        due: Option<Instant>,
        policy: TaskPolicy,
    ) -> Result<()> {
        let key = thread_key(&thread);
        let is_active = self.tasks.contains_key(&key);
        if self.tasks.len() >= MAX_PENDING && !is_active {
            return Err(mlua::Error::RuntimeError(
                self.queue_limit_diagnostic(),
            ));
        }
        if !is_active {
            // Thread pointers are reused after a coroutine is collected. A new
            // task with that pointer must not inherit the old task's counters
            // or retained result.
            self.clear_stale_task_records(key);
        }
        self.tasks.entry(key).or_insert_with(|| TaskState {
            policy,
            ..TaskState::default()
        });
        if !self.diagnostics.contains_key(&key) {
            while self.diagnostic_order.len() >= MAX_PENDING {
                let oldest_inactive = self
                    .diagnostic_order
                    .iter()
                    .position(|old| !self.tasks.contains_key(old));
                let Some(index) = oldest_inactive else { break };
                if let Some(oldest) = self.diagnostic_order.remove(index) {
                    self.diagnostics.remove(&oldest);
                }
            }
            if self.diagnostic_order.len() < MAX_PENDING {
                self.diagnostic_order.push_back(key);
            }
        }
        self.diagnostics.entry(key).or_default().state = Some(if due.is_some() {
            TaskStateKind::Sleeping
        } else {
            TaskStateKind::Ready
        });
        let item = Pending {
            thread,
            args,
            due,
            wake: None,
            sequence: self.next_sequence,
        };
        self.next_sequence = self.next_sequence.wrapping_add(1);
        if due.is_some() {
            self.sleeping.push(item);
        } else {
            self.ready.push_back(item);
        }
        Ok(())
    }

    fn clear_stale_task_records(&mut self, key: usize) {
        if self.tasks.contains_key(&key) {
            return;
        }
        self.diagnostics.remove(&key);
        self.diagnostic_order.retain(|queued| *queued != key);
        self.outcomes.remove(&key);
        self.outcome_order.retain(|queued| *queued != key);
    }

    fn record_error(&mut self, error: String) {
        if self.errors.len() == 64 {
            self.errors.pop_front();
        }
        self.errors.push_back(error);
    }

    fn record_outcome(&mut self, key: usize, outcome: TaskOutcome) {
        if !self.tasks.get(&key).is_some_and(|task| task.retain_outcome) {
            return;
        }
        if !self.outcomes.contains_key(&key) {
            while self.outcome_order.len() >= MAX_PENDING {
                if let Some(oldest) = self.outcome_order.pop_front() {
                    self.outcomes.remove(&oldest);
                }
            }
            self.outcome_order.push_back(key);
        }
        self.outcomes.insert(key, outcome);
    }

    fn publish_outcome(&mut self, key: usize, outcome: TaskOutcome) {
        if self.tasks.get(&key).is_some_and(|task| task.retain_outcome) {
            self.record_outcome(key, outcome);
        }
    }

    fn set_diagnostic_state(&mut self, key: usize, state: TaskStateKind) {
        self.diagnostics.entry(key).or_default().state = Some(state);
    }

    fn snapshot(&self) -> SchedulerSnapshot {
        let mut snapshot = SchedulerSnapshot {
            active_tasks: self.tasks.len(),
            ready: self.ready.len(),
            sleeping: self.sleeping.len(),
            running: self.running.len(),
            cancelled: self.tasks.values().filter(|task| task.cancelled).count(),
            ready_queue_entries: self.ready.len(),
            sleeping_queue_entries: self.sleeping.len(),
            queue_limit: MAX_PENDING,
            active_task_limit: MAX_PENDING,
            outcome_records: self.outcomes.len(),
            outcome_limit: MAX_PENDING,
            diagnostic_records: self.diagnostics.len(),
            diagnostic_history_limit: MAX_PENDING,
            ready_owner_attribution_available: false,
            ..SchedulerSnapshot::default()
        };
        snapshot.waiting = self
            .tasks
            .values()
            .filter(|task| task.parked && !task.cancelled)
            .count();
        snapshot.pending_tasks =
            snapshot.ready + snapshot.sleeping + snapshot.waiting + snapshot.running;
        for diagnostic in self.diagnostics.values() {
            match diagnostic.state {
                Some(TaskStateKind::Completed) => snapshot.done_completed += 1,
                Some(TaskStateKind::Error) => snapshot.done_errors += 1,
                Some(TaskStateKind::Cancelled) => snapshot.done_cancelled += 1,
                _ => {}
            }
        }
        snapshot.done = snapshot.done_completed + snapshot.done_errors + snapshot.done_cancelled;
        snapshot
    }

    /// Bound diagnostics to counters, sanitized source labels and five sample
    /// rows. Never print VM pointers, callback arguments, or script source.
    fn queue_limit_diagnostic(&self) -> String {
        let ready = self.ready.iter().map(|item| thread_key(&item.thread)).collect::<HashSet<_>>();
        let sleeping = self.sleeping.iter().map(|item| thread_key(&item.thread)).collect::<HashSet<_>>();
        let mut phases = BTreeMap::<&'static str, usize>::new();
        let mut owners = BTreeMap::<String, usize>::new();
        let mut samples = Vec::<(String, &'static str, u64, f64, u64, u64)>::new();

        for (key, task) in &self.tasks {
            let phase = if self.running.contains(key) {
                "running"
            } else if ready.contains(key) {
                "ready"
            } else if sleeping.contains(key) {
                "sleeping"
            } else if task.parked {
                "parked"
            } else {
                "other"
            };
            *phases.entry(phase).or_default() += 1;
            *owners.entry(safe_owner_label(&task.owner)).or_default() += 1;
            let diagnostic = self.diagnostics.get(key);
            samples.push((
                safe_owner_label(&task.owner),
                phase,
                diagnostic.map_or(0, |row| row.resumes),
                diagnostic.map_or(0.0, |row| row.execution_ms),
                diagnostic.map_or(0, |row| row.slice_interruptions),
                diagnostic.map_or(0, |row| row.yields),
            ));
        }
        samples.sort_unstable_by(|left, right| {
            left.0.cmp(&right.0).then_with(|| left.1.cmp(right.1))
        });

        let phase_counts = ["ready", "sleeping", "parked", "running", "other"]
            .into_iter()
            .map(|phase| format!("{phase}={}", phases.get(phase).copied().unwrap_or(0)))
            .collect::<Vec<_>>()
            .join(",");
        let mut owner_counts = owners.into_iter().collect::<Vec<_>>();
        owner_counts.sort_unstable_by(|left, right| {
            right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0))
        });
        let owner_totals = owner_counts.iter().cloned().collect::<HashMap<_, _>>();
        samples.sort_unstable_by(|left, right| {
            owner_totals.get(&right.0).cmp(&owner_totals.get(&left.0))
                .then_with(|| left.0.cmp(&right.0))
                .then_with(|| left.1.cmp(right.1))
                .then_with(|| left.2.cmp(&right.2))
        });
        let mut selected_samples = Vec::new();
        let mut sampled_owners = HashSet::new();
        for sample in &samples {
            if sampled_owners.insert(sample.0.as_str()) {
                selected_samples.push(sample);
                if selected_samples.len() == 5 {
                    break;
                }
            }
        }
        let mut owner_labels = owner_counts.iter().take(8)
            .map(|(owner, count)| format!("{owner}:{count}"))
            .collect::<Vec<_>>();
        if owner_counts.len() > 8 {
            let omitted = owner_counts.iter().skip(8).map(|(_, count)| count).sum::<usize>();
            owner_labels.push(format!("omitted:{omitted}"));
        }
        let sample_rows = selected_samples.iter().enumerate()
            .map(|(index, (owner, phase, resumes, execution_ms, preemptions, yields))| {
                format!("t{}:{phase}:{owner}:n{}:r{resumes}:cpu{execution_ms:.0}ms:p{preemptions}:y{yields}", index + 1, owner_totals.get(owner).copied().unwrap_or(0))
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "scheduler queue limit reached (active={} {}; owners=[{}]; sample=[{}])",
            self.tasks.len(), phase_counts, owner_labels.join(","), sample_rows
        )
    }
}

fn safe_owner_label(owner: &str) -> String {
    if owner.is_empty() {
        return "local".into();
    }
    owner.chars().take(32).map(|character| {
        if character.is_ascii_alphanumeric() || matches!(character, ':' | '_' | '-' | '.') {
            character
        } else {
            '_'
        }
    }).collect()
}

fn thread_key(thread: &Thread) -> usize {
    thread.to_pointer() as usize
}

fn lua_number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_integer().map(|number| number as f64))
}

pub(super) fn current_policy(lua: &Lua) -> TaskPolicy {
    lua.app_data_ref::<SchedulerRef>()
        .map(|scheduler| scheduler.borrow().active_policy.unwrap_or_default())
        .unwrap_or_default()
}

/// Schedule a local function using the active parent policy or the default.
pub fn enqueue(lua: &Lua, function: Function, args: MultiValue) -> Result<Thread> {
    enqueue_inner(lua, function, args, current_policy(lua), false, None, None)
}

/// Schedule a local function with a per-thread execution budget.
pub fn enqueue_with_policy(
    lua: &Lua,
    function: Function,
    args: MultiValue,
    policy: TaskPolicy,
) -> Result<Thread> {
    enqueue_inner(lua, function, args, policy, true, None, None)
}

/// Enqueue a source-owned script. Native host semantics assign identity 2 when
/// a thread first receives a `script:` owner; this is metadata only and grants
/// no engine or server permission.
pub fn enqueue_owned(
    lua: &Lua,
    function: Function,
    args: MultiValue,
    owner: String,
) -> Result<Thread> {
    enqueue_inner(
        lua,
        function,
        args,
        current_policy(lua),
        false,
        Some(owner),
        None,
    )
}

/// Enqueue a callback while inheriting its creator coroutine's local metadata.
pub fn enqueue_from(
    lua: &Lua,
    function: Function,
    args: MultiValue,
    creator: Option<&Thread>,
) -> Result<Thread> {
    enqueue_inner(
        lua,
        function,
        args,
        current_policy(lua),
        false,
        None,
        creator,
    )
}

/// Invoke a connection callback immediately, like RBXScriptConnection:Fire.
/// It still runs in its own tracked child coroutine so callback identity,
/// ownership, budgets, and yield handling match ordinary scheduled work.
/// Other ready callbacks remain queued while this one gets its first resume.
pub(crate) fn callback_now_from(
    lua: &Lua,
    function: Function,
    args: MultiValue,
    creator: Option<&Thread>,
) -> Result<()> {
    let thread = enqueue_from(lua, function, args, creator)?;
    let scheduler = lua
        .app_data_ref::<SchedulerRef>()
        .ok_or_else(|| mlua::Error::RuntimeError("scheduler is not installed".into()))?
        .clone();
    let _ = step_specific_thread(lua, &scheduler, Instant::now(), Duration::from_millis(100), &thread);
    Ok(())
}

/// Start a required ModuleScript in its own source context while preserving
/// the caller's scheduler identity. Module code has a real `script` Instance,
/// but requiring it does not grant or raise engine capabilities.
pub(super) fn enqueue_module(
    lua: &Lua,
    function: Function,
    args: MultiValue,
    module_id: &str,
    module_instance: Value,
) -> Result<Thread> {
    if module_id.is_empty() || module_instance.is_nil() {
        return Err(mlua::Error::RuntimeError(
            "ModuleScript owner metadata is unavailable".into(),
        ));
    }
    let thread = enqueue(lua, function, args)?;
    let owner = format!("script:{module_id}");
    let context = current_context(lua, &thread).ok_or_else(|| {
        mlua::Error::RuntimeError("scheduled ModuleScript context is unavailable".into())
    })?;
    // Keep the identity assigned by enqueue() from the requiring coroutine.
    // Only replace source ownership with the ModuleScript whose environment
    // the compiled function actually uses.
    context.set("owner", owner.clone())?;
    context.set("script", module_instance)?;
    if let Some(scheduler) = lua.app_data_ref::<SchedulerRef>().map(|value| value.clone()) {
        if let Some(task) = scheduler.borrow_mut().tasks.get_mut(&thread_key(&thread)) {
            task.owner = owner;
        }
    }
    super::script_threads::retain(lua, &thread)?;
    Ok(thread)
}

fn enqueue_inner(
    lua: &Lua,
    function: Function,
    args: MultiValue,
    policy: TaskPolicy,
    retain_outcome: bool,
    owner: Option<String>,
    creator: Option<&Thread>,
) -> Result<Thread> {
    if policy.slice.is_zero() || policy.total.is_zero() || policy.non_yielding_limit.is_zero() {
        return Err(mlua::Error::RuntimeError(
            "task budgets must be positive".into(),
        ));
    }
    let policy = TaskPolicy {
        slice: policy.slice.min(policy.total),
        ..policy
    };
    let thread = lua.create_thread(function.clone())?;
    let scheduler = lua
        .app_data_ref::<SchedulerRef>()
        .ok_or_else(|| mlua::Error::RuntimeError("scheduler is not installed".into()))?
        .clone();
    {
        let mut state = scheduler.borrow_mut();
        let parent_thread = creator.cloned().unwrap_or_else(|| lua.current_thread());
        let parent_context = state.contexts.as_ref().and_then(|contexts| {
            contexts
                .get::<Option<Table>>(parent_thread.clone())
                .ok()
                .flatten()
        });
        let inherited_identity = parent_context
            .as_ref()
            .and_then(|ctx| ctx.get::<i64>("identity").ok())
            .unwrap_or(8);
        let inherited_owner = parent_context
            .as_ref()
            .and_then(|ctx| ctx.get::<String>("owner").ok())
            .unwrap_or_default();
        let source_owned = owner
            .as_deref()
            .is_some_and(|value| value.starts_with("script:"));
        let child_owner = owner.unwrap_or(inherited_owner);
        let inherited_script = parent_context
            .as_ref()
            .and_then(|ctx| ctx.get::<Value>("script").ok())
            .filter(|value| !value.is_nil());
        let script = if source_owned {
            function
                .environment()
                .and_then(|environment| environment.get::<Value>("script").ok())
                .filter(|value| !value.is_nil())
        } else if child_owner.starts_with("script:") {
            inherited_script
        } else {
            None
        };
        let child_identity = if child_owner.starts_with("script:")
            && child_owner
                != parent_context
                    .as_ref()
                    .and_then(|ctx| ctx.get::<String>("owner").ok())
                    .unwrap_or_default()
        {
            2
        } else {
            inherited_identity
        };
        state.enqueue(thread.clone(), args, None, policy)?;
        let contexts = state.ensure_contexts(lua)?;
        let context = lua.create_table()?;
        context.set("identity", child_identity)?;
        context.set("owner", child_owner.clone())?;
        if let Some(script) = &script {
            context.set("script", script.clone())?;
        }
        contexts.set(thread.clone(), context)?;
        if let Some(task) = state.tasks.get_mut(&thread_key(&thread)) {
            task.retain_outcome = retain_outcome;
            task.interactive_eligible = retain_outcome;
            task.identity = child_identity;
            task.owner = child_owner;
        }
        if script.is_some() {
            super::script_threads::retain(lua, &thread)?;
        }
    }
    Ok(thread)
}

/// Returns the current scheduler-tracked coroutine's identity. Untracked host
/// and direct `coroutine.create` threads use the native default identity 8.
pub fn thread_identity(lua: &Lua) -> i64 {
    let current = lua.current_thread();
    current_context(lua, &current)
        .and_then(|context| context.get::<i64>("identity").ok())
        .unwrap_or(8)
}

pub fn set_thread_identity(lua: &Lua, identity: i64) -> Result<()> {
    let current = lua.current_thread();
    let scheduler = lua
        .app_data_ref::<SchedulerRef>()
        .ok_or_else(|| mlua::Error::RuntimeError("thread security context unavailable".into()))?
        .clone();
    let mut state = scheduler.borrow_mut();
    let contexts = state
        .contexts
        .clone()
        .ok_or_else(|| mlua::Error::RuntimeError("thread security context unavailable".into()))?;
    let context: Option<Table> = contexts.get(current.clone())?;
    let context = context
        .ok_or_else(|| mlua::Error::RuntimeError("thread security context unavailable".into()))?;
    context.set("identity", identity)?;
    if let Some(task) = state.tasks.get_mut(&thread_key(&current)) {
        task.identity = identity;
    }
    Ok(())
}

/// Capability membership follows the native scheduler's identity map. Unknown
/// identity numbers deliberately receive no engine capabilities.
pub fn has_capability(lua: &Lua, capability: &str) -> bool {
    let identity = thread_identity(lua);
    match capability {
        "None" => true,
        "Plugin" => matches!(identity, 1 | 3 | 4 | 5 | 6 | 7 | 8),
        "LocalUser" => matches!(identity, 1 | 3 | 4 | 6 | 7 | 8),
        "RobloxScript" => matches!(identity, 3 | 6 | 7 | 8),
        "WritePlayer" | "RobloxEngine" | "NotAccessible" => matches!(identity, 7 | 8),
        _ => false,
    }
}

pub(super) fn current_context(lua: &Lua, thread: &Thread) -> Option<Table> {
    let scheduler = lua.app_data_ref::<SchedulerRef>()?.clone();
    let contexts = scheduler.borrow().contexts.clone()?;
    contexts.get(thread.clone()).ok().flatten()
}

pub(super) fn script_for_thread(lua: &Lua, thread: &Thread) -> Option<Value> {
    let context = current_context(lua, thread)?;
    let owner = context.get::<String>("owner").ok()?;
    if !owner.starts_with("script:") {
        return None;
    }
    context
        .get::<Value>("script")
        .ok()
        .filter(|value| !value.is_nil())
}

/// Cancel outstanding scheduled work owned by scripts in a destroyed subtree.
/// The currently executing thread is retained only when it is itself running
/// under one of the destroyed script owners; callers use the returned ids to
/// keep that self-destroying script alive through its current coroutine.
pub(super) fn destroy_script_owners(lua: &Lua, ids: &[String]) -> Result<std::collections::HashSet<String>> {
    let owners = ids
        .iter()
        .map(|id| format!("script:{id}"))
        .collect::<std::collections::HashSet<_>>();
    if owners.is_empty() {
        return Ok(Default::default());
    }
    let current = lua.current_thread();
    let current_key = thread_key(&current);
    let scheduler = lua
        .app_data_ref::<SchedulerRef>()
        .map(|scheduler| scheduler.clone());
    let Some(scheduler) = scheduler else {
        return Ok(Default::default());
    };
    let contexts = scheduler.borrow().contexts.clone();
    let Some(contexts) = contexts else {
        return Ok(Default::default());
    };
    let mut cancel = Vec::new();
    let mut preserved = std::collections::HashSet::new();
    for row in contexts.pairs::<Thread, Table>() {
        let (thread, context) = row?;
        let Ok(owner) = context.get::<String>("owner") else {
            continue;
        };
        if !owners.contains(&owner) {
            continue;
        }
        if thread_key(&thread) == current_key {
            preserved.insert(owner.trim_start_matches("script:").to_owned());
        } else if thread.is_resumable() {
            cancel.push(thread);
        }
    }
    // Do not hold scheduler/context borrows while the official VM closes the
    // child coroutine and its to-be-closed values.
    let cancel_fn: Function = lua.globals().get::<Table>("task")?.get("cancel")?;
    for thread in cancel {
        cancel_fn.call::<()>(thread)?;
    }
    Ok(preserved)
}

fn install_thread_context(
    lua: &Lua,
    scheduler: &SchedulerRef,
    thread: &Thread,
    identity: i64,
    owner: String,
    script: Option<Value>,
) -> Result<()> {
    let contexts = scheduler.borrow_mut().ensure_contexts(lua)?;
    let context = lua.create_table()?;
    context.set("identity", identity)?;
    context.set("owner", owner)?;
    if let Some(script) = script {
        context.set("script", script)?;
    }
    contexts.set(thread.clone(), context)?;
    Ok(())
}

/// Resume a suspended thread with the exact supplied tuple. No scheduler
/// borrow is held while Lua executes.
pub fn resume(lua: &Lua, thread: Thread, args: MultiValue) -> Result<()> {
    if !thread.is_resumable() {
        return Ok(());
    }
    let scheduler = lua
        .app_data_ref::<SchedulerRef>()
        .ok_or_else(|| mlua::Error::RuntimeError("scheduler is not installed".into()))?
        .clone();
    let key = thread_key(&thread);
    let policy = {
        let mut state = scheduler.borrow_mut();
        state.ready.retain(|item| thread_key(&item.thread) != key);
        state
            .sleeping
            .retain(|item| thread_key(&item.thread) != key);
        let task = state.tasks.get_mut(&key);
        if let Some(task) = task {
            task.parked = false;
            task.policy
        } else {
            state.active_policy.unwrap_or_default()
        }
    };
    let result = scheduler.borrow_mut().enqueue(thread, args, None, policy);
    result
}

/// Park a suspended coroutine until an external local event calls `resume`.
pub fn park(lua: &Lua, thread: &Thread) -> Result<()> {
    let scheduler = lua
        .app_data_ref::<SchedulerRef>()
        .ok_or_else(|| mlua::Error::RuntimeError("scheduler is not installed".into()))?
        .clone();
    let mut state = scheduler.borrow_mut();
    let key = thread_key(thread);
    state.ready.retain(|item| thread_key(&item.thread) != key);
    state
        .sleeping
        .retain(|item| thread_key(&item.thread) != key);
    state.tasks.entry(key).or_default().parked = true;
    state.set_diagnostic_state(key, TaskStateKind::Waiting);
    Ok(())
}

pub fn is_parked(scheduler: &SchedulerRef, thread: &Thread) -> bool {
    scheduler
        .borrow()
        .tasks
        .get(&thread_key(thread))
        .is_some_and(|task| task.parked)
}

/// External completions must not resurrect cancelled or manually resumed tasks.
pub fn is_waiting(lua: &Lua, thread: &Thread) -> bool {
    thread.is_resumable()
        && lua.app_data_ref::<SchedulerRef>().is_some_and(|scheduler| {
            scheduler
                .borrow()
                .tasks
                .get(&thread_key(thread))
                .is_some_and(|task| task.parked && !task.cancelled)
        })
}

pub fn take_yielded(scheduler: &SchedulerRef, thread: &Thread) -> Option<MultiValue> {
    match take_outcome(scheduler, thread)? {
        TaskOutcome::Yielded(values) => Some(values),
        other => {
            scheduler
                .borrow_mut()
                .record_outcome(thread_key(thread), other);
            None
        }
    }
}

pub fn take_outcome(scheduler: &SchedulerRef, thread: &Thread) -> Option<TaskOutcome> {
    let key = thread_key(thread);
    let mut state = scheduler.borrow_mut();
    let outcome = state.outcomes.remove(&key);
    if outcome.is_some() {
        state.outcome_order.retain(|item| *item != key);
    }
    outcome
}

pub fn diagnostics(scheduler: &SchedulerRef, thread: &Thread) -> Option<TaskDiagnostics> {
    scheduler
        .borrow()
        .diagnostics
        .get(&thread_key(thread))
        .cloned()
}

pub fn snapshot(scheduler: &SchedulerRef) -> SchedulerSnapshot {
    scheduler.borrow().snapshot()
}

pub fn drain_errors(scheduler: &SchedulerRef) -> Vec<String> {
    scheduler.borrow_mut().errors.drain(..).collect()
}

/// Install the standard task helpers in this local Luau state.
pub fn install(lua: &Lua, scheduler: &SchedulerRef) -> Result<()> {
    lua.set_app_data(scheduler.clone());
    {
        let mut state = scheduler.borrow_mut();
        state.ensure_contexts(lua)?;
        state.preempt_marker = Some(deadline_interrupt::marker(lua)?);
    }
    let task = lua.create_table()?;
    let marker = scheduler.borrow_mut().ensure_marker(lua)?;

    let spawn = lua.create_function(|lua, (function, args): (Function, MultiValue)| {
        enqueue(lua, function, args)
    })?;
    task.set("spawn", spawn.clone())?;
    task.set("defer", spawn)?;

    let delay = lua.create_function(
        |lua, (seconds, function, args): (f64, Function, MultiValue)| {
            if !seconds.is_finite() || !(0.0..=MAX_WAIT_SECONDS).contains(&seconds) {
                return Err(mlua::Error::RuntimeError(
                    "invalid task.delay duration".into(),
                ));
            }
            let thread = lua.create_thread(function)?;
            let scheduler = lua
                .app_data_ref::<SchedulerRef>()
                .ok_or_else(|| mlua::Error::RuntimeError("scheduler is not installed".into()))?
                .clone();
            let started = Instant::now();
            let policy = current_policy(lua);
            let parent = lua.current_thread();
            let parent_context = current_context(lua, &parent);
            let identity = parent_context
                .as_ref()
                .and_then(|ctx| ctx.get::<i64>("identity").ok())
                .unwrap_or(8);
            let owner = parent_context
                .as_ref()
                .and_then(|ctx| ctx.get::<String>("owner").ok())
                .unwrap_or_default();
            let script = parent_context
                .as_ref()
                .and_then(|ctx| ctx.get::<Value>("script").ok())
                .filter(|value| !value.is_nil());
            install_thread_context(
                lua,
                &scheduler,
                &thread,
                identity,
                owner.clone(),
                script.clone(),
            )?;
            scheduler.borrow_mut().enqueue(
                thread.clone(),
                args,
                Some(started + Duration::from_secs_f64(seconds)),
                policy,
            )?;
            if let Some(item) = scheduler.borrow_mut().sleeping.last_mut() {
                item.wake = Some(Wake::Delay);
            }
            if owner.starts_with("script:") && script.is_some() {
                super::script_threads::retain(lua, &thread)?;
            }
            Ok(thread)
        },
    )?;
    task.set("delay", delay)?;

    let wait_setup: Function = lua.load(
        "return function(task, marker) task.wait = function(seconds)\n            seconds = seconds or 0\n            if type(seconds) ~= 'number' or seconds ~= seconds or seconds < 0 or seconds > 86400 then error('invalid task.wait duration', 2) end\n            return coroutine.yield(marker, seconds)\n        end end",
    ).eval()?;
    wait_setup.call::<()>((task.clone(), marker))?;

    let close_thread: Function = lua.globals().get::<Table>("coroutine")?.get("close")?;
    let cancel = lua.create_function(move |lua, thread: Thread| {
        // Let the official VM validate and close the coroutine before changing
        // scheduler state. A suspended stack can otherwise be resumed manually
        // and keep its locals alive after cancellation. A running/normal thread
        // is rejected here with its scheduler state untouched.
        close_thread.call::<()>(thread.clone())?;
        if let Some(scheduler) = lua.app_data_ref::<SchedulerRef>() {
            let scheduler = scheduler.clone();
            let key = thread_key(&thread);
            let mut state = scheduler.borrow_mut();
            state.ready.retain(|item| thread_key(&item.thread) != key);
            state
                .sleeping
                .retain(|item| thread_key(&item.thread) != key);
            if state.tasks.contains_key(&key) {
                state.publish_outcome(key, TaskOutcome::Cancelled);
                state.tasks.remove(&key);
                state.set_diagnostic_state(key, TaskStateKind::Cancelled);
            }
        }
        super::script_threads::cleanup_finished(lua)?;
        Ok(())
    })?;
    task.set("cancel", cancel)?;
    lua.globals().set("task", task.clone())?;

    // Legacy Roblox globals have a different return contract from task APIs:
    // wait returns both the measured wait duration and an absolute elapsed
    // runtime value, while spawn/delay do not return the scheduled thread.
    // Keep the wrappers in Lua so wait can yield through task.wait.
    let started = Instant::now();
    let runtime_clock = lua.create_function(move |_, ()| Ok(started.elapsed().as_secs_f64()))?;
    let legacy_wait_factory: Function = lua
        .load(
            "return function(task_wait, clock) return function(seconds)\n\
                local elapsed = task_wait(seconds)\n\
                return elapsed, clock()\n\
            end end",
        )
        .eval()?;
    let legacy_wait: Function = legacy_wait_factory.call((task.get::<Function>("wait")?, runtime_clock))?;
    lua.globals().set("wait", legacy_wait)?;

    let legacy_spawn_delay_factory: Function = lua
        .load(
            "return function(task)\n\
                return function(callback, ...) task.defer(callback, ...) end,\n\
                    function(seconds, callback, ...) task.delay(seconds, callback, ...) end\n\
            end",
        )
        .eval()?;
    let (legacy_spawn, legacy_delay): (Function, Function) =
        legacy_spawn_delay_factory.call(task.clone())?;
    lua.globals().set("spawn", legacy_spawn)?;
    lua.globals().set("delay", legacy_delay)?;

    // Caller origin is independent of the editable identity number. Game
    // script tasks and their descendants retain their source-owned context.
    lua.globals().set(
        "checkcaller",
        lua.create_function(|lua, ()| {
            let owner = current_context(lua, &lua.current_thread())
                .and_then(|context| context.get::<String>("owner").ok())
                .unwrap_or_default();
            Ok(!owner.starts_with("script:"))
        })?,
    )?;

    for name in ["getthreadidentity", "getidentity", "getthreadcontext"] {
        lua.globals().set(
            name,
            lua.create_function(|lua, ()| Ok(thread_identity(lua)))?,
        )?;
    }
    for name in ["setthreadidentity", "setidentity", "setthreadcontext"] {
        lua.globals().set(
            name,
            lua.create_function(|lua, value: Value| {
                // The pinned Luau API's luaL_checkinteger converts through
                // lua_tointegerx (a C int cast). mlua's i64 argument decoder
                // instead rejects large numbers such as +infinity before
                // matching native identity semantics. Reuse the VM's own
                // conversion so fractional and out-of-range numbers follow
                // the same path as the pinned C++ binding.
                let mut was_number = 0;
                let identity: i32 = unsafe {
                    lua.exec_raw((value.clone(),), |state| {
                        let top = mlua::ffi::lua_gettop(state);
                        let converted = mlua::ffi::lua_tointegerx_(state, top, &mut was_number);
                        mlua::ffi::lua_settop(state, top - 1);
                        mlua::ffi::lua_pushnumber(state, converted as f64);
                    })?
                };
                if was_number == 0 {
                    return Err(mlua::Error::BadArgument {
                        to: Some("function 'setthreadidentity'".into()),
                        pos: 1,
                        name: None,
                        cause: std::sync::Arc::new(mlua::Error::FromLuaConversionError {
                            from: value.type_name(),
                            to: "integer".into(),
                            message: None,
                        }),
                    });
                }
                set_thread_identity(lua, identity as i64)
            })?,
        )?;
    }
    let inherit = lua.create_function(|lua, child: Thread| {
        let scheduler = lua
            .app_data_ref::<SchedulerRef>()
            .ok_or_else(|| mlua::Error::RuntimeError("scheduler is not installed".into()))?
            .clone();
        let parent = lua.current_thread();
        let parent_context = current_context(lua, &parent);
        let identity = parent_context
            .as_ref()
            .and_then(|ctx| ctx.get::<i64>("identity").ok())
            .unwrap_or(8);
        let owner = parent_context
            .as_ref()
            .and_then(|ctx| ctx.get::<String>("owner").ok())
            .unwrap_or_default();
        let script = parent_context
            .as_ref()
            .and_then(|ctx| ctx.get::<Value>("script").ok())
            .filter(|value| !value.is_nil());
        install_thread_context(lua, &scheduler, &child, identity, owner, script)
    })?;
    let coroutine = lua.globals().get::<Table>("coroutine")?;
    let current_preempt_marker = lua.create_function(|lua, ()| {
        Ok(lua
            .named_registry_value::<Table>(deadline_interrupt::MARKER_KEY)
            .ok())
    })?;
    let wrap_coroutines: Function = lua
        .load(
            r#"return function(co, native_create, native_resume, inherit, current_preempt_marker)
                co.create = function(fn) local thread = native_create(fn); inherit(thread); return thread end
                co.resume = function(thread, ...)
                    local args = table.pack(...)
                    while true do
                        local result = table.pack(native_resume(thread, table.unpack(args, 1, args.n)))
                        local marker = current_preempt_marker()
                        if result[1] and marker ~= nil and result[2] == marker then
                            coroutine.yield(marker)
                            args = table.pack()
                        else
                            return table.unpack(result, 1, result.n)
                        end
                    end
                end
                co.wrap = function(fn)
                    local thread = co.create(fn)
                    return function(...)
                        local result = table.pack(co.resume(thread, ...))
                        if not result[1] then error(result[2], 2) end
                        return table.unpack(result, 2, result.n)
                    end
                end
            end"#,
        )
        .eval()?;
    wrap_coroutines.call::<()>((
        coroutine.clone(),
        coroutine.get::<Function>("create")?,
        coroutine.get::<Function>("resume")?,
        inherit,
        current_preempt_marker,
    ))?;
    Ok(())
}

/// Execute at most one host budget of VM work. Per-task slices can preempt at
/// 5ms and resume on a later call when their policy permits it.
pub fn step(lua: &Lua, scheduler: &SchedulerRef, now: Instant, budget: Duration) -> StepReport {
    step_with_limits(lua, scheduler, now, budget)
}

/// Give one ready result-bearing job a reserved interactive slice. Selecting
/// from the FIFO queue keeps multiple submitted jobs in round-robin order.
/// Background callbacks retain the rest of the host budget.
pub(crate) fn step_interactive(lua: &Lua, scheduler: &SchedulerRef, budget: Duration) -> StepReport {
    let thread = {
        let mut state = scheduler.borrow_mut();
        let mut oldest: Option<(u64, Thread)> = None;
        let mut after_cursor: Option<(u64, Thread)> = None;
        for item in &state.ready {
            if !state.tasks.get(&thread_key(&item.thread)).is_some_and(|task| {
                task.retain_outcome && task.interactive_eligible && !task.cancelled
            }) {
                continue;
            }
            let candidate = (item.sequence, item.thread.clone());
            if oldest.as_ref().is_none_or(|(sequence, _)| item.sequence < *sequence) {
                oldest = Some(candidate.clone());
            }
            if state
                .last_interactive_sequence
                .is_none_or(|last| item.sequence > last)
                && after_cursor
                    .as_ref()
                    .is_none_or(|(sequence, _)| item.sequence < *sequence)
            {
                after_cursor = Some(candidate);
            }
        }
        let selected = after_cursor.or(oldest);
        if let Some((sequence, _)) = &selected {
            state.last_interactive_sequence = Some(*sequence);
        }
        selected.map(|(_, thread)| thread)
    };
    thread.map_or_else(StepReport::default, |thread| {
        step_specific_thread(lua, scheduler, Instant::now(), budget, &thread)
    })
}

/// Capture admitted callbacks while keeping the thread alive, so a recycled VM
/// pointer cannot make an unrelated task hold a later frame open.
pub(crate) fn frame_callback_batch(
    scheduler: &SchedulerRef,
    fire: impl FnOnce() -> Result<()>,
) -> Result<Vec<(Thread, u64)>> {
    let sequence = scheduler.borrow().next_sequence;
    fire()?;
    let state = scheduler.borrow();
    Ok(state.ready.iter().filter(|item| item.sequence >= sequence).map(|item| {
        let yields = state.diagnostics.get(&thread_key(&item.thread)).map_or(0, |d| d.yields);
        (item.thread.clone(), yields)
    }).collect())
}

pub(crate) fn frame_callback_unfinished(scheduler: &SchedulerRef, thread: &Thread, yields: u64) -> bool {
    let state = scheduler.borrow();
    let key = thread_key(thread);
    state.tasks.get(&key).is_some_and(|task| !task.cancelled)
        && state.diagnostics.get(&key).is_none_or(|d| d.yields <= yields)
}

/// Run newly fired frame callbacks without waking ordinary task.wait sleepers.
/// The task registry stays intact, including limits, ownership and CPU budgets.
pub(crate) fn step_phase(
    lua: &Lua,
    scheduler: &SchedulerRef,
    budget: Duration,
    fire: impl FnOnce() -> Result<()>,
) -> Result<StepReport> {
    let (mut ready, mut sleeping) = {
        let mut state = scheduler.borrow_mut();
        (
            std::mem::take(&mut state.ready),
            std::mem::take(&mut state.sleeping),
        )
    };
    let result = fire().map(|()| step(lua, scheduler, Instant::now(), budget));
    // Restore on enqueue errors too; unrelated jobs must never disappear.
    let mut state = scheduler.borrow_mut();
    ready.append(&mut state.ready);
    sleeping.append(&mut state.sleeping);
    state.ready = ready;
    state.sleeping = sleeping;
    result
}

pub fn step_with_limits(
    lua: &Lua,
    scheduler: &SchedulerRef,
    now: Instant,
    budget: Duration,
) -> StepReport {
    step_with_limits_for(lua, scheduler, now, budget, None)
}

fn step_specific_thread(
    lua: &Lua,
    scheduler: &SchedulerRef,
    now: Instant,
    budget: Duration,
    thread: &Thread,
) -> StepReport {
    step_with_limits_for(lua, scheduler, now, budget, Some(thread_key(thread)))
}

/// Reinsert a preempted continuation ahead of tasks queued after it. VM
/// preemption is invisible to the script; only an authored yield should give
/// later callbacks a turn before this continuation resumes.
fn requeue_preempted(state: &mut Scheduler, item: Pending) {
    let sequence = item.sequence;
    let index = state
        .ready
        .iter()
        .position(|queued| queued.sequence > sequence)
        .unwrap_or(state.ready.len());
    state.ready.insert(index, item);
}

fn step_with_limits_for(
    lua: &Lua,
    scheduler: &SchedulerRef,
    now: Instant,
    budget: Duration,
    only_thread: Option<usize>,
) -> StepReport {
    let budget = budget.min(NONYIELDING_LIMIT);
    let host_deadline = Instant::now() + budget;
    let mut report = StepReport::default();

    // A targeted resume (interactive job or immediate callback) deliberately
    // runs only one already-ready coroutine. Scanning and rebuilding the full
    // sleeper queue here cannot help that coroutine, and can dominate the
    // reserved slice when many unrelated tasks are waiting. The next ordinary
    // scheduler step performs the normal due-task wakeup in FIFO order.
    if only_thread.is_none() {
        let mut state = scheduler.borrow_mut();
        let mut ready_sleepers = Vec::new();
        let mut still_sleeping = Vec::with_capacity(state.sleeping.len());
        for item in state.sleeping.drain(..) {
            if item.due.is_some_and(|due| due <= now) {
                ready_sleepers.push(item);
            } else {
                still_sleeping.push(item);
            }
        }
        state.sleeping = still_sleeping;
        ready_sleepers.sort_by_key(|item| (item.due, item.sequence));
        for mut item in ready_sleepers {
            if let Some(wake) = item.wake.take() {
                match wake {
                    Wake::Wait { started } => {
                        let elapsed = now.saturating_duration_since(started).as_secs_f64();
                        item.args = MultiValue::from_vec(vec![Value::Number(elapsed.into())]);
                    }
                    Wake::Delay => {}
                }
            }
            state.set_diagnostic_state(thread_key(&item.thread), TaskStateKind::Ready);
            // A sleeper becomes ready now, after tasks already in the queue.
            item.sequence = state.next_sequence;
            state.next_sequence = state.next_sequence.wrapping_add(1);
            state.ready.push_back(item);
        }
    }

    while Instant::now() < host_deadline {
        let next = {
            let mut state = scheduler.borrow_mut();
            match only_thread {
                Some(wanted) => state
                    .ready
                    .iter()
                    .position(|item| thread_key(&item.thread) == wanted)
                    .and_then(|index| state.ready.remove(index)),
                None => state.ready.pop_front(),
            }
        };
        let Some(mut item) = next else {
            break;
        };
        let key = thread_key(&item.thread);
        let (policy, cpu_elapsed, uninterrupted, cancelled) = {
            let mut state = scheduler.borrow_mut();
            if only_thread.is_none() {
                if let Some(task) = state.tasks.get_mut(&key) {
                    if task.retain_outcome {
                        // Once an authored yield resumes in normal FIFO order,
                        // the job may use its reserved slice again if VM work
                        // preempts it before its next authored yield.
                        task.interactive_eligible = true;
                    }
                }
            }
            state
                .tasks
                .get(&key)
                .map(|task| {
                    (
                        task.policy,
                        task.cpu_elapsed,
                        task.uninterrupted,
                        task.cancelled,
                    )
                })
                .unwrap_or((TaskPolicy::default(), Duration::ZERO, Duration::ZERO, false))
        };
        if cancelled {
            let mut state = scheduler.borrow_mut();
            state.set_diagnostic_state(key, TaskStateKind::Cancelled);
            state.publish_outcome(key, TaskOutcome::Cancelled);
            state.tasks.remove(&key);
            report.completed += 1;
            continue;
        }
        let remaining = policy.total.saturating_sub(cpu_elapsed);
        if remaining.is_zero() {
            let mut state = scheduler.borrow_mut();
            let message = "local Luau task exceeded its execution budget".to_owned();
            state.set_diagnostic_state(key, TaskStateKind::Error);
            state.record_error(message.clone());
            state.publish_outcome(key, TaskOutcome::TimedOut(message.clone()));
            state.tasks.remove(&key);
            report.errors.push(message);
            report.completed += 1;
            continue;
        }

        let started = Instant::now();
        // A short host poll is a scheduling boundary, not a smaller authored
        // task budget. Strict tasks may finish their configured slice across
        // several polls, but cannot reset it by being preempted each frame.
        let slice_remaining = if policy.resume_across_frames {
            policy.slice.min(remaining)
        } else {
            policy.slice.saturating_sub(uninterrupted).min(remaining)
        };
        let task_deadline = started + slice_remaining;
        let resume_deadline = host_deadline.min(task_deadline);
        let hard_limit = if policy.resume_across_frames {
            remaining.min(policy.non_yielding_limit.saturating_sub(uninterrupted))
        } else {
            slice_remaining
        };
        let hard_deadline = started + hard_limit;
        let interrupt =
            match deadline_interrupt::DeadlineInterrupt::new(lua, resume_deadline, hard_deadline) {
                Ok(interrupt) => interrupt,
                Err(error) => {
                    let message = error.to_string();
                    let mut state = scheduler.borrow_mut();
                    state.set_diagnostic_state(key, TaskStateKind::Error);
                    state.record_error(message.clone());
                    state.publish_outcome(key, TaskOutcome::Error(message.clone()));
                    state.tasks.remove(&key);
                    report.errors.push(message);
                    report.completed += 1;
                    continue;
                }
            };

        let previous_policy = scheduler.borrow().active_policy;
        {
            let mut state = scheduler.borrow_mut();
            state.running.insert(key);
            state.active_policy = Some(policy);
            state.set_diagnostic_state(key, TaskStateKind::Running);
            let diagnostic = state.diagnostics.entry(key).or_default();
            diagnostic.resumes = diagnostic.resumes.saturating_add(1);
        }
        let result = item.thread.resume::<MultiValue>(item.args);
        let was_preempted = interrupt.preempted();
        let was_aborted = interrupt.aborted();
        drop(interrupt);
        {
            let mut state = scheduler.borrow_mut();
            state.running.remove(&key);
            state.active_policy = previous_policy;
        }
        let elapsed = started.elapsed();
        report.execution_time += elapsed;
        report.resumed += 1;

        let mut state = scheduler.borrow_mut();
        state.total_resumes = state.total_resumes.saturating_add(1);
        if state.tasks.get(&key).is_some_and(|task| task.cancelled) {
            state.set_diagnostic_state(key, TaskStateKind::Cancelled);
            state.diagnostics.entry(key).or_default().execution_ms +=
                elapsed.as_secs_f64() * 1000.0;
            state.publish_outcome(key, TaskOutcome::Cancelled);
            state.tasks.remove(&key);
            report.completed += 1;
            continue;
        }
        if was_aborted {
            // Even if the script caught the native callback's timeout, this
            // resume exhausted its budget and cannot be safely replayed.
            let message =
                "local Luau task exceeded its execution budget in a non-yieldable callback"
                    .to_owned();
            state.record_error(message.clone());
            let diagnostic = state.diagnostics.entry(key).or_default();
            diagnostic.execution_ms += elapsed.as_secs_f64() * 1000.0;
            diagnostic.slice_interruptions = diagnostic.slice_interruptions.saturating_add(1);
            state.total_preemptions = state.total_preemptions.saturating_add(1);
            state.set_diagnostic_state(key, TaskStateKind::Error);
            state.publish_outcome(key, TaskOutcome::TimedOut(message.clone()));
            state.tasks.remove(&key);
            report.errors.push(message);
            report.preempted += 1;
            report.completed += 1;
            continue;
        }
        let output = match result {
            Ok(values) => values,
            Err(error) => {
                let message = error.to_string();
                state.record_error(message.clone());
                state.diagnostics.entry(key).or_default().execution_ms +=
                    elapsed.as_secs_f64() * 1000.0;
                state.set_diagnostic_state(key, TaskStateKind::Error);
                state.publish_outcome(key, TaskOutcome::Error(message.clone()));
                report.errors.push(message);
                state.tasks.remove(&key);
                report.completed += 1;
                continue;
            }
        };
        if item.thread.status() == ThreadStatus::Finished {
            state.set_diagnostic_state(key, TaskStateKind::Completed);
            state.diagnostics.entry(key).or_default().execution_ms +=
                elapsed.as_secs_f64() * 1000.0;
            state.publish_outcome(key, TaskOutcome::Completed(output));
            state.tasks.remove(&key);
            report.completed += 1;
            continue;
        }

        let propagated_preemption = output.front().is_some_and(|value| {
            matches!(value, Value::Table(table) if state.preempt_marker.as_ref().is_some_and(|marker| marker.to_pointer() == table.to_pointer()))
        });
        if was_preempted || propagated_preemption {
            let (uninterrupted, cpu_elapsed) = {
                let task = state.tasks.entry(key).or_default();
                task.uninterrupted = task.uninterrupted.saturating_add(elapsed);
                task.cpu_elapsed = task.cpu_elapsed.saturating_add(elapsed);
                (task.uninterrupted, task.cpu_elapsed)
            };
            {
                let diagnostic = state.diagnostics.entry(key).or_default();
                diagnostic.execution_ms += elapsed.as_secs_f64() * 1000.0;
                diagnostic.slice_interruptions = diagnostic.slice_interruptions.saturating_add(1);
            }
            state.total_preemptions = state.total_preemptions.saturating_add(1);
            report.preempted += 1;
            if uninterrupted >= policy.non_yielding_limit
                || cpu_elapsed >= policy.total
                || (!policy.resume_across_frames && uninterrupted >= policy.slice)
            {
                let message = if uninterrupted >= policy.non_yielding_limit {
                    "local Luau task exceeded its non-yielding limit"
                } else {
                    "local Luau task exceeded its execution budget"
                }
                .to_owned();
                state.record_error(message.clone());
                state.set_diagnostic_state(key, TaskStateKind::Error);
                state.publish_outcome(key, TaskOutcome::TimedOut(message.clone()));
                report.errors.push(message);
                state.tasks.remove(&key);
                report.completed += 1;
                continue;
            }
            state.set_diagnostic_state(key, TaskStateKind::Ready);
            item.args = MultiValue::new();
            item.due = None;
            requeue_preempted(&mut state, item);
            continue;
        }

        let parked = {
            let task = state.tasks.entry(key).or_default();
            task.uninterrupted = Duration::ZERO;
            task.cpu_elapsed = task.cpu_elapsed.saturating_add(elapsed);
            task.parked
        };
        state.diagnostics.entry(key).or_default().execution_ms += elapsed.as_secs_f64() * 1000.0;
        if parked {
            state.tasks.entry(key).or_default().interactive_eligible = false;
            state.tasks.entry(key).or_default().cpu_elapsed = Duration::ZERO;
            let diagnostic = state.diagnostics.entry(key).or_default();
            diagnostic.yields = diagnostic.yields.saturating_add(1);
            state.set_diagnostic_state(key, TaskStateKind::Waiting);
            continue;
        }

        let values: Vec<Value> = output.into_iter().collect();
        let marker = state.marker.as_ref().map(Table::to_pointer);
        let wait_seconds = match (values.first(), marker) {
            (Some(Value::Table(value)), Some(marker)) if value.to_pointer() == marker => {
                values.get(1).and_then(lua_number)
            }
            _ => None,
        };
        if let Some(seconds) = wait_seconds {
            if seconds.is_finite() && (0.0..=MAX_WAIT_SECONDS).contains(&seconds) {
                let started = Instant::now();
                item.args = MultiValue::new();
                item.due = Some(started + Duration::from_secs_f64(seconds));
                item.wake = Some(Wake::Wait { started });
                item.sequence = state.next_sequence;
                state.next_sequence = state.next_sequence.wrapping_add(1);
                let diagnostic = state.diagnostics.entry(key).or_default();
                diagnostic.yields = diagnostic.yields.saturating_add(1);
                let task = state.tasks.entry(key).or_default();
                task.cpu_elapsed = Duration::ZERO;
                task.interactive_eligible = false;
                state.set_diagnostic_state(key, TaskStateKind::Sleeping);
                state.sleeping.push(item);
            } else {
                let message = "invalid task.wait duration".to_owned();
                state.record_error(message.clone());
                state.set_diagnostic_state(key, TaskStateKind::Error);
                state.publish_outcome(key, TaskOutcome::Error(message.clone()));
                report.errors.push(message);
                state.tasks.remove(&key);
                report.completed += 1;
            }
        } else if values.first().is_some_and(
            |value| matches!(value, Value::Table(table) if Some(table.to_pointer()) == marker),
        ) {
            let message = "invalid task.wait duration".to_owned();
            state.record_error(message.clone());
            state.set_diagnostic_state(key, TaskStateKind::Error);
            state.publish_outcome(key, TaskOutcome::Error(message.clone()));
            report.errors.push(message);
            state.tasks.remove(&key);
            report.completed += 1;
        } else {
            let yielded_values = MultiValue::from_vec(values);
            let task = state.tasks.entry(key).or_default();
            task.parked = true;
            task.cpu_elapsed = Duration::ZERO;
            task.interactive_eligible = false;
            state.publish_outcome(key, TaskOutcome::Yielded(yielded_values));
            let diagnostic = state.diagnostics.entry(key).or_default();
            diagnostic.yields = diagnostic.yields.saturating_add(1);
            state.set_diagnostic_state(key, TaskStateKind::Waiting);
        }
        if report.resumed >= MAX_PENDING {
            break;
        }
    }

    super::script_threads::cleanup_finished(lua).unwrap_or_else(|error| {
        scheduler.borrow_mut().record_error(error.to_string());
    });
    let state = scheduler.borrow();
    report.ready = state.ready.len();
    report.sleeping = state.sleeping.len();
    report.waiting = state
        .tasks
        .values()
        .filter(|task| task.parked && !task.cancelled)
        .count();
    report
}

#[cfg(test)]
mod queue_diagnostic_tests {
    use super::*;

    fn callback_thread(lua: &Lua) -> Thread {
        let function = lua.load("return function() end").eval::<Function>().unwrap();
        lua.create_thread(function).unwrap()
    }

    #[test]
    fn interactive_slice_advances_result_jobs_without_consuming_background_queue() {
        let lua = Lua::new();
        let scheduler = Rc::new(RefCell::new(Scheduler::new()));
        install(&lua, &scheduler).unwrap();
        lua.load("background=0; for i=1,20 do task.spawn(function() background+=1 end) end").exec().unwrap();
        let first=enqueue_with_policy(&lua,lua.load("return 41").into_function().unwrap(),MultiValue::new(),TaskPolicy::default()).unwrap();
        let second=enqueue_with_policy(&lua,lua.load("return 42").into_function().unwrap(),MultiValue::new(),TaskPolicy::default()).unwrap();
        let report=step_interactive(&lua,&scheduler,Duration::from_millis(20));
        assert!(report.errors.is_empty());
        assert!(matches!(take_outcome(&scheduler,&first),Some(TaskOutcome::Completed(_))));
        assert!(take_outcome(&scheduler,&second).is_none());
        lua.load("assert(background==0)").exec().unwrap();
        let report=step_interactive(&lua,&scheduler,Duration::from_millis(20));
        assert!(report.errors.is_empty());
        assert!(matches!(take_outcome(&scheduler,&second),Some(TaskOutcome::Completed(_))));
        let report=step(&lua,&scheduler,Instant::now(),Duration::from_millis(20));
        assert!(report.errors.is_empty());
        lua.load("assert(background==20)").exec().unwrap();
    }

    #[test]
    fn interactive_cpu_jobs_rotate_across_repeated_preemptions() {
        let lua = Lua::new();
        let scheduler = Rc::new(RefCell::new(Scheduler::new()));
        install(&lua, &scheduler).unwrap();
        lua.globals()
            .set(
                "slow_tick",
                lua.create_function(|_, ()| {
                    std::thread::sleep(Duration::from_millis(1));
                    Ok(())
                })
                .unwrap(),
            )
            .unwrap();
        let first = enqueue_with_policy(
            &lua,
            lua.load("for _=1,40 do slow_tick() end; return 'first'")
                .into_function()
                .unwrap(),
            MultiValue::new(),
            TaskPolicy::default(),
        )
        .unwrap();
        let second = enqueue_with_policy(
            &lua,
            lua.load("for _=1,40 do slow_tick() end; return 'second'")
                .into_function()
                .unwrap(),
            MultiValue::new(),
            TaskPolicy::default(),
        )
        .unwrap();

        let first_slice = step_interactive(&lua, &scheduler, Duration::from_millis(5));
        assert!(first_slice.errors.is_empty(), "{:?}", first_slice.errors);
        assert!(first_slice.preempted > 0);
        let first_key = thread_key(&first);
        let second_key = thread_key(&second);
        assert_eq!(scheduler.borrow().diagnostics[&first_key].resumes, 1);
        assert_eq!(scheduler.borrow().diagnostics[&second_key].resumes, 0);

        let second_slice = step_interactive(&lua, &scheduler, Duration::from_millis(5));
        assert!(second_slice.errors.is_empty(), "{:?}", second_slice.errors);
        assert!(second_slice.preempted > 0);
        let state = scheduler.borrow();
        assert_eq!(state.diagnostics[&first_key].resumes, 1);
        assert_eq!(state.diagnostics[&second_key].resumes, 1);
        assert!(!state.outcomes.contains_key(&first_key));
        assert!(!state.outcomes.contains_key(&second_key));
    }

    #[test]
    fn targeted_resume_defers_due_sleepers_to_the_next_ordinary_step_in_order() {
        let lua = Lua::new();
        let scheduler = Rc::new(RefCell::new(Scheduler::new()));
        install(&lua, &scheduler).unwrap();
        lua.load("order={}").exec().unwrap();

        for label in ["first", "second"] {
            let source = format!(
                "task.wait(60); table.insert(order, '{label}')"
            );
            enqueue_with_policy(
                &lua,
                lua.load(source).into_function().unwrap(),
                MultiValue::new(),
                TaskPolicy::default(),
            )
            .unwrap();
        }

        let first_step = step(&lua, &scheduler, Instant::now(), Duration::from_millis(20));
        assert!(first_step.errors.is_empty(), "{:?}", first_step.errors);
        assert_eq!(first_step.sleeping, 2);

        // Make both sleepers due without waiting. They stay in their original
        // sequence so the ordinary pump must resume them in that order.
        let due = Instant::now() - Duration::from_millis(1);
        {
            let mut state = scheduler.borrow_mut();
            for item in &mut state.sleeping {
                item.due = Some(due);
            }
        }

        let interactive = enqueue_with_policy(
            &lua,
            lua.load("table.insert(order, 'interactive')").into_function().unwrap(),
            MultiValue::new(),
            TaskPolicy::default(),
        )
        .unwrap();
        let targeted = step_interactive(&lua, &scheduler, Duration::from_millis(20));
        assert!(targeted.errors.is_empty(), "{:?}", targeted.errors);
        assert!(matches!(
            take_outcome(&scheduler, &interactive),
            Some(TaskOutcome::Completed(_))
        ));
        assert_eq!(targeted.sleeping, 2, "targeted resume must leave sleepers queued");
        assert_eq!(lua.load("table.concat(order, ',')").eval::<String>().unwrap(), "interactive");

        let ordinary = step(
            &lua,
            &scheduler,
            Instant::now() + Duration::from_secs(1),
            Duration::from_millis(20),
        );
        assert!(ordinary.errors.is_empty(), "{:?}", ordinary.errors);
        assert_eq!(ordinary.sleeping, 0);
        assert_eq!(
            lua.load("table.concat(order, ',')").eval::<String>().unwrap(),
            "interactive,first,second"
        );
    }

    #[test]
    fn preempted_callback_continuation_keeps_its_fifo_position() {
        let lua = Lua::new();
        let scheduler = Rc::new(RefCell::new(Scheduler::new()));
        install(&lua, &scheduler).unwrap();
        lua.load("order={}").exec().unwrap();
        lua.globals()
            .set(
                "slow_tick",
                lua.create_function(|_, ()| {
                    std::thread::sleep(Duration::from_millis(1));
                    Ok(())
                })
                .unwrap(),
            )
            .unwrap();

        enqueue_with_policy(
            &lua,
            lua.load("for _=1,12 do slow_tick() end; table.insert(order,'first')")
                .into_function()
                .unwrap(),
            MultiValue::new(),
            TaskPolicy::new(Duration::from_millis(5), Duration::from_secs(2), true).unwrap(),
        )
        .unwrap();
        enqueue_with_policy(
            &lua,
            lua.load("table.insert(order,'second')").into_function().unwrap(),
            MultiValue::new(),
            TaskPolicy::default(),
        )
        .unwrap();

        let mut preemptions = 0;
        for _ in 0..40 {
            let report = step(&lua, &scheduler, Instant::now(), Duration::from_millis(5));
            assert!(report.errors.is_empty(), "{:?}", report.errors);
            preemptions += report.preempted;
            if lua
                .load("#order")
                .eval::<usize>()
                .unwrap()
                == 2
            {
                break;
            }
        }
        assert!(preemptions > 0, "the slow callback must cross a VM preemption");
        assert_eq!(
            lua.load("table.concat(order, ',')").eval::<String>().unwrap(),
            "first,second"
        );
    }

    #[test]
    fn interactive_pump_keeps_emitted_callback_ahead_of_job_resumed_after_wait() {
        let lua = Lua::new();
        let scheduler = Rc::new(RefCell::new(Scheduler::new()));
        install(&lua, &scheduler).unwrap();
        lua.load("events={}").exec().unwrap();
        lua.globals()
            .set(
                "slow_tick",
                lua.create_function(|_, ()| {
                    std::thread::sleep(Duration::from_millis(1));
                    Ok(())
                })
                .unwrap(),
            )
            .unwrap();
        let job = enqueue_with_policy(
            &lua,
            lua.load(
                "task.spawn(function() table.insert(events,'callback') end); task.wait(0); for _=1,40 do slow_tick() end; return table.concat(events,',')",
            )
            .into_function()
            .unwrap(),
            MultiValue::new(),
            TaskPolicy::default(),
        )
        .unwrap();

        let first = step_interactive(&lua, &scheduler, Duration::from_millis(20));
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        assert_eq!(first.sleeping, 1);
        assert_eq!(first.ready, 1, "the emitted callback remains queued");

        // Let an earlier ordinary phase wake the job without running it, as
        // happens when a frame's execution budget is exhausted.
        let wake = step(
            &lua,
            &scheduler,
            Instant::now() + Duration::from_millis(1),
            Duration::ZERO,
        );
        assert_eq!(wake.ready, 2);
        let reserved = step_interactive(&lua, &scheduler, Duration::from_millis(20));
        assert_eq!(reserved.resumed, 0, "a post-wait job must not jump its callback");
        assert_eq!(lua.load("#events").eval::<usize>().unwrap(), 0);

        let ordinary = step(&lua, &scheduler, Instant::now(), Duration::from_millis(20));
        assert!(ordinary.errors.is_empty(), "{:?}", ordinary.errors);
        assert!(ordinary.preempted > 0, "the resumed result job must hit a VM deadline");
        assert!(take_outcome(&scheduler, &job).is_none());
        assert!(scheduler.borrow().tasks[&thread_key(&job)].interactive_eligible);

        let mut completed = false;
        for _ in 0..20 {
            let reserved = step_interactive(&lua, &scheduler, Duration::from_millis(20));
            assert!(reserved.errors.is_empty(), "{:?}", reserved.errors);
            if matches!(take_outcome(&scheduler, &job), Some(TaskOutcome::Completed(_))) {
                completed = true;
                break;
            }
        }
        assert!(completed, "the job should finish through reserved preemption slices");
        assert_eq!(
            lua.load("table.concat(events, ',')").eval::<String>().unwrap(),
            "callback"
        );
    }

    #[test]
    fn queue_limit_diagnostic_reports_bounded_phase_and_owner_counts() {
        let lua = Lua::new();
        let mut scheduler = Scheduler::new();
        let ready = callback_thread(&lua);
        let sleeping = callback_thread(&lua);
        let parked = callback_thread(&lua);
        let running = callback_thread(&lua);
        let ready_key = thread_key(&ready);
        let sleeping_key = thread_key(&sleeping);
        let parked_key = thread_key(&parked);
        let running_key = thread_key(&running);
        for (key, owner, is_parked) in [
            (ready_key, "local", false),
            (sleeping_key, "script:place-2", false),
            (parked_key, "script:place-2", true),
            (running_key, "script:place-3", false),
        ] {
            scheduler.tasks.insert(key, TaskState { owner: owner.into(), parked: is_parked, ..TaskState::default() });
        }
        scheduler.ready.push_back(Pending {
            thread: ready,
            args: MultiValue::new(),
            due: None,
            wake: None,
            sequence: 0,
        });
        scheduler.sleeping.push(Pending {
            thread: sleeping,
            args: MultiValue::new(),
            due: Some(Instant::now() + Duration::from_secs(30)),
            wake: None,
            sequence: 1,
        });
        scheduler.running.insert(running_key);
        scheduler.diagnostics.insert(ready_key, TaskDiagnostics {
            state: Some(TaskStateKind::Ready),
            resumes: 3,
            yields: 1,
            slice_interruptions: 2,
            execution_ms: 42.75,
        });

        let diagnostic = scheduler.queue_limit_diagnostic();
        assert!(diagnostic.contains("active=4"));
        assert!(diagnostic.contains("ready=1,sleeping=1,parked=1,running=1,other=0"));
        assert!(diagnostic.contains("script:place-2:2"));
        assert!(diagnostic.contains("sample=["));
        assert!(diagnostic.contains(":r3:cpu43ms:p2:y1"));
        assert!(!diagnostic.contains("0x"));
        assert!(safe_owner_label("script:bad / name\n").starts_with("script:bad___name_"));
    }

    #[test]
    fn diagnostic_churn_evicts_completed_rows_before_active_jobs() {
        let lua = Lua::new();
        let scheduler = Rc::new(RefCell::new(Scheduler::new()));
        install(&lua, &scheduler).unwrap();
        let long_running = enqueue_with_policy(
            &lua,
            lua.load("task.wait(60)").into_function().unwrap(),
            MultiValue::new(),
            TaskPolicy::default(),
        )
        .unwrap();
        let started = step(&lua, &scheduler, Instant::now(), Duration::from_millis(20));
        assert!(started.errors.is_empty(), "{:?}", started.errors);
        assert_eq!(started.sleeping, 1);
        let long_key = thread_key(&long_running);

        let callback = lua.load("return 1").into_function().unwrap();
        let mut retained_threads = Vec::new();
        for batch_size in [MAX_PENDING / 2, MAX_PENDING / 2, 64] {
            for _ in 0..batch_size {
                retained_threads.push(
                    enqueue(&lua, callback.clone(), MultiValue::new()).unwrap(),
                );
            }
            for _ in 0..16 {
                if scheduler.borrow().ready.is_empty() {
                    break;
                }
                let report = step(&lua, &scheduler, Instant::now(), Duration::from_millis(250));
                assert!(report.errors.is_empty(), "{:?}", report.errors);
            }
            assert!(scheduler.borrow().ready.is_empty(), "queued callbacks should drain");
            let state = scheduler.borrow();
            assert_eq!(state.tasks.len(), 1, "only the waiting long job stays active");
            assert_eq!(state.diagnostics[&long_key].resumes, 1);
        }

        let state = scheduler.borrow();
        assert_eq!(state.diagnostic_order.len(), MAX_PENDING);
        assert_eq!(state.diagnostics.len(), MAX_PENDING);
        assert!(state.diagnostics.contains_key(&long_key));
        assert_eq!(state.diagnostics[&long_key].yields, 1);
        assert!(retained_threads.len() > MAX_PENDING, "keep thread pointers unique during churn");
    }

    #[test]
    fn reusing_a_completed_thread_key_discards_stale_diagnostics_and_outcomes() {
        let lua = Lua::new();
        let thread = callback_thread(&lua);
        let key = thread_key(&thread);
        let mut scheduler = Scheduler::new();
        scheduler.diagnostics.insert(key, TaskDiagnostics {
            state: Some(TaskStateKind::Completed),
            resumes: 9,
            yields: 3,
            slice_interruptions: 5,
            execution_ms: 41.0,
        });
        scheduler.diagnostic_order.push_back(key);
        scheduler.outcomes.insert(key, TaskOutcome::Cancelled);
        scheduler.outcome_order.push_back(key);

        scheduler.clear_stale_task_records(key);

        assert!(!scheduler.diagnostics.contains_key(&key));
        assert!(!scheduler.diagnostic_order.contains(&key));
        assert!(!scheduler.outcomes.contains_key(&key));
        assert!(!scheduler.outcome_order.contains(&key));
    }
}

#[cfg(test)]
mod repeated_preemption_progress_tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc, sync::{Arc, atomic::{AtomicUsize, Ordering}}, thread, time::Duration};

    fn preempt_until_complete(lua: &Lua, scheduler: &SchedulerRef, source: &str) -> (usize, MultiValue) {
        let function = lua.load(source).into_function().unwrap();
        let thread = enqueue_with_policy(
            lua, function, MultiValue::new(),
            TaskPolicy::new(Duration::from_millis(5), Duration::from_secs(2), true).unwrap(),
        ).unwrap();
        let mut preemptions = 0;
        for _ in 0..100 {
            let report = step(lua, scheduler, Instant::now(), Duration::from_millis(5));
            assert!(report.errors.is_empty(), "{:?}", report.errors);
            preemptions += report.preempted;
            if let Some(TaskOutcome::Completed(values)) = take_outcome(scheduler, &thread) {
                return (preemptions, values);
            }
            assert!(thread.is_resumable(), "task stopped without a retained outcome");
        }
        panic!("deterministic callback loop did not finish after repeated scheduler slices")
    }

    fn setup() -> (Lua, SchedulerRef, Arc<AtomicUsize>) {
        let lua = Lua::new();
        let scheduler = Rc::new(RefCell::new(Scheduler::new()));
        install(&lua, &scheduler).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        lua.globals().set("native_tick", lua.create_function(move |_, ()| {
            thread::sleep(Duration::from_millis(1));
            observed.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }).unwrap()).unwrap();
        (lua, scheduler, calls)
    }

    #[test]
    fn repeated_deadlines_resume_the_same_loop_and_preserve_native_side_effects() {
        let (lua, scheduler, calls) = setup();
        let (preemptions, values) = preempt_until_complete(&lua, &scheduler, r#"
            local sum=0
            for i=1,16 do native_tick(); sum+=i end
            return sum
        "#);
        assert!(preemptions >= 2, "loop only experienced {preemptions} preemptions");
        assert_eq!(calls.load(Ordering::Relaxed), 16);
        assert_eq!(values.front().and_then(Value::as_integer), Some(136));
    }

    #[test]
    fn repeated_deadlines_propagate_through_coroutine_wrap_without_replaying_callbacks() {
        let (lua, scheduler, calls) = setup();
        let (preemptions, values) = preempt_until_complete(&lua, &scheduler, r#"
            local wrapped=coroutine.wrap(function()
                local sum=0
                for i=1,16 do native_tick(); sum+=i end
                return sum
            end)
            return wrapped()
        "#);
        assert!(preemptions >= 2, "wrapped loop only experienced {preemptions} preemptions");
        assert_eq!(calls.load(Ordering::Relaxed), 16);
        assert_eq!(values.front().and_then(Value::as_integer), Some(136));
    }
}
