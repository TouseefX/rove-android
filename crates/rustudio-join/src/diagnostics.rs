//! What a real join reports, for a phone that cannot hand back a log.
//!
//! The point of this module is that a failed join on a device says **where** it
//! failed. Every stage keeps its own outcome, so "signed in, allocated, refused
//! the certificate" is distinguishable from "signed in, never got an
//! allocation" without a debugger and without a network capture.
//!
//! Nothing here is secret: the account's user id appears, its name and cookie
//! do not; the endpoint appears, the token and tickets do not. [`Report::lines`]
//! is what an overlay draws.

use std::fmt;
use std::time::{Duration, Instant};

/// One stage of a join, and what happened in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub name: &'static str,
    pub outcome: Outcome,
    pub took: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok(String),
    Failed(String),
    /// Reached, and still waiting when the report was taken.
    Waiting,
}

impl Outcome {
    fn is_failed(&self) -> bool {
        matches!(self, Outcome::Failed(_))
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Ok(note) => write!(f, "ok ({note})"),
            Outcome::Failed(why) => write!(f, "FAILED: {why}"),
            Outcome::Waiting => f.write_str("waiting"),
        }
    }
}

/// A join's report. Begun when a join is attempted, one step per stage.
#[derive(Clone, Debug)]
pub struct Report {
    /// The place being joined, once it is known.
    pub place_id: u64,
    pub user_id: u64,
    steps: Vec<Step>,
    started: Instant,
    /// Where the last step ended; a step's `took` is the span from here.
    mark: Duration,
    /// When a named step began, for a step whose end is known much later
    /// (`settle`). `log in` is the one that needs it.
    started_at: std::collections::HashMap<String, Duration>,
    /// The furthest stage reached, which is the one line that matters.
    pub stage: &'static str,
}

impl Report {
    pub fn begin(place_id: u64) -> Self {
        Self {
            place_id,
            user_id: 0,
            steps: Vec::new(),
            started: Instant::now(),
            mark: Duration::ZERO,
            started_at: std::collections::HashMap::new(),
            stage: "start",
        }
    }

    /// Record a step that succeeded, with a note that is safe to show.
    pub fn ok(&mut self, name: &'static str, note: impl Into<String>) {
        self.push(
            Step {
                name,
                outcome: Outcome::Ok(note.into()),
                took: Duration::ZERO,
            },
            name,
        )
    }

    /// Record a step that failed. The report keeps going after a failure so the
    /// overlay can show what was tried; [`Report::failed_step`] then answers
    /// with it.
    pub fn failed(&mut self, name: &'static str, why: impl Into<String>) {
        self.push(
            Step {
                name,
                outcome: Outcome::Failed(why.into()),
                took: Duration::ZERO,
            },
            name,
        )
    }

    /// Replace a step already pushed (by name) with its outcome, keeping the
    /// moment it *started* as the beginning of its `took`.
    ///
    /// This is what a stage that spans the session needs: "log in" is known to
    /// have started when the five packets are written, and known to have
    /// succeeded only when the session ends. Pushing it once at the end
    /// charged the whole receive to it (a two-second login reported as 33 s);
    /// pushing it at the start and closing it here gives the two-second span
    /// and leaves the receive as its own line.
    pub fn settle(&mut self, name: &'static str, outcome: Step) {
        let now = self.started.elapsed();
        let began = self
            .started_at
            .get(name)
            .copied()
            .unwrap_or_else(|| now.saturating_sub(outcome.took));
        let mut step = outcome;
        step.took = now.saturating_sub(began);
        if let Some(existing) = self.steps.iter_mut().rev().find(|step| step.name == name) {
            *existing = step;
        } else {
            // Never opened by `begins`: the span from the previous mark, which
            // is where this step's work actually began.
            step.took = began;
            self.steps.push(step);
        }
        self.mark = now;
        self.stage = name;
    }

    /// Mark a step as begun, so `settle` can measure it from here.
    pub fn begins(&mut self, name: &'static str) {
        self.started_at.insert(name.to_string(), self.started.elapsed());
    }

    /// A step reached but not finished: the overlay draws it as waiting.
    pub fn waiting(&mut self, name: &'static str) {
        self.push(
            Step {
                name,
                outcome: Outcome::Waiting,
                took: Duration::ZERO,
            },
            name,
        )
    }

