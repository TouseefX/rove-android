//! Explicit host-side limits for long authored workloads. Defaults stay unchanged.
use super::scheduler::TaskPolicy;
use std::time::Duration;

fn milliseconds(raw: &str, maximum: u64) -> Result<Duration, String> {
    let value = raw
        .parse::<u64>()
        .map_err(|_| "script CPU limit must be an integer".to_owned())?;
    if !(1..=maximum).contains(&value) {
        return Err(format!("script CPU limit must be 1..{maximum} ms"));
    }
    Ok(Duration::from_millis(value))
}

fn configure(
    mut policy: TaskPolicy,
    total: Option<&str>,
    uninterrupted: Option<&str>,
) -> Result<TaskPolicy, String> {
    if let Some(raw) = total {
        policy.total = milliseconds(raw, 300_000)?;
        policy.slice = Duration::from_millis(5).min(policy.total);
        policy.resume_across_frames = true;
    }
    if let Some(raw) = uninterrupted {
        if total.is_none() {
            return Err("ROVE_SCRIPT_NONYIELDING_MS requires ROVE_SCRIPT_CPU_MS".into());
        }
        policy.non_yielding_limit = milliseconds(raw, 30_000)?.min(policy.total);
    }
    Ok(policy)
}

pub(super) fn from_environment(policy: TaskPolicy) -> Result<TaskPolicy, String> {
    fn setting(name: &str) -> Result<Option<String>, String> {
        match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(_) => Err(format!("{name} must be UTF-8")),
        }
    }
    configure(
        policy,
        setting("ROVE_SCRIPT_CPU_MS")?.as_deref(),
        setting("ROVE_SCRIPT_NONYIELDING_MS")?.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordinary_limits_are_unchanged_and_explicit_limits_remain_bounded() {
        let ordinary = TaskPolicy::default();
        assert_eq!(configure(ordinary, None, None).unwrap(), ordinary);
        let extended = configure(ordinary, Some("30000"), Some("10000")).unwrap();
        assert_eq!(extended.slice, Duration::from_millis(5));
        assert_eq!(extended.total, Duration::from_secs(30));
        assert_eq!(extended.non_yielding_limit, Duration::from_secs(10));
        assert!(extended.resume_across_frames);
        for invalid in ["0", "300001", "-1", "abc", ""] {
            assert!(configure(ordinary, Some(invalid), None).is_err());
        }
        assert!(configure(ordinary, None, Some("10000")).is_err());
        assert!(configure(ordinary, Some("30000"), Some("30001")).is_err());
        assert_eq!(
            configure(ordinary, Some("50"), Some("100"))
                .unwrap()
                .non_yielding_limit,
            Duration::from_millis(50)
        );
    }

    #[test]
    fn configured_callback_limit_controls_real_luau_and_keeps_owner_usable() {
        use super::super::scheduler::{self, Scheduler, TaskOutcome};
        use mlua::{Lua, MultiValue};
        use std::{cell::RefCell, rc::Rc, time::Instant};
        for (limit, should_finish) in [("2", false), ("500", true)] {
            let lua = Lua::new();
            let scheduler = Rc::new(RefCell::new(Scheduler::new()));
            scheduler::install(&lua, &scheduler).unwrap();
            lua.globals()
                .set(
                    "pause_native",
                    lua.create_function(|_, ()| {
                        std::thread::sleep(Duration::from_millis(20));
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();
            let function = lua.load("table.sort({2,1}, function(a,b) pause_native(); local sum=0; for i=1,1000 do sum+=i end; return a<b end); return 42").into_function().unwrap();
            let policy = configure(TaskPolicy::default(), Some("1000"), Some(limit)).unwrap();
            let thread =
                scheduler::enqueue_with_policy(&lua, function, MultiValue::new(), policy).unwrap();
            let mut outcome = None;
            for _ in 0..100 {
                scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(5));
                outcome = scheduler::take_outcome(&scheduler, &thread);
                if outcome.is_some() {
                    break;
                }
            }
            assert_eq!(
                matches!(outcome, Some(TaskOutcome::Completed(_))),
                should_finish,
                "{outcome:?}"
            );
            if !should_finish {
                assert!(
                    matches!(outcome, Some(TaskOutcome::TimedOut(_))),
                    "{outcome:?}"
                );
            }
            assert_eq!(lua.load("return 6*7").eval::<i64>().unwrap(), 42);
        }
    }
}
