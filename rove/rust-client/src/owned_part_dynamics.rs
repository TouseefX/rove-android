//! Bounded isolated-box dynamics, ported from scripts/owned_part_dynamics.py.
//! Inelastic static contacts and one capped BodyVelocity; no assembly solver.
use super::owned_part_poses::PartFrame;
use rove_client_core::physics::{sweep_aabb_oriented_records, OrientedBox, OrientedSurface};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Servo {
    pub target: [f64; 3],
    pub max_force: [f64; 3],
    pub gain: f64,
    pub mass: f64,
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    (0..3).map(|i| a[i] * b[i]).sum()
}
fn outward(mut v: [f64; 3], normals: &[[f64; 3]]) -> [f64; 3] {
    for _ in 0..4 {
        let mut changed = false;
        for n in normals {
            let d = dot(v, *n);
            if d < -1e-10 {
                for i in 0..3 {
                    v[i] -= d * n[i];
                }
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    v
}
pub(crate) fn half_size(size: [f64; 3], r: [f64; 9]) -> Result<[f64; 3], &'static str> {
    if size.iter().any(|v| !v.is_finite() || *v <= 0.)
        || !super::physics_sender::validate_rotation(r)
    {
        return Err("invalid_box");
    }
    for row in r.chunks_exact(3) {
        let mut nonzero = row.iter().filter(|v| v.abs() > 1e-8);
        if nonzero.next().is_none_or(|v| (v.abs() - 1.).abs() > 1e-8) || nonzero.next().is_some() {
            return Err("rotation_not_axis_aligned");
        }
    }
    Ok(std::array::from_fn(|i| {
        (0..3).map(|j| r[i * 3 + j].abs() * size[j] * 0.5).sum()
    }))
}

// The same stable minimum-axis tie handling as the Python model. All axes
// are normalized, so the correction budget is in world units.
fn correction(p: [f64; 3], h: [f64; 3], b: OrientedBox) -> Option<(f64, [f64; 3])> {
    let world = [[1., 0., 0.], [0., 1., 0.], [0., 0., 1.]];
    let axes: [[f64; 3]; 3] =
        std::array::from_fn(|j| std::array::from_fn(|i| b.rotation[i * 3 + j]));
    let mut candidates = Vec::with_capacity(15);
    candidates.extend(world);
    candidates.extend(axes);
    for a in world {
        for b in axes {
            let c = [
                a[1] * b[2] - a[2] * b[1],
                a[2] * b[0] - a[0] * b[2],
                a[0] * b[1] - a[1] * b[0],
            ];
            let length = dot(c, c).sqrt();
            if length > 1e-12 {
                candidates.push(c.map(|v| v / length));
            }
        }
    }
    let delta = std::array::from_fn(|i| p[i] - b.center[i]);
    let mut best: Option<(f64, [f64; 3])> = None;
    for axis in candidates {
        let d = dot(delta, axis);
        let ra = (0..3).map(|i| h[i] * axis[i].abs()).sum::<f64>();
        let rb = (0..3)
            .map(|i| b.half_size[i] * dot(axis, axes[i]).abs())
            .sum::<f64>();
        let depth = ra + rb - d.abs();
        if depth <= 1e-10 * (ra + rb).max(1.) {
            return None;
        }
        if best.is_none_or(|v| depth < v.0 - 1e-10 * v.0.max(1.)) {
            best = Some((depth, axis.map(|v| v * if d > 0. { 1. } else { -1. })));
        }
    }
    best
}

fn acceleration(
    v: [f64; 3],
    servo: Option<Servo>,
    dt: f64,
    gravity: f64,
    normals: &[[f64; 3]],
) -> [f64; 3] {
    let external = outward([0., -gravity, 0.], normals);
    let Some(s) = servo else {
        return external;
    };
    let a = std::array::from_fn(|i| {
        let free = v[i] + external[i] * dt;
        let ratio = s.gain * dt / s.mass;
        let required = s.gain / (1. + ratio) * (s.target[i] - free);
        let end = free + dt / s.mass * required.clamp(-s.max_force[i], s.max_force[i]);
        (end - v[i]) / dt
    });
    outward(a, normals)
}

pub(crate) fn step(
    frame: &PartFrame,
    size: [f64; 3],
    dt: f64,
    gravity: f64,
    servo: Option<Servo>,
    surfaces: &[OrientedSurface],
) -> Result<PartFrame, &'static str> {
    if !dt.is_finite() || dt <= 0. || dt > 0.25 || !gravity.is_finite() || gravity < 0. {
        return Err("invalid_simulation_interval");
    }
    if frame.angular.iter().any(|v| *v != 0.) {
        return Err("angular_dynamics_unsupported");
    }
    if let Some(s) = servo {
        if !s.mass.is_finite()
            || s.mass <= 0.
            || !s.gain.is_finite()
            || !(0.0..=1e12).contains(&s.gain)
            || s.max_force
                .iter()
                .any(|v| !v.is_finite() || *v < 0. || *v > 1e20)
            || s.target.iter().any(|v| !v.is_finite() || v.abs() >= 1e9)
        {
            return Err("invalid_velocity_servo");
        }
    }
    let half = half_size(size, frame.rotation)?;
    let mut result = frame.clone();
    let correction_limit = size.into_iter().fold(f64::INFINITY, f64::min).max(1.);
    let mut total_correction = 0.;
    let mut corrected = false;
    for _ in 0..4 {
        let next = surfaces
            .iter()
            .filter_map(|s| correction(result.position, half, s.bounds))
            .min_by(|a, b| a.0.total_cmp(&b.0));
        let Some((depth, normal)) = next else {
            corrected = true;
            break;
        };
        total_correction += depth;
        if total_correction > correction_limit + 1e-9 {
            return Err("overlap_correction_limit");
        }
        for i in 0..3 {
            result.position[i] += depth * normal[i];
        }
        result.linear = outward(result.linear, &[normal]);
    }
    if !corrected
        && surfaces
            .iter()
            .any(|s| correction(result.position, half, s.bounds).is_some())
    {
        return Err("overlap_correction_limit");
    }
    let mut total = dt;
    while total > 1e-12 {
        let mut left = total.min(1. / 240.);
        total -= left;
        let mut normals = Vec::new();
        for index in 0..=4 {
            let accel = acceleration(result.linear, servo, left, gravity, &normals);
            let end = std::array::from_fn(|i| result.linear[i] + accel[i] * left);
            let delta = std::array::from_fn(|i| (result.linear[i] + end[i]) * 0.5 * left);
            if dot(delta, delta) <= 1e-28 {
                for i in 0..3 {
                    result.position[i] += delta[i];
                }
                result.linear = end;
                break;
            }
            let hit = sweep_aabb_oriented_records(result.position, half, delta, surfaces, "")
                .map_err(|_| "invalid_collision_geometry")?;
            let Some(hit) = hit else {
                for i in 0..3 {
                    result.position[i] += delta[i];
                }
                result.linear = end;
                break;
            };
            let n = hit.hit.normal;
            if dot(n, n) < 1e-20 {
                return Err("initial_interpenetration");
            }
            let elapsed = left * hit.hit.fraction.clamp(0., 1.);
            let plane = dot(
                std::array::from_fn(|i| result.position[i] + delta[i] * hit.hit.fraction),
                n,
            );
            let a = if elapsed > 0. {
                acceleration(result.linear, servo, elapsed, gravity, &normals)
            } else {
                accel
            };
            let velocity = std::array::from_fn(|i| result.linear[i] + a[i] * elapsed);
            for i in 0..3 {
                result.position[i] += (result.linear[i] + velocity[i]) * 0.5 * elapsed;
            }
            let error = plane - dot(result.position, n);
            for i in 0..3 {
                result.position[i] += n[i] * error;
            }
            result.linear = outward(velocity, &[n]);
            let fresh = !normals.iter().any(|old| dot(*old, n) > 1. - 1e-8);
            if fresh {
                normals.push(n);
            }
            left -= elapsed;
            if left <= 1e-12 {
                break;
            }
            if index == 4 || (elapsed <= 1e-12 && !fresh) {
                return Err("contact_progress_unresolved");
            }
        }
    }
    if result
        .position
        .iter()
        .chain(result.linear.iter())
        .any(|v| !v.is_finite() || v.abs() >= 1e9)
    {
        return Err("simulated_state_unbounded");
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    #[test]
    fn owned_dynamics_matches_independent_python_steps() {
        let cases: Value =
            serde_json::from_str(include_str!("../tests/fixtures/owned-part-dynamics.json"))
                .unwrap();
        let vec = |v: &Value| std::array::from_fn(|i| v[i].as_f64().unwrap());
        for case in cases["cases"].as_array().unwrap() {
            let frame = PartFrame {
                id: 1,
                position: vec(&case["position"]),
                rotation: rove_client_core::physics::IDENTITY_ROTATION,
                linear: vec(&case["velocity"]),
                angular: [0.; 3],
            };
            let surfaces = case["boxes"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(i, b)| OrientedSurface {
                    node_id: format!("1:{i}"),
                    bounds: OrientedBox::new(vec(&b[0]), vec(&b[1]), frame.rotation).unwrap(),
                })
                .collect::<Vec<_>>();
            let s = &case["servo"];
            let servo = (!s.is_null()).then(|| Servo {
                target: vec(&s["target_velocity"]),
                max_force: vec(&s["max_force"]),
                gain: s["gain"].as_f64().unwrap(),
                mass: case["mass"].as_f64().unwrap(),
            });
            let out = step(
                &frame,
                vec(&case["size"]),
                case["dt"].as_f64().unwrap(),
                case["gravity"].as_f64().unwrap(),
                servo,
                &surfaces,
            )
            .unwrap();
            for (key, got) in [
                ("expected_position", out.position),
                ("expected_velocity", out.linear),
            ] {
                for i in 0..3 {
                    assert!(
                        (got[i] - case[key][i].as_f64().unwrap()).abs() < 1e-7,
                        "{} {key}: {got:?}",
                        case["name"]
                    );
                }
            }
        }
    }
    #[test]
    fn owned_dynamics_rejects_unsupported_motion_atomically() {
        let mut frame = PartFrame {
            id: 1,
            position: [0.; 3],
            rotation: rove_client_core::physics::IDENTITY_ROTATION,
            linear: [0.; 3],
            angular: [1., 0., 0.],
        };
        assert_eq!(
            step(&frame, [2.; 3], 0.01, 10., None, &[]).unwrap_err(),
            "angular_dynamics_unsupported"
        );
        frame.angular = [0.; 3];
        assert!(step(&frame, [2.; 3], 0., 10., None, &[]).is_err());
        assert!(step(&frame, [2.; 3], 1., 10., None, &[]).is_err());
        let wall = OrientedSurface {
            node_id: "1:2".into(),
            bounds: OrientedBox::new([0.; 3], [10.; 3], frame.rotation).unwrap(),
        };
        assert_eq!(
            step(&frame, [2.; 3], 0.01, 10., None, &[wall]).unwrap_err(),
            "overlap_correction_limit"
        );
        assert_eq!(frame.position, [0.; 3]);
    }
}
