//! Received Tags bytes and fresh-complete constructor defaults, matching the
//! Python received_tags adapter. Shared strings are resolved by replication.
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

fn decode(value: &Value) -> Option<Vec<String>> {
    let bytes = if let Some(text) = value.as_str() {
        if text.len() > 1024 * 1024 { return None; }
        text.as_bytes().to_vec()
    } else {
        let hex = value.get("$bytes_hex")?.as_str()?;
        if hex.len() > 2 * 1024 * 1024 || hex.len() % 2 != 0 { return None; }
        hex.as_bytes().chunks_exact(2).map(|pair| {
            let a=(pair[0] as char).to_digit(16)?;
            let b=(pair[1] as char).to_digit(16)?;
            Some((a*16+b) as u8)
        }).collect::<Option<Vec<_>>>()?
    };
    let mut tags=BTreeSet::new();
    for raw in bytes.split(|byte| *byte==0).filter(|raw| !raw.is_empty()) {
        if raw.len()>65_536 { return None; }
        tags.insert(std::str::from_utf8(raw).ok()?.to_owned());
        if tags.len()>256 { return None; }
    }
    Some(tags.into_iter().collect())
}

pub(crate) fn snapshot(blob: &Value) -> Map<String, Value> {
    match decode(blob) {
        Some(tags) => json!({"collection_tags":tags,"collection_tags_known":true}),
        None => json!({"collection_tags":[],"collection_tags_known":false}),
    }.as_object().unwrap().clone()
}

pub(crate) fn creation(blob: Option<&Value>, fresh_complete: bool) -> Map<String, Value> {
    if let Some(blob)=blob { return snapshot(blob); }
    if fresh_complete {
        return json!({"collection_tags":[],"collection_tags_known":true,
            "collection_tags_origin":"constructor_default"}).as_object().unwrap().clone();
    }
    Map::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::WorldStore;

    #[test]
    fn tag_payloads_match_python_and_invalid_values_stay_unknown() {
        assert_eq!(snapshot(&json!("b\0a\0\0b\0☃"))["collection_tags"],json!(["a","b","☃"]));
        assert_eq!(snapshot(&json!({"$bytes_hex":"6200610062"}))["collection_tags"],json!(["a","b"]));
        for value in [Value::Null,json!({"shared":"unresolved"}),json!({"$bytes_hex":"ff"}),
            json!({"$bytes_hex":"0"}),json!({"$bytes_hex":"zz"}),json!("x".repeat(65_537))] {
            assert_eq!(snapshot(&value)["collection_tags_known"],false);
        }
        let too_many=(0..257).map(|i|i.to_string()).collect::<Vec<_>>().join("\0");
        assert_eq!(snapshot(&json!(too_many))["collection_tags_known"],false);
        assert!(creation(None,false).is_empty());
        assert_eq!(creation(None,true)["collection_tags_known"],true);
        assert_eq!(creation(Some(&Value::Null),true)["collection_tags_known"],false);
    }

    #[test]
    fn canonical_tag_receipts_preserve_old_views_and_explicit_unknown_state() {
        let mut store=WorldStore::from_snapshot(&json!({"version":1,"sequence":0,"nodes":[]})).unwrap();
        let create=|sequence,complete|json!({"sequence":sequence,"kind":"create","id":"p","node":{
            "id":"p","class_name":"Part","received_creation_complete":complete,"properties":{}}});
        let apply=|store:&mut WorldStore,action:Value| {
            store.apply_delta(&json!({"version":1,"delta":true,"sequence":action["sequence"],"actions":[action]})).unwrap();
        };
        apply(&mut store,create(1,true));
        let before=store.view();
        assert_eq!(before.read_only_node("p").unwrap().record_field("collection_tags_known"),Some(json!(true)));
        apply(&mut store,json!({"sequence":2,"kind":"set","id":"p","fields":{"Tags":"a\0b"}}));
        assert_eq!(store.view().read_only_node("p").unwrap().record_field("collection_tags"),Some(json!(["a","b"])));
        assert_eq!(before.read_only_node("p").unwrap().record_field("collection_tags"),Some(json!([])));
        apply(&mut store,json!({"sequence":3,"kind":"set","id":"p","fields":{"Tags":{"$bytes_hex":"ff"}}}));
        assert_eq!(store.view().read_only_node("p").unwrap().record_field("collection_tags_known"),Some(json!(false)));
        apply(&mut store,create(4,true));
        assert_ne!(store.view().read_only_node("p").unwrap().record_field("collection_tags_known"),Some(json!(true)));
        apply(&mut store,json!({"sequence":5,"kind":"set","id":"p","fields":{"Tags":""}}));
        assert_eq!(store.view().read_only_node("p").unwrap().record_field("collection_tags_known"),Some(json!(true)));
    }
}
