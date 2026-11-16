//! World-space point velocity: linear velocity plus angular velocity cross offset.
use super::*;

pub(super) fn motion_vector(value: &Json, label: &str) -> mlua::Result<[f64; 3]> {
    let raw = value
        .get("$vector3")
        .and_then(Json::as_array)
        .filter(|v| v.len() == 3)
        .ok_or_else(|| err(format!("{label} is unavailable")))?;
    let mut out = [0.; 3];
    for (index, component) in raw.iter().enumerate() {
        out[index] = component
            .as_f64()
            .filter(|v| v.is_finite() && v.abs() <= f32::MAX as f64)
            .ok_or_else(|| err(format!("{label} must be a finite Vector3")))?;
    }
    Ok(out)
}

fn at_position(_lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    if !ctx.borrow().nodes.is_live(id) || !world::is_a(ctx, id, "BasePart") {
        return Err(err("GetVelocityAtPosition requires a live BasePart"));
    }
    let Some(Value::Vector(position)) = args.front() else {
        return Err(err("GetVelocityAtPosition requires a finite Vector3"));
    };
    let position = [
        position.x() as f64,
        position.y() as f64,
        position.z() as f64,
    ];
    if position.iter().any(|v| !v.is_finite()) {
        return Err(err("GetVelocityAtPosition requires a finite Vector3"));
    }
    // Received members carry their assembly observations. Locally created rigid
    // members use the same root selection as the existing assembly mass APIs.
    let received = ctx
        .borrow()
        .nodes
        .get(id)
        .is_some_and(|node| node.server_owned);
    let source = if received {
        id.to_owned()
    } else {
        mass_assembly_root(ctx, &mass_assembly_members(ctx, id)?, id)?
    };
    let mut linear = motion_vector(
        &world::get_property(ctx, &source, "AssemblyLinearVelocity")?,
        "AssemblyLinearVelocity",
    )?;
    let angular = motion_vector(
        &world::get_property(ctx, &source, "AssemblyAngularVelocity")?,
        "AssemblyAngularVelocity",
    )?;
    if angular.iter().any(|v| *v != 0.) {
        let center = motion_vector(
            &mass_assembly_value(ctx, &source, "AssemblyCenterOfMass")?,
            "AssemblyCenterOfMass",
        )?;
        let offset = [
            position[0] - center[0],
            position[1] - center[1],
            position[2] - center[2],
        ];
        let spin = [
            angular[1] * offset[2] - angular[2] * offset[1],
            angular[2] * offset[0] - angular[0] * offset[2],
            angular[0] * offset[1] - angular[1] * offset[0],
        ];
        for axis in 0..3 {
            linear[axis] += spin[axis];
        }
    }
    if linear
        .iter()
        .any(|v| !v.is_finite() || v.abs() > f32::MAX as f64)
    {
        return Err(err("point velocity exceeds finite Vector3 range"));
    }
    Ok(MultiValue::from_vec(vec![Value::Vector(Vector::new(
        linear[0] as f32,
        linear[1] as f32,
        linear[2] as f32,
    ))]))
}

pub(super) fn install(ctx: &WorldRef) {
    world::register_method(ctx, "BasePart", "GetVelocityAtPosition", at_position);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine_world_projection::ProjectionContext, engine_world_view::SharedWorldView};
    use rove_client_core::{schema::Schema, world::WorldStore};
    use std::collections::HashMap;
    #[test]
    fn received_point_query_keeps_unrelated_shared_parts_lazy() {
        let mut nodes = vec![
            json!({"id":"game","class_name":"DataModel","name":"game"}),
            json!({"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game"}),
            json!({"id":"p","class_name":"Part","name":"Part","parent":"ws","properties":{
                "AssemblyLinearVelocity":{"$vector3":[1,2,3]},"AssemblyAngularVelocity":{"$vector3":[0,0,2]},"AssemblyCenterOfMass":{"$vector3":[0,0,0]}
            }}),
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
        let result = at_position(
            &Lua::new(),
            &ctx,
            "p",
            MultiValue::from_vec(vec![Value::Vector(Vector::new(2., 0., 0.))]),
        )
        .unwrap();
        let Value::Vector(v) = result.front().unwrap() else {
            panic!("expected Vector3")
        };
        assert_eq!([v.x(), v.y(), v.z()], [1., 6., 3.]);
        let world = ctx.borrow();
        for i in 0..1000 {
            assert!(world.nodes.peek(&format!("untouched{i}")).is_none());
        }
        assert!(world.effects.get("mutations").is_none());
    }
}
