//! AttributesReplicate snapshots, shared by the canonical store and VM adapter.
//! A bad explicit blob invalidates the map; it is never a known empty map.
use crate::{replication::read_attribute_value, wire::{Reader, WireError}};
use serde_json::{json, Map, Value};

fn decode(value: &Value) -> Result<Map<String, Value>, WireError> {
    let bytes = if let Some(text) = value.as_str() {
        if text.len() > 1024 * 1024 { return Err(WireError::new("Attribute blob exceeds limit")); }
        text.as_bytes().to_vec()
    } else {
        let text = value.get("$bytes_hex").and_then(Value::as_str)
            .filter(|s| s.len() <= 2 * 1024 * 1024 && s.len() % 2 == 0)
            .ok_or_else(|| WireError::new("Invalid attribute blob"))?;
        text.as_bytes().chunks_exact(2).map(|c| {
            let a = (c[0] as char).to_digit(16);
            let b = (c[1] as char).to_digit(16);
            a.zip(b).map(|(a,b)| (a*16+b) as u8)
                .ok_or_else(|| WireError::new("Invalid attribute hex"))
        }).collect::<Result<Vec<_>,_>>()?
    };
    let mut result = Map::new();
    if bytes.is_empty() { return Ok(result); }
    let mut reader = Reader::new(&bytes);
    let count = reader.u32()?;
    if count > 1024 { return Err(WireError::new("Attribute count exceeds limit")); }
    for _ in 0..count {
        let len = reader.u32()? as usize;
        if !(1..=100).contains(&len) { return Err(WireError::new("Invalid attribute name")); }
        let name = std::str::from_utf8(reader.take(len)?)
            .map_err(|_| WireError::new("Invalid attribute name"))?;
        if name.contains('\0') || result.contains_key(name) {
            return Err(WireError::new("Invalid attribute name"));
        }
        result.insert(name.to_owned(), read_attribute_value(&mut reader)?);
    }
    reader.finish()?;
    Ok(result)
}

pub(crate) fn snapshot(blob: &Value) -> Map<String, Value> {
    match decode(blob) {
        Ok(values) => json!({"attributes":values,"attributes_known":true}).as_object().unwrap().clone(),
        Err(_) => json!({"attributes":{},"attributes_known":false}).as_object().unwrap().clone(),
    }
}

pub(crate) fn creation(blob: Option<&Value>, fresh_complete: bool) -> Map<String, Value> {
    if let Some(blob) = blob { return snapshot(blob); }
    if fresh_complete {
        return json!({"attributes":{},"attributes_known":true,"attributes_origin":"constructor_default"})
            .as_object().unwrap().clone();
    }
    Map::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::WorldStore;
    fn blob(bytes: &[u8]) -> Value {
        json!({"$bytes_hex":bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()})
    }
    #[test]
    fn fixed_length_strings_and_binary_values_match_python_layout() {
        // Two named values: a UTF-8 string and a binary string.
        let raw = [2,0,0,0, 1,0,0,0,b'a', 2,3,0,0,0,b'h',b'e',b'y',
                   1,0,0,0,b'b', 2,2,0,0,0,255,0];
        let map = snapshot(&blob(&raw));
        assert_eq!(map["attributes_known"], true);
        assert_eq!(map["attributes"], json!({"a":"hey","b":{"$bytes_hex":"ff00"}}));
        for raw in [&raw[..raw.len()-1], &[1,0,0,0,1,0,0,0,b'x',255][..], &[0,0,0,0,1][..]] {
            assert_eq!(snapshot(&blob(raw))["attributes_known"], false);
        }
        assert!(creation(None, false).is_empty());
        assert_eq!(creation(None, true)["attributes_known"], true);
        assert_eq!(creation(Some(&Value::Null), true)["attributes_known"], false);
    }
    #[test]
    fn canonical_creation_updates_and_bad_blobs_preserve_knowledge() {
        let mut store = WorldStore::from_snapshot(&json!({"version":1,"sequence":0,"nodes":[]})).unwrap();
        let create = |sequence| json!({"sequence":sequence,"kind":"create","id":"p","node":{
            "id":"p","class_name":"LocalScript","received_creation_complete":true,"properties":{}}});
        let apply = |store: &mut WorldStore, action: Value| {
            store.apply_delta(&json!({"version":1,"delta":true,"sequence":action["sequence"],"actions":[action]})).unwrap();
        };
        apply(&mut store, create(1));
        let before = store.view();
        assert_eq!(before.read_only_node("p").unwrap().record_field("attributes_known"),Some(json!(true)));
        apply(&mut store, json!({"sequence":2,"kind":"attribute","id":"p","attribute_update":{"name":"x","known":true,"value":4}}));
        assert_eq!(store.view().read_only_node("p").unwrap().metadata_field("attributes","x"),Some(json!(4)));
        apply(&mut store, json!({"sequence":3,"kind":"set","id":"p","fields":{"AttributesReplicate":blob(&[255])}}));
        let bad = store.view().read_only_node("p").unwrap();
        assert_eq!(bad.record_field("attributes_known"),Some(json!(false)));
        assert!(bad.metadata_field("attributes","x").is_none());
        apply(&mut store, json!({"sequence":4,"kind":"set","id":"p","fields":{"AttributesReplicate":blob(&[])}}));
        assert_eq!(store.view().read_only_node("p").unwrap().record_field("attributes_known"),Some(json!(true)));
        // A repeated create does not prove the instance was freshly constructed.
        apply(&mut store, create(5));
        assert_eq!(store.view().read_only_node("p").unwrap().record_field("attributes_known"),None);
        assert_eq!(before.read_only_node("p").unwrap().record_field("attributes_known"),Some(json!(true)));
    }
}
