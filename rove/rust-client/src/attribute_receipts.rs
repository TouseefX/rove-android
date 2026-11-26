//! Bounded receipt metadata only; no attribute values or payloads are retained.
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Default)]
pub(crate) struct AttributeReceipts {
    names: BTreeMap<String, Counts>,
    omitted: u64,
}
#[derive(Default, serde::Serialize)]
struct Counts {
    updates: u64,
    unknown: u64,
    removals: u64,
    last_sequence: u64,
}

/// Bounded observations of the received cloneability flag. Retain only its
/// boolean value and sequence, never arbitrary property strings or payloads.
#[derive(Default)]
pub(crate) struct ArchivableReceipts {
    nodes: BTreeMap<String, (u64, u64, Option<bool>)>,
    omitted: u64,
}
impl ArchivableReceipts {
    pub fn observe(&mut self, id: &str, sequence: u64, value: Option<&Value>) {
        if id.len() > 256 || (self.nodes.len() >= 32 && !self.nodes.contains_key(id)) {
            self.omitted = self.omitted.saturating_add(1);
            return;
        }
        let entry = self.nodes.entry(id.to_owned()).or_default();
        entry.0 = entry.0.saturating_add(1);
        entry.1 = sequence;
        entry.2 = value.and_then(Value::as_bool);
    }
    pub fn report(&self) -> Value {
        json!({"source":"committed_property_changes","nodes":self.nodes.iter().map(|(id,(updates,sequence,value))|
            json!({"id":id,"updates":updates,"last_sequence":sequence,"last_value":value})).collect::<Vec<_>>(),
            "omitted_updates":self.omitted})
    }
}
impl AttributeReceipts {
    pub fn observe(&mut self, action: &Value) {
        if action["kind"] != "attribute" { return; }
        let update = &action["attribute_update"];
        let Some(name) = update["name"].as_str() else { return; };
        if name.len() > 100 || (self.names.len() >= 64 && !self.names.contains_key(name)) {
            self.omitted = self.omitted.saturating_add(1);
            return;
        }
        let counts = self.names.entry(name.to_owned()).or_default();
        counts.updates = counts.updates.saturating_add(1);
        let known = update["known"] == true;
        counts.unknown += u64::from(!known);
        counts.removals += u64::from(known && update.get("value").is_some_and(Value::is_null));
        counts.last_sequence = action["sequence"].as_u64().unwrap_or(0);
    }
    pub fn report(&self) -> Value {
        json!({"source":"committed_incremental_attribute_receipts", "names":self.names,
            "omitted_name_updates":self.omitted,"retains_values":false})
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cloneability_receipts_are_bounded_and_preserve_false() {
        let mut r=ArchivableReceipts::default();
        r.observe("p",1,Some(&json!(true)));
        r.observe("p",2,Some(&json!(false)));
        assert_eq!(r.report()["nodes"][0],json!({"id":"p","updates":2,"last_sequence":2,"last_value":false}));
        for n in 0..100 { r.observe(&format!("n{n}"),3,Some(&json!("not a bool"))); }
        assert_eq!(r.nodes.len(),32);
        assert_eq!(r.omitted,69);
        assert!(!r.report().to_string().contains("not a bool"));
    }
    #[test]
    fn bounded_metadata_distinguishes_unknown_values_from_removal() {
        let mut receipts = AttributeReceipts::default();
        for (sequence, known, value) in [(1,true,json!(123)),(2,false,json!(null)),(3,true,json!(null))] {
            receipts.observe(&json!({"kind":"attribute","sequence":sequence,
                "attribute_update":{"name":"score","known":known,"value":value}}));
        }
        let report = receipts.report();
        assert_eq!(report["names"]["score"], json!({"updates":3,"unknown":1,"removals":1,"last_sequence":3}));
        assert!(!report.to_string().contains("123"));
        for n in 0..1000 {
            receipts.observe(&json!({"kind":"attribute","attribute_update":{"name":format!("n{n}"),"known":true,"value":42}}));
        }
        assert_eq!(receipts.names.len(),64);
        assert_eq!(receipts.omitted,937);
    }
}
