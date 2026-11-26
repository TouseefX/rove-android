//! Per-session code preparation on the VM owner thread. No historical payloads.
use crate::{
    bytecode_format,
    engine_world_view::SharedProjectedWorldView,
    program_check::{unpack_container, PublicTrust},
};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
};
const MAX_CODE: usize = 128 * 1024;
const MAX_CATALOG: usize = 16 * 1024 * 1024;
fn hash(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}
fn bytes(v: &Value) -> Option<Vec<u8>> {
    let h = v.get("$bytes_hex")?.as_str()?;
    if h.len() > MAX_CODE * 2 || h.len() % 2 != 0 {
        return None;
    }
    h.as_bytes()
        .chunks_exact(2)
        .map(|p| Some(((p[0] as char).to_digit(16)? * 16 + (p[1] as char).to_digit(16)?) as u8))
        .collect()
}
fn candidates(v: &Value, out: &mut Vec<Vec<u8>>, depth: usize) {
    if depth > 3 || out.len() >= 8 {
        return;
    }
    if let Some(b) = bytes(v) {
        if !b.is_empty() {
            out.push(b);
        }
        return;
    }
    if let Some(a) = v.as_array() {
        let blocks: Vec<_> = a
            .iter()
            .filter_map(bytes)
            .filter(|b| !b.is_empty())
            .collect();
        if blocks.len() == 2 && blocks.iter().map(Vec::len).sum::<usize>() <= MAX_CODE {
            out.push(blocks.concat());
        }
        for item in a {
            candidates(item, out, depth + 1);
        }
    }
}
fn prepared(
    original: &[u8],
    raw: &[u8],
    field: &str,
    trust: Option<&PublicTrust>,
    small: bool,
) -> Result<Value, String> {
    let short = small && original.len() < 256;
    if !short {
        trust
            .ok_or("public_trust_unavailable")?
            .verify(original)
            .map_err(|e| e.to_string())?;
    }
    let normalized = bytecode_format::normalize(original, Some(203)).map_err(|e| e.to_string())?;
    let m = &normalized.metadata;
    if short
        && (!(m["bytecode_version"] == 13 || m["bytecode_version"] == 14)
            || m["type_version"] != 3
            || m["trailer_bytes"] != 0
            || m["prototype_extension_bytes"] != 0
            || normalized.bytes.len() != original.len())
    {
        return Err("invalid_short_script_bytecode".into());
    }
    let mut entry = json!({"bytecode_hex":hex(&normalized.bytes),"original_bytecode_hex":hex(original),
  "signature_verified":!short,"provenance":if short {"session_small_bytecode"}else{"verified_bytecode"},
  "origin":field,"input_field":field,"input_sha256":hash(raw),"original_sha256":hash(original),
  "normalized_sha256":hash(&normalized.bytes),"digest":hash(original),
  "bytecode_version":m["bytecode_version"],"type_version":m["type_version"]});
    if short {
        entry["admission"] = json!("pinned_session_rsb_luau13_14_no_trailer_under_256");
    }
    Ok(entry)
}
fn prepare(props: &Value, trust: Option<&PublicTrust>, allow_small: bool) -> Value {
    let source = &props["Source"];
    let raw = bytes(source);
    let text = source.as_str().map(str::to_owned).or_else(|| {
        raw.as_ref()
            .and_then(|v| std::str::from_utf8(v).ok())
            .filter(|s| {
                s.chars()
                    .all(|c| c >= ' ' || matches!(c, '\n' | '\r' | '\t'))
            })
            .map(str::to_owned)
    });
    if let Some(s) = text {
        if s.len() > MAX_CODE {
            return json!({"reason":"source_size_limit"});
        }
        return json!({"source":s,"provenance":"source","origin":"Source","input_field":"Source","input_sha256":hash(s.as_bytes()),"source_sha256":hash(s.as_bytes())});
    }
    let mut failure = json!({"reason":"code_unavailable"});
    let mut count = 0;
    for field in ["Source", "Bytecode", "ScriptBytecode"] {
        let mut list = Vec::new();
        candidates(&props[field], &mut list, 0);
        for raw in list {
            if count >= 8 {
                break;
            }
            count += 1;
            if let Ok(unpacked) = unpack_container(&raw) {
                if let Ok(v) = prepared(&unpacked, &raw, field, trust, allow_small) {
                    return v;
                }
            }
            match prepared(&raw, &raw, field, trust, false) {
                Ok(v) => return v,
                Err(e) => failure = json!({"reason":e,"origin":field,"input_sha256":hash(&raw)}),
            }
        }
    }
    failure
}

