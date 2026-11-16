//! Assembly anchoring query; collision contacts are not rigid connections.
use super::*;

fn grounded(_lua: &Lua, ctx: &WorldRef, id: &str, _args: MultiValue) -> mlua::Result<MultiValue> {
    if !ctx.borrow().nodes.is_live(id) || !world::is_a(ctx, id, "BasePart") {
        return Err(err("IsGrounded requires a live BasePart"));
    }
    let anchored = |part: &str| {
        world::get_property(ctx, part, "Anchored")
            .ok()
            .and_then(|v| v.as_bool())
    };
    if anchored(id) == Some(true) {
        return Ok(MultiValue::from_vec(vec![Value::Boolean(true)]));
    }
    let mut unknown = anchored(id).is_none();
    for member in mass_assembly_members(ctx, id)? {
        match anchored(&member) {
            Some(true) => return Ok(MultiValue::from_vec(vec![Value::Boolean(true)])),
            Some(false) => {}
            None => unknown = true,
        }
    }
    if unknown {
        return Err(err("assembly Anchored state is unavailable"));
    }
    Ok(MultiValue::from_vec(vec![Value::Boolean(false)]))
}
pub(super) fn install(ctx: &WorldRef) {
    world::register_method(ctx, "BasePart", "IsGrounded", grounded);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine_world_projection::ProjectionContext, engine_world_view::SharedWorldView};
    use rove_client_core::{schema::Schema, world::WorldStore};
    use std::collections::HashMap;
    #[test]
    fn shared_grounding_traverses_the_assembly_without_expanding_unrelated_parts() {
        let mut nodes = vec![
            json!({"id":"game","class_name":"DataModel","name":"game"}),
            json!({"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game"}),
            json!({"id":"p","class_name":"Part","name":"Part","parent":"ws","properties":{"Anchored":false}}),
            json!({"id":"q","class_name":"Part","name":"Anchor","parent":"ws","properties":{"Anchored":true}}),
            json!({"id":"j","class_name":"WeldConstraint","name":"Joint","parent":"ws","properties":{"Enabled":true,"Part0":{"$ref":"p"},"Part1":{"$ref":"q"}}}),
        ];
        for i in 0..1000 {
            nodes.push(json!({"id":format!("untouched{i}"),"class_name":"Part","name":"Part","parent":"ws"}));
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
            world::World::from_snapshot(&json!({"version":1,"sequence":0,"nodes":[]}), Some(&view))
                .unwrap(),
        ));
        let lua = Lua::new();
        assert!(matches!(
            grounded(&lua, &ctx, "p", MultiValue::new())
                .unwrap()
                .front(),
            Some(Value::Boolean(true))
        ));
        world::set_engine_property(&ctx, "q", "Anchored", json!(false), None).unwrap();
        assert!(matches!(
            grounded(&lua, &ctx, "p", MultiValue::new())
                .unwrap()
                .front(),
            Some(Value::Boolean(false))
        ));
        let w = ctx.borrow();
        for i in 0..1000 {
            assert!(w.nodes.peek(&format!("untouched{i}")).is_none());
        }
        assert!(w.effects.get("mutations").is_none());
    }
}
