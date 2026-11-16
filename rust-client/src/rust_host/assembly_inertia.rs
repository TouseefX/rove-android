//! Current rigid-assembly inertia. No persistent tensor or duplicate world cache.
use super::*;
type Matrix = [[f64; 3]; 3];
const MAX_MEMBERS: usize = 4096;

fn outer_add(matrix: &mut Matrix, v: [f64; 3], weight: f64) {
    for i in 0..3 {
        for j in 0..3 {
            matrix[i][j] += weight * v[i] * v[j];
        }
    }
}

// Covariance of a uniform primitive about its geometric center. The tensor is
// m * (trace(covariance) * identity - covariance).
fn primitive(size: [f64; 3], shape: i64) -> mlua::Result<([f64; 3], Matrix)> {
    if size.iter().any(|v| !v.is_finite() || *v <= 0.) {
        return Err(err("assembly inertia requires positive finite Size"));
    }
    let [x, y, z] = size;
    let mut c = [[0.; 3]; 3];
    let mut center = [0.; 3];
    match shape {
        0 => {
            let r = x.min(y).min(z) / 2.;
            for (i, row) in c.iter_mut().enumerate() {
                row[i] = r * r / 5.;
            }
        }
        1 => {
            c[0][0] = x * x / 12.;
            c[1][1] = y * y / 12.;
            c[2][2] = z * z / 12.;
        }
        2 => {
            let r = y.min(z) / 2.;
            c[0][0] = x * x / 12.;
            c[1][1] = r * r / 4.;
            c[2][2] = r * r / 4.;
        }
        3 => {
            center = [0., -y / 6., z / 6.];
            c[0][0] = x * x / 12.;
            c[1][1] = y * y / 18.;
            c[2][2] = z * z / 18.;
            c[1][2] = y * z / 36.;
            c[2][1] = c[1][2];
        }
        4 => {
            // A corner wedge is a square-based pyramid. Split the base on its
            // diagonal; the resulting tetrahedra have equal volume.
            center = [x / 8., -y / 4., -z / 8.];
            let vertices = [
                [-x / 2., -y / 2., -z / 2.],
                [x / 2., -y / 2., -z / 2.],
                [x / 2., -y / 2., z / 2.],
                [-x / 2., -y / 2., z / 2.],
                [x / 2., y / 2., -z / 2.],
            ];
            for indices in [[0, 1, 2, 4], [0, 2, 3, 4]] {
                let mut centroid = [0.; 3];
                for index in indices {
                    centroid = add3(centroid, scale3(vertices[index], 0.25));
                }
                for index in indices {
                    outer_add(&mut c, sub3(vertices[index], centroid), 1. / 40.);
                }
                outer_add(&mut c, sub3(centroid, center), 0.5);
            }
        }
        _ => return Err(err("assembly inertia geometry is unavailable")),
    }
    Ok((center, c))
}

fn rotate(c: Matrix, r: [f64; 9]) -> Matrix {
    std::array::from_fn(|i| {
        std::array::from_fn(|j| {
            (0..3)
                .flat_map(|a| (0..3).map(move |b| r[3 * i + a] * c[a][b] * r[3 * j + b]))
                .sum()
        })
    })
}
fn transform(r: [f64; 9], v: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|i| (0..3).map(|j| r[3 * i + j] * v[j]).sum())
}
fn boolean(ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<bool> {
    world::get_property(ctx, id, key)?
        .as_bool()
        .ok_or_else(|| err(format!("assembly {key} is unavailable")))
}

fn tensor(ctx: &WorldRef, id: &str) -> mlua::Result<Option<Matrix>> {
    if !ctx.borrow().nodes.is_live(id) || !world::is_a(ctx, id, "BasePart") {
        return Err(err("angular conversion requires a live BasePart"));
    }
    let Some(container) = super::super::joint_graph::container(ctx, id, false)? else {
        return Ok(None);
    };
    if !world::is_a(ctx, &container, "Workspace") {
        return Ok(None);
    }
    let members = mass_assembly_members(ctx, id)?;
    if members.len() >= MAX_MEMBERS {
        return Err(err("assembly inertia member limit exceeded"));
    }
    for part in members
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(id))
    {
        if boolean(ctx, part, "Anchored")? {
            return Ok(None);
        }
    }
    let root = mass_assembly_root(ctx, &members, id)?;
    let (mut total, mut center, mut covariance) = (0., [0.; 3], [[0.; 3]; 3]);
    for part in members
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(id))
    {
        if part != root && boolean(ctx, part, "Massless")? {
            continue;
        }
        let size = part_mass_properties::size(ctx, part)?;
        let shape = part_mass_properties::shape(ctx, part)?;
        let (local, c) = primitive(size, shape)?;
        let mass = part_mass_properties::read(ctx, part)?;
        if mass == 0. {
            continue;
        }
        let frame = read_frame(&world::get_property(ctx, part, "CFrame")?)?;
        if frame
            .position
            .iter()
            .chain(frame.rotation.iter())
            .any(|v| !v.is_finite())
        {
            return Err(err("assembly inertia requires a finite CFrame"));
        }
        let position = add3(frame.position, transform(frame.rotation, local));
        let world_c = rotate(c, frame.rotation);
        let next = total + mass;
        let delta = sub3(position, center);
        // Online parallel-axis accumulation avoids storing every member tensor
        // and avoids subtracting large world-origin second moments.
        outer_add(&mut covariance, delta, mass * (total / next));
        for i in 0..3 {
            for j in 0..3 {
                covariance[i][j] += mass * world_c[i][j];
            }
        }
        center = add3(center, scale3(delta, mass / next));
        total = next;
    }
    if !total.is_finite() || total <= 0. {
        return Err(err("assembly inertia requires positive finite mass"));
    }
    let tensor: Matrix = std::array::from_fn(|i| {
        std::array::from_fn(|j| {
            if i == j {
                (0..3).filter(|k| *k != i).map(|k| covariance[k][k]).sum()
            } else {
                -covariance[i][j]
            }
        })
    });
    if tensor.iter().flatten().any(|v| !v.is_finite()) {
        return Err(err("assembly inertia exceeds finite range"));
    }
    Ok(Some(tensor))
}

