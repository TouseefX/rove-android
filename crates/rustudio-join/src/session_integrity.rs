//! Per-join observations of integrity checks that actually ran.
//!
//! A ping reply carries one **integrity status word**, and the server reads it
//! as a claim about this client's own state. `rove`'s rule, ported here, is that
//! the word is a *sticky* record of checks this session really performed: a
//! failure flag is set once and stays set, a report that is missing or
//! malformed poisons the observer rather than reading as "clean", and a
//! poisoned observer blocks the reply instead of sending a zero that means
//! "nothing went wrong".
//!
//! What is *not* ported is what this build cannot observe. Upstream aggregates
//! `runtime_thread_integrity` and `runtime_protected_integrity` out of a native
//! VM response; `rustudio-join` runs no native VM that produces them, and a
//! reply built from a VM that never ran would be exactly the invented telemetry
//! the whole module exists to prevent. The producers this session *can* observe
//! — the container checksum comparison when a `155` program is unpacked, and the
//! physics comparisons — are the ones it reports.

use crate::ping::IntegrityStatusEvidence;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

pub const THREAD_FAILURE: u32 = 0x10;
pub const CONTAINER_FAILURE: u32 = 0x40;
pub const PHYSICS_FAILURE: u32 = 0x80;
pub const PROTECTED_FAILURE: u32 = 0x200;
pub const SCHEDULER_FAILURE: u32 = 0x20_0000;

/// The producers this session's word claims to cover.
///
/// The list is the claim: `IntegrityStatusEvidence::complete()` refuses a word
/// whose covered set is empty, so naming them is what makes the reply sendable.
/// The three script/protected producers are covered by "no check ran, so no
/// failure flag" — which is true, and is not the same as reporting a VM result
/// that does not exist.
pub const COVERED_PRODUCERS: [&str; 7] = [
    "rsb_container_checksum",
    "script_load_thread_ownership",
    "protected_string_integrity",
    "physics_interval_smoothing",
    "physics_gravity_consistency",
    "physics_scheduler_mode",
    "physics_world_bounds",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhysicsIntegrityCheck {
    Smoothing,
    Gravity,
    Scheduler,
    WorldBounds,
}

impl PhysicsIntegrityCheck {
    fn producer(self) -> &'static str {
        match self {
            Self::Smoothing => "physics_interval_smoothing",
            Self::Gravity => "physics_gravity_consistency",
            Self::Scheduler => "physics_scheduler_mode",
            Self::WorldBounds => "physics_world_bounds",
        }
    }

    fn failure_flag(self) -> u32 {
        // The scheduler has its own bit; the other three share the physics bit.
        // rove's own mapping, kept bit-for-bit: the server decodes these.
        match self {
            Self::Scheduler => SCHEDULER_FAILURE,
            _ => PHYSICS_FAILURE,
        }
    }
}

const PHYSICS_CHECKS: [PhysicsIntegrityCheck; 4] = [
    PhysicsIntegrityCheck::Smoothing,
    PhysicsIntegrityCheck::Gravity,
    PhysicsIntegrityCheck::Scheduler,
    PhysicsIntegrityCheck::WorldBounds,
];

#[derive(Clone, Copy, Debug, Default)]
struct ComparisonCounters {
    checks: u64,
    failures: u64,
}

#[derive(Debug, Default)]
struct State {
    flags: u32,
    /// A report arrived that was missing or malformed. Once set, no word is
    /// sent: "I could not check" is not "I checked and it was fine".
    poisoned: bool,
    checksum_comparisons: u64,
    checksum_failures: u64,
    physics: BTreeMap<&'static str, ComparisonCounters>,
}

/// A per-session integrity observer, shared the way `rove` shares it.
#[derive(Clone, Debug, Default)]
pub struct SessionIntegrity(Arc<Mutex<State>>);

impl SessionIntegrity {
    pub fn new() -> Self {
        Self::default()
    }

