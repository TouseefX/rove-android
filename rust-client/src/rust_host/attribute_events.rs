//! Ordered attribute notifications without cloning instance property maps.
use super::world::{self, WorldRef};
use mlua::Lua;
use serde_json::{Map, Value as Json};
use std::collections::{BTreeSet, HashMap};

#[derive(Default)]
pub(super) struct Receipt(HashMap<usize, Vec<(String, String)>>);
impl Receipt {
    pub(super) fn publish(&mut self, lua: &Lua, ctx: &WorldRef, index: usize) -> mlua::Result<()> {
        for (id, name) in self.0.remove(&index).unwrap_or_default() {
            world::fire_attribute_changed(lua, ctx, &id, &name)?;
        }
        Ok(())
    }
}

struct State {
    values: Map<String, Json>,
    known: bool,
}
impl State {
    fn observable(&self, name: &str) -> Option<&Json> {
        self.values
            .get(name)
            .or(if self.known { Some(&Json::Null) } else { None })
    }
    fn update(&mut self, name: &str, value: Option<&Json>) -> bool {
        let changed = !equal(self.observable(name), value);
        if let Some(value) = value {
            if value.is_null() && self.known {
                self.values.remove(name);
            } else {
                self.values.insert(name.to_owned(), value.clone());
            }
        } else {
            // An unavailable value is distinct from a known nil. Keep that
            // uncertainty rather than synthesizing a complete attribute map.
            self.values.remove(name);
            self.known = false;
        }
        changed
    }
    fn replace(&mut self, values: Map<String, Json>, known: bool) -> Vec<String> {
        let next = Self { values, known };
        let names: BTreeSet<_> = self
            .values
            .keys()
            .chain(next.values.keys())
            .cloned()
            .collect();
        let changed = names
            .into_iter()
            .filter(|name| !equal(self.observable(name), next.observable(name)))
            .collect();
        *self = next;
        changed
    }
}
fn equal(a: Option<&Json>, b: Option<&Json>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => world::attribute_value_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}
pub(super) fn relevant(action: &Json) -> bool {
    match action["kind"].as_str() {
        Some("attribute") => true,
        Some("set" | "set_property") => action.get("attribute_snapshot").is_some(),
        _ => false,
    }
}

/// Prepare before validation; publish only after the transaction commits.
/// Actor journals use top-level name/value, received updates attribute_update.
pub(super) fn prepare(ctx: &WorldRef, actions: &[Json], previous: Option<u64>) -> Receipt {
    let fresh = |a: &Json| previous.is_none_or(|seq| a["sequence"].as_u64().unwrap_or(0) > seq);
    let mut receipt = Receipt::default();
    if !actions.iter().any(|a| fresh(a) && relevant(a)) {
        return receipt;
    }
    let world = ctx.borrow();
    let touched: std::collections::HashSet<_> = actions
        .iter()
        .filter(|a| fresh(a) && relevant(a))
        .filter_map(|a| a["id"].as_str())
        .collect();
    let mut states = HashMap::<String, State>::new();
    for (index, action) in actions.iter().enumerate().filter(|(_, a)| fresh(a)) {
        if action["kind"] == "create" {
            if let Some(id) = action["node"]["id"]
                .as_str()
                .filter(|id| touched.contains(id))
            {
                states.insert(
                    id.to_owned(),
                    State {
                        values: action["node"]["attributes"]
                            .as_object()
                            .cloned()
                            .unwrap_or_default(),
                        known: action["node"]["attributes_known"]
                            .as_bool()
                            .unwrap_or(false),
                    },
                );
            }
            continue;
        }
        if !relevant(action) {
            continue;
        }
        let Some(id) = action["id"].as_str() else {
            continue;
        };
        if !states.contains_key(id) {
            let Some(node) = world.nodes.get(id) else {
                continue;
            };
            states.insert(
                id.to_owned(),
                State {
                    values: node.attributes.clone(),
                    known: node.attributes_known,
                },
            );
        }
        let state = states.get_mut(id).unwrap();
        let mut names = Vec::new();
        match action["kind"].as_str() {
            Some("attribute") => {
                let (name, value) = if let Some(update) = action.get("attribute_update") {
                    (
                        update["name"].as_str(),
                        if update["known"].as_bool().unwrap_or(false) {
                            update.get("value")
                        } else {
                            None
                        },
                    )
                } else {
                    (action["name"].as_str(), action.get("value"))
                };
                if let Some(name) = name {
                    if state.update(name, value) {
                        names.push(name.to_owned());
                    }
                }
            }
            Some("set" | "set_property") => {
                let snapshot = &action["attribute_snapshot"];
                if let (Some(values), Some(known)) = (
                    snapshot["attributes"].as_object(),
                    snapshot["attributes_known"].as_bool(),
                ) {
                    names = state.replace(values.clone(), known);
                }
            }
            _ => {}
        }
        if !names.is_empty() {
            receipt.0.insert(
                index,
                names
                    .into_iter()
                    .map(|name| (id.to_owned(), name))
                    .collect(),
            );
        }
    }
    receipt
}

#[cfg(test)]
mod tests {
    use super::world::World;
    use super::*;
    use crate::{engine_world_projection::ProjectionContext, engine_world_view::SharedWorldView};
    use rove_client_core::{schema::Schema, world::WorldStore};
    use serde_json::json;
    use std::{cell::RefCell, rc::Rc};
    #[test]
    fn attribute_planning_only_materializes_the_touched_attribute_record() {
        let mut nodes = vec![
            json!({"id":"game","class_name":"DataModel","name":"game"}),
            json!({"id":"folder","class_name":"Folder","name":"folder"}),
        ];
        for i in 0..1000 {
            nodes.push(
                json!({"id":format!("p{i}"),"class_name":"Part","name":"part","parent":"folder"}),
            );
        }
        let store =
            WorldStore::from_snapshot(&json!({"version":1,"sequence":0,"nodes":nodes})).unwrap();
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
        let view = SharedWorldView::projected(
            store.view(),
            ProjectionContext::new(&schema, HashMap::new()).unwrap(),
        );
        let ctx = Rc::new(RefCell::new(
            World::from_snapshot(&json!({"version":1,"sequence":0,"nodes":[]}), Some(&view))
                .unwrap(),
        ));
        let before = ctx.borrow().nodes.materialized_count();
        let movement = prepare(
            &ctx,
            &[json!({"kind":"set","sequence":1,"id":"p0","fields":{"Position":[1,2,3]}})],
            Some(0),
        );
        assert!(movement.0.is_empty());
        assert_eq!(ctx.borrow().nodes.materialized_count(), before);
        let attributes = prepare(
            &ctx,
            &[
                json!({"kind":"attribute","sequence":1,"id":"p0","attribute_update":{"name":"Score","known":true,"value":1}}),
            ],
            Some(0),
        );
        assert_eq!(
            attributes.0[&0],
            vec![("p0".to_owned(), "Score".to_owned())]
        );
        assert_eq!(ctx.borrow().nodes.materialized_count(), before + 1);
    }
}
