//! Python's session-local ISR estimator, driven only by received steps and real
//! QUIC delivery observations. Optional native clock branches are not modeled.
use serde_json::{json, Value};
#[derive(Default)]
pub(crate) struct IsrClock {
    remote: Option<u32>,
    acknowledged: Option<u32>,
    pub offset: Option<i32>,
    remote_new: bool,
    ack_new: bool,
    stabilized: bool,
    updates: u64,
    acknowledgments: u64,
    losses: u64,
    observations: u64,
}
fn delta(a: u32, b: u32) -> i32 {
    a.wrapping_sub(b) as i32
}
fn checked(value: i32) -> Result<i32, &'static str> {
    if (-0x7fff_fffe..=0x7fff_fffe).contains(&value) {
        Ok(value)
    } else {
        Err("isr-clock-reserved-offset")
    }
}
impl IsrClock {
    pub fn observe_remote(&mut self, step: u32) {
        if self.remote.is_none_or(|old| delta(step, old) > 0) {
            self.remote = Some(step);
            self.remote_new = true;
            self.observations += 1;
        }
    }
    pub fn acknowledged(&mut self, step: u32) {
        self.acknowledgments += 1;
        if self.acknowledged.is_none_or(|old| delta(step, old) > 0) {
            self.acknowledged = Some(step);
            self.ack_new = true;
        }
    }
    pub fn lost(&mut self) {
        self.losses += 1;
    }
    pub fn update(&mut self, current: u32) -> Result<Option<i32>, &'static str> {
        if !self.remote_new || !self.ack_new {
            return Ok(self.offset);
        }
        let raw = checked(delta(self.remote.unwrap(), self.acknowledged.unwrap()))?;
        let alpha = 0.1f32;
        let mut value = self.offset.map_or(raw, |old| {
            ((1.0f32 - alpha) * (old as f32) + alpha * (raw as f32)) as i32
        });
        if !self.stabilized && self.updates >= 20 {
            value = checked(delta(self.remote.unwrap(), current))?;
            self.stabilized = true;
        }
        self.offset = Some(value);
        self.updates += 1;
        self.remote_new = false;
        self.ack_new = false;
        Ok(self.offset)
    }
    pub fn report(&self) -> Value {
        json!({"source":"remote_isr_steps_and_transport_acknowledged_local_steps",
            "remote_step":self.remote,"acknowledged_local_step":self.acknowledged,
            "remote_to_local_offset":self.offset,"updates":self.updates,"stabilized":self.stabilized,
            "acknowledgments":self.acknowledgments,"losses":self.losses,"remote_observations":self.observations,
            "native_branch":"integer_smoothing_10_percent_stabilize_after_20",
            "optional_native_clock_features_implemented":false})
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requires_new_remote_and_ack_and_ignores_reordering_and_loss() {
        let mut c = IsrClock::default();
        c.observe_remote(1100);
        assert_eq!(c.update(500).unwrap(), None);
        c.lost();
        assert_eq!(c.update(500).unwrap(), None);
        c.acknowledged(500);
        assert_eq!(c.update(510).unwrap(), Some(600));
        c.observe_remote(1110);
        assert_eq!(c.update(520).unwrap(), Some(600));
        c.acknowledged(490);
        assert_eq!(c.update(520).unwrap(), Some(600));
        c.acknowledged(520);
        assert_eq!(c.update(530).unwrap(), Some(599));
        c.observe_remote(1109);
        c.acknowledged(530);
        assert_eq!(c.update(540).unwrap(), Some(599));
        assert_eq!(IsrClock::default().offset, None);
    }
    #[test]
    fn wraps_steps_and_rejects_reserved_epoch() {
        let mut c = IsrClock::default();
        c.observe_remote(3);
        c.acknowledged(u32::MAX - 6);
        assert_eq!(c.update(0).unwrap(), Some(10));
        c.observe_remote(5);
        c.acknowledged(1);
        assert_eq!(c.update(2).unwrap(), Some(9));
        let mut c = IsrClock::default();
        c.observe_remote(0x80000000);
        c.acknowledged(0);
        assert!(c.update(0).is_err());
        assert_eq!(c.offset, None);
    }
    #[test]
    fn matches_exact_python_float32_and_stabilization_vectors() {
        let rows: Value =
            serde_json::from_str(include_str!("../tests/fixtures/isr-clock-python.json")).unwrap();
        let mut c = IsrClock::default();
        for r in rows.as_array().unwrap() {
            c.observe_remote(r["remote"].as_u64().unwrap() as u32);
            c.acknowledged(r["ack"].as_u64().unwrap() as u32);
            assert_eq!(
                c.update(r["current"].as_u64().unwrap() as u32).unwrap(),
                Some(r["offset"].as_i64().unwrap() as i32)
            );
            assert_eq!(c.stabilized, r["stabilized"].as_bool().unwrap());
        }
    }
}