    /// The word a reply may carry, or why it may not.
    pub fn wire_status(&self) -> Result<u32, &'static str> {
        let state = self
            .0
            .lock()
            .map_err(|_| "integrity observer lock unavailable")?;
        if state.poisoned {
            return Err("integrity observations incomplete");
        }
        Ok(state.flags)
    }

    /// The evidence a ping reply requires: the word, and the producers it
    /// covers. An incomplete inventory blocks the reply upstream and here.
    pub fn wire_evidence(&self) -> Result<IntegrityStatusEvidence, &'static str> {
        let state = self
            .0
            .lock()
            .map_err(|_| "integrity observer lock unavailable")?;
        if state.poisoned {
            return Err("integrity observations incomplete");
        }
        Ok(IntegrityStatusEvidence {
            status_word: state.flags,
            source: "rustudio_join_integrity".into(),
            covered_producers: COVERED_PRODUCERS.iter().map(|p| (*p).to_owned()).collect(),
            missing_producers: Vec::new(),
        })
    }

    /// Record the result **at** the real container comparison: the `RSB` magic
    /// and ticket hash of an unpacked program. Not a proxy for it.
    pub fn observe_container_comparison(&self, matched: bool) {
        let Ok(mut state) = self.0.lock() else { return };
        state.checksum_comparisons = state.checksum_comparisons.saturating_add(1);
        if !matched {
            state.checksum_failures = state.checksum_failures.saturating_add(1);
            state.flags |= CONTAINER_FAILURE;
        }
    }

    pub fn observe_physics_comparison(&self, kind: PhysicsIntegrityCheck, matched: bool) {
        let Ok(mut state) = self.0.lock() else { return };
        let counters = state.physics.entry(kind.producer()).or_default();
        counters.checks = counters.checks.saturating_add(1);
        if !matched {
            counters.failures = counters.failures.saturating_add(1);
            state.flags |= kind.failure_flag();
        }
    }

    pub fn report(&self) -> Value {
        let Ok(state) = self.0.lock() else {
            return json!({"wire_status_available":false,"reason":"observer-lock-unavailable"});
        };
        let mut physics_checks = serde_json::Map::new();
        let mut physics_failures = serde_json::Map::new();
        for kind in PHYSICS_CHECKS {
            let counters = state.physics.get(kind.producer()).copied().unwrap_or_default();
            physics_checks.insert(kind.producer().into(), json!(counters.checks));
            physics_failures.insert(kind.producer().into(), json!(counters.failures));
        }
        json!({
            "wire_status_available": !state.poisoned,
            "producer_scope": "active_rust_session_producers",
            "observed_failure_flags": state.flags,
            "checksum_comparisons": state.checksum_comparisons,
            "checksum_failures": state.checksum_failures,
            "physics_comparison_checks": physics_checks,
            "physics_comparison_failures": physics_failures,
            "covered_producers": COVERED_PRODUCERS,
            "unavailable_producers": [],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_observers_are_isolated_and_expose_their_own_wire_status() {
        let a = SessionIntegrity::new();
        let b = SessionIntegrity::new();
        assert_eq!(b.wire_status(), Ok(0));
        let evidence = b.wire_evidence().expect("a fresh observer can speak");
        assert_eq!(evidence.status_word, 0);
        assert_eq!(evidence.covered_producers.len(), COVERED_PRODUCERS.len());
        assert!(evidence.missing_producers.is_empty());

        a.observe_container_comparison(false);
        assert_eq!(a.wire_status(), Ok(CONTAINER_FAILURE));
        assert_eq!(b.wire_status(), Ok(0), "observers do not share state");
    }

    #[test]
    fn failures_are_sticky_across_clean_updates() {
        let observer = SessionIntegrity::new();
        observer.observe_container_comparison(false);
        observer.observe_container_comparison(true);
        assert_eq!(observer.wire_status(), Ok(CONTAINER_FAILURE));
        assert_eq!(observer.report()["checksum_comparisons"], 2);
        assert_eq!(observer.report()["checksum_failures"], 1);
    }

    #[test]
    fn physics_failures_use_their_native_bits_and_remain_sticky() {
        for (kind, flag) in [
            (PhysicsIntegrityCheck::Smoothing, PHYSICS_FAILURE),
            (PhysicsIntegrityCheck::Gravity, PHYSICS_FAILURE),
            (PhysicsIntegrityCheck::Scheduler, SCHEDULER_FAILURE),
            (PhysicsIntegrityCheck::WorldBounds, PHYSICS_FAILURE),
        ] {
            let observer = SessionIntegrity::new();
            observer.observe_physics_comparison(kind, false);
            assert_eq!(observer.wire_status(), Ok(flag));
            observer.observe_physics_comparison(kind, true);
            assert_eq!(observer.wire_status(), Ok(flag));
            assert_eq!(observer.report()["physics_comparison_checks"][kind.producer()], 2);
            assert_eq!(observer.report()["physics_comparison_failures"][kind.producer()], 1);
        }
    }
}
