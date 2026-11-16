//! Bounded avatar header cache. Property reads still use the current world.
use rove_client_core::world::WorldView;
use serde_json::Value;

const MAX_CHILDREN: usize = 256;

#[derive(Debug, Default)]
pub(crate) struct CharacterChildren {
    character: Option<String>,
    ids: Vec<String>,
    sequence: u64,
    observed: u64,
    dirty: bool,
    bounded: bool,
    scans: u64,
}

impl CharacterChildren {
    pub fn observe(&mut self, sequence: u64, actions: Option<&[Value]>) {
        if sequence <= self.observed { return; }
        self.observed = sequence;
        self.dirty |= actions.is_none_or(|rows| rows.iter().any(|row| {
            matches!(row["kind"].as_str(), Some("create" | "remove"))
                || (row["kind"] == "set" && (row["fields"].get("Parent").is_some()
                    || row["fields"].get("Name").is_some()))
        }));
    }

    pub fn get<'a>(&'a mut self, view: &WorldView, character: &str) -> Option<&'a [String]> {
        // A caller that skips receipt observation gets the conservative scan.
        let unobserved = view.sequence() != self.sequence && view.sequence() > self.observed;
        if self.character.as_deref() != Some(character) || self.dirty || unobserved {
            self.ids.clear();
            self.bounded = true;
            view.for_each_node(|node| {
                if view.parent(node.id()) != Some(character) { return; }
                if self.ids.len() < MAX_CHILDREN { self.ids.push(node.id().to_owned()); }
                else { self.bounded = false; }
            });
            if !self.bounded { self.ids.clear(); }
            self.scans += 1;
            self.character = Some(character.to_owned());
            self.dirty = false;
        }
        self.sequence = view.sequence();
        self.bounded.then_some(self.ids.as_slice())
    }

    pub fn scans(&self) -> u64 { self.scans }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rove_client_core::world::WorldStore;
    use serde_json::json;

    fn world() -> WorldStore {
        WorldStore::from_snapshot(&json!({"version":1,"sequence":1,"nodes":[
            {"id":"char","class_name":"Model","parent":null,"properties":{}},
            {"id":"leg","class_name":"Part","parent":"char","name":"Left Leg","properties":{"Size":[1,2,1]}},
            {"id":"outside","class_name":"Part","parent":null,"properties":{}}
        ]})).unwrap()
    }

    #[test]
    fn property_receipts_reuse_headers_but_reads_see_new_values() {
        let mut world=world(); let mut cache=CharacterChildren::default();
        assert_eq!(cache.get(&world.view(),"char").unwrap(), &["leg"]);
        let actions=json!([{"sequence":2,"kind":"set","id":"leg","fields":{"Size":[1,4,1]}}]);
        world.apply_delta(&json!({"version":1,"delta":true,"sequence":2,"actions":actions})).unwrap();
        cache.observe(2,actions.as_array().map(Vec::as_slice));
        let view=world.view();
        assert_eq!(cache.get(&view,"char").unwrap(), &["leg"]);
        assert_eq!(view.get_property("leg","Size"),Some(json!([1,4,1])));
        assert_eq!(cache.scans(),1);
    }

    #[test]
    fn topology_and_unobserved_revisions_invalidate() {
        let mut world=world(); let mut cache=CharacterChildren::default();
        cache.get(&world.view(),"char");
        let actions=json!([{"sequence":2,"kind":"set","id":"outside","fields":{"Parent":{"$ref":"char"}}}]);
        world.apply_delta(&json!({"version":1,"delta":true,"sequence":2,"actions":actions})).unwrap();
        cache.observe(2,actions.as_array().map(Vec::as_slice));
        assert_eq!(cache.get(&world.view(),"char").unwrap().len(),2);
        assert_eq!(cache.scans(),2);
        world.apply_delta(&json!({"version":1,"delta":true,"sequence":3,"actions":[{"sequence":3,"kind":"remove","id":"leg"}]})).unwrap();
        assert_eq!(cache.get(&world.view(),"char").unwrap(), &["outside"]);
        assert_eq!(cache.scans(),3);
        assert!(cache.get(&world.view(),"other-character").unwrap().is_empty());
    }

    #[test]
    fn oversized_character_falls_back_without_truncating_physics_inputs() {
        let nodes=(0..=MAX_CHILDREN).map(|i|json!({"id":i.to_string(),"class_name":"Part","parent":"char","properties":{}})).collect::<Vec<_>>();
        let world=WorldStore::from_snapshot(&json!({"version":1,"sequence":1,"nodes":nodes})).unwrap();
        let mut cache=CharacterChildren::default();
        assert!(cache.get(&world.view(),"char").is_none());
        assert!(cache.ids.is_empty());
    }
}