    fn push(&mut self, mut step: Step, stage: &'static str) {
        step.took = self.started.elapsed().saturating_sub(self.mark);
        // The mark is where this step was pushed, not the sum of the steps
        // before it. The difference matters: `log in` is pushed when the
        // session *ends*, so measuring from the sum charged connect and a
        // thirty-second receive to a step that took two seconds, and a report
        // reading "log in 33157 ms" said nothing about where the time went.
        self.mark = self.started.elapsed();
        self.steps.push(step);
        self.stage = stage;
    }

    pub fn failed_any(&self) -> bool {
        self.failed_step().is_some()
    }

    /// The first step that failed, if any.
    pub fn failed_step(&self) -> Option<&Step> {
        self.steps.iter().find(|step| step.outcome.is_failed())
    }

    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// The report as an overlay draws it: one line per step, the stage it
    /// reached first, and the total time last.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "join {}: {}",
            self.place_id,
            if self.failed_any() {
                "failed"
            } else {
                "in progress"
            }
        )];
        for step in &self.steps {
            lines.push(format!(
                "  {:<22} {} ({} ms)",
                step.name,
                step.outcome,
                step.took.as_millis()
            ));
        }
        lines.push(format!(
            "  {:<22} {}",
            "stage",
            self.stage
        ));
        lines.push(format!(
            "  {:<22} {} ms",
            "elapsed",
            self.elapsed().as_millis()
        ));
        lines
    }

    /// The one sentence to show when the join failed, or `None` when it has
    /// not failed.
    pub fn verdict(&self) -> Option<String> {
        self.failed_step().map(|step| {
            match &step.outcome {
                Outcome::Failed(why) => format!("{} failed: {why}", step.name),
                _ => unreachable!("only a failed step is returned"),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_join_reads_as_its_stages_in_order() {
        let mut report = Report::begin(1818);
        report.ok("sign in", "user 424242");
        report.ok("allocate", "5.5.5.5:443, 2 pins, token yes");
        report.ok("connect", "webtransport");
        report.ok("log in", "5 packets");
        report.waiting("receive");
        let lines = report.lines();
        assert!(lines[0].starts_with("join 1818: in progress"), "{lines:?}");
        let text = lines.join("\n");
        for name in ["sign in", "allocate", "connect", "log in", "receive"] {
            assert!(text.contains(name), "{name} missing from\n{text}");
        }
        assert!(text.contains("stage"), "the furthest stage is named");
        assert_eq!(report.verdict(), None);
    }

    #[test]
    fn a_failed_join_names_the_step_and_stops_naming_later_ones() {
        let mut report = Report::begin(1818);
        report.ok("sign in", "user 424242");
        report.failed("allocate", "the account needs a challenge");
        assert_eq!(
            report.verdict().as_deref(),
            Some("allocate failed: the account needs a challenge")
        );
        assert_eq!(report.stage, "allocate", "the stage it stopped at");
        assert!(report.lines()[0].contains("failed"));
    }

    #[test]
    fn a_steps_time_is_its_own_span_not_everything_before_it() {
        // The bug this pins: `log in` is pushed after the session ends, so
        // measuring from the report's start charged the whole thirty-second
        // receive to it. Each step now measures from where the last one ended.
        let mut report = Report::begin(1818);
        report.ok("sign in", "user 424242");
        std::thread::sleep(Duration::from_millis(30));
        report.ok("allocate", "5.5.5.5:443");
        std::thread::sleep(Duration::from_millis(30));
        report.ok("connect", "webtransport");
        std::thread::sleep(Duration::from_millis(30));
        report.ok("log in", "5 packets");
        let steps = report.steps();
        assert_eq!(steps.len(), 4);
        // Every step took roughly its own sleep, not the growing total.
        for step in steps.iter().skip(1) {
            assert!(
                step.took >= Duration::from_millis(20) && step.took < Duration::from_millis(120),
                "{} took {:?}, which is the elapsed total rather than its own span",
                step.name,
                step.took
            );
        }
        let total: Duration = steps.iter().map(|step| step.took).sum();
        assert!(total >= Duration::from_millis(90), "the steps add up to the elapsed time");
    }

    #[test]
    fn a_report_never_holds_a_secret() {
        // The steps are written by callers, so this is a check on the shape:
        // nothing in a report is a cookie, a token or a ticket.
        let mut report = Report::begin(1818);
        report.ok("allocate", "5.5.5.5:443, 2 pins, token yes");
        let text = report.lines().join("\n");
        assert!(!text.contains("ROBLOSECURITY"));
        assert!(text.contains("token yes"), "the presence is reported, not the value");
    }
}
