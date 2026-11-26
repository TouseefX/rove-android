//! Shared limits between the VM, receive thread, and character predictor.
use serde_json::Value;

pub(crate) const BATCH_LIMIT: usize = 256;
pub(crate) const PENDING_LIMIT: usize = 512;

pub(crate) fn take_batch(pending: &mut Vec<Value>) -> Vec<Value> {
    let count = pending.len().min(BATCH_LIMIT);
    pending.drain(..count).collect()
}