fn multiply(m: Matrix, v: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|i| dot3(m[i], v))
}
fn solve(m: Matrix, rhs: [f64; 3]) -> mlua::Result<[f64; 3]> {
    let scale = m.iter().flatten().fold(0f64, |a, v| a.max(v.abs()));
    if scale == 0. {
        return Err(err("assembly inertia is singular"));
    }
    let mut l = [[0.; 3]; 3];
    for i in 0..3 {
        for j in 0..=i {
            let v = m[i][j] / scale - (0..j).map(|k| l[i][k] * l[j][k]).sum::<f64>();
            l[i][j] = if i == j {
                if v <= 0. || !v.is_finite() {
                    return Err(err("assembly inertia is not positive definite"));
                }
                v.sqrt()
            } else {
                v / l[j][j]
            };
        }
    }
    let mut y = [0.; 3];
    for i in 0..3 {
        y[i] = (rhs[i] / scale - (0..i).map(|j| l[i][j] * y[j]).sum::<f64>()) / l[i][i];
    }
    let mut x = [0.; 3];
    for i in (0..3).rev() {
        x[i] = (y[i] - (i + 1..3).map(|j| l[j][i] * x[j]).sum::<f64>()) / l[i][i];
    }
    Ok(x)
}

fn conversion(
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
    inverse: bool,
) -> mlua::Result<MultiValue> {
    if args.is_empty() || args.len() > 2 {
        return Err(err(
            "angular conversion expects a Vector3 and optional angular velocity",
        ));
    }
    let input = read_vec(&args[0])?;
    let omega = match args.get(1) {
        None | Some(Value::Nil) => [0.; 3],
        Some(v) => read_vec(v)?,
    };
    let Some(i) = tensor(ctx, id)? else {
        let n = if inverse { 0. } else { f32::INFINITY };
        return Ok(MultiValue::from_vec(vec![Value::Vector(Vector::new(
            n, n, n,
        ))]));
    };
    let momentum = multiply(i, omega);
    let gyro = [
        omega[1] * momentum[2] - omega[2] * momentum[1],
        omega[2] * momentum[0] - omega[0] * momentum[2],
        omega[0] * momentum[1] - omega[1] * momentum[0],
    ];
    let result = if inverse {
        solve(i, sub3(input, gyro))?
    } else {
        add3(multiply(i, input), gyro)
    };
    if result
        .iter()
        .any(|v| !v.is_finite() || v.abs() > f32::MAX as f64)
    {
        return Err(err("angular conversion exceeds finite Vector3 range"));
    }
    Ok(MultiValue::from_vec(vec![Value::Vector(Vector::new(
        result[0] as f32,
        result[1] as f32,
        result[2] as f32,
    ))]))
}
pub(super) fn install(ctx: &WorldRef) {
    world::register_method(
        ctx,
        "BasePart",
        "AngularAccelerationToTorque",
        |_, ctx, id, args| conversion(ctx, id, args, false),
    );
    world::register_method(
        ctx,
        "BasePart",
        "TorqueToAngularAcceleration",
        |_, ctx, id, args| conversion(ctx, id, args, true),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine_world_projection::ProjectionContext, engine_world_view::SharedWorldView};
    use rove_client_core::{schema::Schema, world::WorldStore};
    use std::collections::HashMap;
    #[test]
    fn inertia_query_keeps_unrelated_shared_parts_lazy() {
        let mut nodes = vec![
            json!({"id":"game","class_name":"DataModel","name":"game"}),
            json!({"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game"}),
            json!({"id":"p","class_name":"Part","name":"Part","parent":"ws","properties":{
                "Size":{"$vector3":[2,2,2]},"Shape":{"$enum":{"type":"PartType","value":1}},"Anchored":false,"Mass":8,"CFrame":{"$cframe":{"position":[0,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}}
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
        let result = tensor(&ctx, "p").unwrap().unwrap();
        assert!((result[0][0] - 16. / 3.).abs() < 1e-10);
        let world = ctx.borrow();
        for i in 0..1000 {
            assert!(world.nodes.peek(&format!("untouched{i}")).is_none());
        }
        assert!(world.effects.get("mutations").is_none());
    }

    #[test]
    fn inertia_member_bound_rejects_large_assemblies_before_mass_queries() {
        let mut nodes = vec![
            json!({"id":"game","class_name":"DataModel"}),
            json!({"id":"ws","class_name":"Workspace","parent":"game"}),
        ];
        for i in 0..=MAX_MEMBERS {
            nodes.push(json!({"id":format!("p{i}"),"class_name":"Part","parent":"ws","properties":{"Anchored":false}}));
            if i > 0 {
                nodes.push(json!({"id":format!("j{i}"),"class_name":"WeldConstraint","parent":"ws","properties":{
                    "Enabled":true,"Part0":{"$ref":format!("p{}",i-1)},"Part1":{"$ref":format!("p{i}")}}}));
            }
        }
        let ctx = Rc::new(RefCell::new(
            world::World::from_snapshot(&json!({"version":1,"sequence":0,"nodes":nodes}), None)
                .unwrap(),
        ));
        assert!(tensor(&ctx, "p0")
            .unwrap_err()
            .to_string()
            .contains("member limit"));
        assert!(ctx.borrow().effects.get("mutations").is_none());
    }
}