pub(crate) struct Catalog {
    trust: Option<PublicTrust>,
    allow_small: bool,
    entries: Map<String, Value>,
    digests: HashMap<String, String>,
    bytes: usize,
    preparations: u64,
}
impl Catalog {
    pub fn new(root: &Path, allow_small: bool) -> Self {
        Self {
            trust: PublicTrust::from_files(
                &root.join("inputs/bytecode-signature-v1-public.bin"),
                &root.join("inputs/bytecode-signature-v1-public.json"),
            )
            .ok(),
            allow_small,
            entries: Map::new(),
            digests: HashMap::new(),
            bytes: 0,
            preparations: 0,
        }
    }
    // Only the current frame's raw nodes are read. No WorldView survives this call.
    pub fn update(
        &mut self,
        view: &SharedProjectedWorldView,
        header: &Value,
        initial: bool,
    ) -> Result<Option<Value>, String> {
        let mut ids = BTreeSet::new();
        if initial {
            view.for_each_node(|n| {
                if matches!(n.class_name(), "Script" | "LocalScript" | "ModuleScript") {
                    ids.insert(n.id().to_owned());
                }
            });
        } else if let Some(actions) = header["actions"].as_array() {
            for a in actions {
                let id = a["id"].as_str().or_else(|| a["node"]["id"].as_str());
                if let Some(id) = id {
                    if matches!(a["kind"].as_str(), Some("create" | "remove"))
                        || ["Source", "Bytecode", "ScriptBytecode", "Name"]
                            .iter()
                            .any(|f| a["fields"].get(*f).is_some())
                    {
                        ids.insert(id.to_owned());
                    }
                }
            }
        }
        let mut changed = initial;
        for id in ids {
            let node = view
                .lookup_node(&id)
                .filter(|n| matches!(n.class_name(), "Script" | "LocalScript" | "ModuleScript"));
            let Some(node) = node else {
                if let Some(v) = self.entries.remove(&id) {
                    self.bytes -= serde_json::to_vec(&v).unwrap().len();
                    self.digests.remove(&id);
                    changed = true;
                }
                continue;
            };
            let raw = node.raw();
            let props = json!({"Source":raw.property("Source"),"Bytecode":raw.property("Bytecode"),"ScriptBytecode":raw.property("ScriptBytecode")});
            let digest = hash(
                &serde_json::to_vec(&json!([raw.created_sequence(), props, node.name().ok()]))
                    .map_err(|e| e.to_string())?,
            );
            if self.digests.get(&id) == Some(&digest) {
                continue;
            }
            if !self.entries.contains_key(&id) && self.entries.len() >= 16384 {
                return Err("script catalog exceeds 16384 entries".into());
            }
            let mut entry = prepare(&props, self.trust.as_ref(), self.allow_small);
            self.preparations += 1;
            entry["class_name"] = json!(node.class_name());
            let old = self
                .entries
                .remove(&id)
                .map_or(0, |v| serde_json::to_vec(&v).unwrap().len());
            self.bytes -= old;
            let mut size = serde_json::to_vec(&entry).unwrap().len();
            if self.bytes + size > MAX_CATALOG - 16384 * 64 {
                entry = json!({"reason":"catalog_size_limit","class_name":node.class_name()});
                size = serde_json::to_vec(&entry).unwrap().len();
            }
            self.bytes += size;
            self.entries.insert(id.clone(), entry);
            self.digests.insert(id, digest);
            changed = true;
        }
        Ok(changed.then(|| Value::Object(self.entries.clone())))
    }
    pub fn report(&self) -> Value {
        let mut reasons = Map::new();
        let mut admitted = 0;
        for e in self.entries.values() {
            if let Some(r) = e["reason"].as_str() {
                let n = reasons.get(r).and_then(Value::as_u64).unwrap_or(0);
                reasons.insert(r.into(), json!(n + 1));
            } else {
                admitted += 1;
            }
        }
        json!({"entries":self.entries.len(),"admitted":admitted,"bytes":self.bytes,"preparations":self.preparations,"blocked":reasons})
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_plain_source_and_binary_text_are_bounded() {
        for v in [json!("return 42"), json!({"$bytes_hex":hex(b"return 42")})] {
            assert_eq!(
                prepare(&json!({"Source":v}), None, false)["source"],
                "return 42"
            );
        }
        assert_eq!(
            prepare(&json!({"Source":"x".repeat(MAX_CODE+1)}), None, false)["reason"],
            "source_size_limit"
        );
    }
    #[test]
    fn catalog_untrusted_bytes_never_get_an_admission_label() {
        let e = prepare(
            &json!({"Source":[{"$bytes_hex":"0e0300"},{"$bytes_hex":"0000"}]}),
            None,
            true,
        );
        assert!(e.get("bytecode_hex").is_none());
        assert!(e.get("signature_verified").is_none());
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::{engine_world_projection::ProjectionContext, engine_world_view::SharedWorldView};
    use rove_client_core::{schema::Schema, world::WorldStore};
    fn view(store: &WorldStore) -> SharedProjectedWorldView {
        let schema = Schema {
            version: 3,
            flags: 0,
            enums: vec![],
            classes: vec![],
            properties: vec![],
            events: vec![],
            strings: vec![],
            names: vec![],
            lr_types: vec![],
            parent_property_id: 0,
            decoded_bytes: 0,
        };
        SharedWorldView::projected(
            store.view(),
            ProjectionContext::new(&schema, HashMap::new()).unwrap(),
        )
    }
    #[test]
    fn catalog_current_receipts_update_remove_and_cache_without_retaining_world() {
        let mut store=WorldStore::from_snapshot(&json!({"version":1,"nodes":[{"id":"m","class_name":"ModuleScript","properties":{"Source":"return 42"}}]})).unwrap();
        let mut catalog = Catalog::new(Path::new("/nonexistent"), false);
        let first = catalog
            .update(&view(&store), &json!({}), true)
            .unwrap()
            .unwrap();
        assert_eq!(first["m"]["source"], "return 42");
        let unrelated = json!({"version":1,"delta":true,"sequence":1,"actions":[{"kind":"set","sequence":1,"id":"m","fields":{"Name":"Renamed"}}]});
        store.apply_delta(&unrelated).unwrap();
        catalog.update(&view(&store), &unrelated, false).unwrap();
        let count = catalog.preparations;
        assert!(catalog
            .update(&view(&store), &unrelated, false)
            .unwrap()
            .is_none());
        assert_eq!(catalog.preparations, count);
        let change = json!({"version":1,"delta":true,"sequence":2,"actions":[{"kind":"set","sequence":2,"id":"m","fields":{"Source":"return 99"}}]});
        store.apply_delta(&change).unwrap();
        assert_eq!(
            catalog
                .update(&view(&store), &change, false)
                .unwrap()
                .unwrap()["m"]["source"],
            "return 99"
        );
        let remove = json!({"version":1,"delta":true,"sequence":3,"actions":[{"kind":"remove","sequence":3,"id":"m"}]});
        store.apply_delta(&remove).unwrap();
        assert_eq!(
            catalog.update(&view(&store), &remove, false).unwrap(),
            Some(json!({}))
        );
        assert_eq!(catalog.bytes, 0);
    }
    #[test]
    fn catalog_admitted_source_runs_in_official_vm() {
        let store=WorldStore::from_snapshot(&json!({"version":1,"nodes":[{"id":"m","class_name":"ModuleScript","name":"Probe","parent":"workspace","properties":{"Source":"return {answer=42}"}},{"id":"workspace","class_name":"Workspace","name":"Workspace","properties":{}}]})).unwrap();
        let mut catalog = Catalog::new(Path::new("/nonexistent"), false);
        let mut snapshot = store.to_snapshot();
        snapshot["services"] = json!({"Workspace":"workspace"});
        snapshot["script_sources"] = catalog
            .update(&view(&store), &json!({}), true)
            .unwrap()
            .unwrap();
        let mut vm = crate::runtime_owner::Runtime::new(snapshot, 32).unwrap();
        let result = vm
            .execute_control("return require(workspace.Probe).answer".into(), 2000)
            .unwrap();
        crate::runtime_owner::successful(&result).unwrap();
        assert_eq!(result["values"][0].as_f64(), Some(42.0), "{result}");
    }
}
