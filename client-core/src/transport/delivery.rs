//! Bounded, connection-local datagram delivery receipts. A successful queue
//! operation is not an ACK; only the QUIC peer's ACK path produces one.
use quinn_proto::DatagramObserver;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Mutex,
};
const LIMIT: usize = 128;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Delivery {
    Acknowledged(u32),
    Lost(u32),
}
#[derive(Default)]
struct State {
    pending: BTreeMap<Vec<u8>, u32>,
    packets: BTreeMap<u64, Vec<u32>>,
    outcomes: VecDeque<Delivery>,
    forgotten: u64,
    acknowledged: u64,
    lost: u64,
}
#[derive(Default)]
pub struct DatagramDelivery {
    state: Mutex<State>,
}
impl DatagramDelivery {
    pub fn register(&self, bytes: Vec<u8>, step: u32) -> Result<(), &'static str> {
        if bytes.len() > 1200 {
            return Err("delivery-datagram-too-large");
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "delivery-observer-unavailable")?;
        if state.pending.contains_key(&bytes) {
            return Err("delivery-datagram-already-registered");
        }
        // Forgotten observations are unknown, never synthesized as losses/ACKs.
        if state.pending.len() >= LIMIT {
            state.pending.pop_first();
            state.forgotten += 1;
        }
        state.pending.insert(bytes, step);
        Ok(())
    }
    pub fn drain(&self) -> Vec<Delivery> {
        self.state.lock().unwrap().outcomes.drain(..).collect()
    }
    pub fn report(&self) -> Value {
        let s = self.state.lock().unwrap();
        json!({"acknowledged":s.acknowledged,"lost":s.lost,"pending":s.pending.len(),
            "in_flight_packets":s.packets.len(),"forgotten_observations":s.forgotten,
            "source":"quic_data_packet_ack_and_loss"})
    }
    fn finish(&self, packet: u64, ack: bool) {
        let mut s = self.state.lock().unwrap();
        let Some(steps) = s.packets.remove(&packet) else {
            return;
        };
        for step in steps {
            if s.outcomes.len() >= LIMIT {
                s.outcomes.pop_front();
                s.forgotten += 1;
            }
            if ack {
                s.acknowledged += 1;
                s.outcomes.push_back(Delivery::Acknowledged(step));
            } else {
                s.lost += 1;
                s.outcomes.push_back(Delivery::Lost(step));
            }
        }
    }
}
impl DatagramObserver for DatagramDelivery {
    fn sent(&self, packet: u64, bytes: &[u8]) {
        let mut s = self.state.lock().unwrap();
        let Some(step) = s.pending.remove(bytes) else {
            return;
        };
        if !s.packets.contains_key(&packet) && s.packets.len() >= LIMIT {
            if let Some((_, steps)) = s.packets.pop_first() {
                s.forgotten += steps.len() as u64;
            }
        }
        let steps = s.packets.entry(packet).or_default();
        if steps.len() < LIMIT {
            steps.push(step);
        } else {
            s.forgotten += 1;
        }
    }
    fn acknowledged(&self, packet: u64) {
        self.finish(packet, true);
    }
    fn lost(&self, packet: u64) {
        self.finish(packet, false);
    }
}

/// Register the exact HTTP/3 datagram before handing it to the transport driver.
pub fn send_tracked(
    connection: &wtransport::Connection,
    tracker: &DatagramDelivery,
    wire: u32,
    sequence: u64,
    payload: &[u8],
    step: u32,
) -> Result<(), &'static str> {
    let packet = crate::channels::encode_datagram(wire, sequence, payload)
        .map_err(|_| "delivery-framing-invalid")?;
    let h3 = wtransport::proto::datagram::Datagram::new(
        wtransport::proto::ids::QStreamId::from_session_id(connection.session_id()),
        &packet,
    );
    let mut bytes = vec![0; h3.write_size()];
    h3.write(&mut bytes)
        .map_err(|_| "delivery-framing-invalid")?;
    tracker.register(bytes, step)?;
    connection
        .send_datagram(packet)
        .map_err(|_| "delivery-submission-failed")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_exact_sent_packets_can_be_acknowledged_once() {
        let s = DatagramDelivery::default();
        let other = DatagramDelivery::default();
        s.register(vec![1, 2], 42).unwrap();
        s.acknowledged(1);
        assert!(s.drain().is_empty());
        s.sent(2, &[1, 3]);
        s.acknowledged(2);
        assert!(s.drain().is_empty());
        s.sent(3, &[1, 2]);
        other.acknowledged(3);
        assert!(s.drain().is_empty());
        s.acknowledged(3);
        s.acknowledged(3);
        s.lost(3);
        assert_eq!(s.drain(), vec![Delivery::Acknowledged(42)]);
        s.register(vec![4], 43).unwrap();
        s.sent(4, &[4]);
        s.lost(4);
        s.acknowledged(4);
        assert_eq!(s.drain(), vec![Delivery::Lost(43)]);
    }
    #[test]
    fn queues_are_bounded_without_inventing_delivery() {
        let s = DatagramDelivery::default();
        for n in 0..300u32 {
            s.register(n.to_le_bytes().to_vec(), n).unwrap();
        }
        assert_eq!(s.report()["pending"], 128);
        assert!(s.drain().is_empty());
        for n in 0..300u32 {
            s.sent(n as u64, &n.to_le_bytes());
            s.acknowledged(n as u64);
        }
        assert_eq!(s.drain().len(), 128);
        assert_eq!(s.report()["acknowledged"], 128);
        assert_eq!(s.report()["lost"], 0);
    }
}
