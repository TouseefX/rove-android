use super::*;
use crate::rust_host::scheduler::{self, Scheduler, SchedulerRef, TaskOutcome, TaskPolicy};
use mlua::{Function, Lua, MultiValue, Value};
use std::{
    cell::RefCell,
    rc::Rc,
    thread,
    time::{Duration, Instant},
};

#[test]
fn legacy_wait_in_engine_environment_yields_and_returns_elapsed_and_runtime_time() {
    let lua = Lua::new();
    let scheduler: SchedulerRef = Rc::new(RefCell::new(Scheduler::new()));
    scheduler::install(&lua, &scheduler).expect("scheduler install");

    let environment = engine_environment(&lua).expect("engine globals");
    assert!(environment.get::<Function>("wait").is_ok());
    assert!(environment.get::<Function>("spawn").is_ok());
    assert!(environment.get::<Function>("delay").is_ok());

    let function: Function = lua
        .load(
            r#"
            return function()
                local spawned, delayed
                local spawn_returns = table.pack(spawn(function(value) spawned = value end, 17))
                local delay_returns = table.pack(delay(0.01, function(value) delayed = value end, 23))
                local elapsed1, runtime1 = wait(0.01)
                local elapsed2, runtime2 = wait(0.01)
                return elapsed1, runtime1, elapsed2, runtime2, spawned, delayed,
                    spawn_returns.n == 0, delay_returns.n == 0
            end
        "#,
        )
        .eval()
        .expect("legacy wait fixture");
    function
        .set_environment(environment)
        .expect("set engine environment");
    let thread = scheduler::enqueue_with_policy(
        &lua,
        function,
        MultiValue::new(),
        TaskPolicy::default(),
    )
    .expect("enqueue fixture");

    let mut completed = None;
    for _ in 0..8 {
        let report = scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(50));
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        if let Some(outcome) = scheduler::take_outcome(&scheduler, &thread) {
            completed = Some(outcome);
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    let TaskOutcome::Completed(values) = completed.expect("legacy wait thread completes") else {
        panic!("legacy wait thread did not complete successfully");
    };
    let values = values.into_iter().collect::<Vec<_>>();
    assert_eq!(values.len(), 8, "legacy return tuple: {values:?}");
    let number = |value: &Value| {
        value
            .as_f64()
            .or_else(|| value.as_integer().map(|v| v as f64))
    };
    let (Some(elapsed1), Some(runtime1), Some(elapsed2), Some(runtime2)) = (
        number(&values[0]),
        number(&values[1]),
        number(&values[2]),
        number(&values[3]),
    ) else {
        panic!("legacy wait must return two numeric values per call: {values:?}");
    };
    assert!(elapsed1 >= 0.01 && elapsed2 >= 0.01, "{values:?}");
    assert!(runtime1 > 0.0 && runtime2 > runtime1, "{values:?}");
    assert_eq!(values[4].as_integer(), Some(17));
    assert_eq!(values[5].as_integer(), Some(23));
    assert_eq!(values[6].as_boolean(), Some(true));
    assert_eq!(values[7].as_boolean(), Some(true));
}
