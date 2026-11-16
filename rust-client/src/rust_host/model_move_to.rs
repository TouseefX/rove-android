//! Model placement, separate from Humanoid walking and pivot-offset placement.
use super::*;

const MAX_PAIRS: usize = 1_000_000;

fn containing_root(ctx: &WorldRef, id: &str) -> mlua::Result<Option<String>> {
    let mut current = id.to_owned();
    let mut seen = HashSet::new();
    loop {
        if world::is_a(ctx, &current, "Workspace") || world::is_a(ctx, &current, "WorldModel") {
            return Ok(Some(current));
        }
        if !seen.insert(current.clone()) || seen.len() > MAX_NODES {
            return Err(err("MoveTo found an invalid hierarchy"));
        }
        let parent = ctx
            .borrow()
            .nodes
            .get(&current)
            .and_then(|node| node.parent.clone());
        let Some(parent) = parent else {
            return Ok(None);
        };
        current = parent;
    }
}

struct BoxPose {
    frame: Frame,
    half: [f64; 3],
}

fn box_pose(ctx: &WorldRef, id: &str, frame: Frame) -> mlua::Result<BoxPose> {
    let size = world::get_property(ctx, id, "Size")?;
    let size = size["$vector3"]
        .as_array()
        .filter(|size| size.len() == 3)
        .ok_or_else(|| err("MoveTo requires a known part Size"))?;
    let mut half = [0.; 3];
    for (out, value) in half.iter_mut().zip(size) {
        let size = value
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0. && *n <= f32::MAX as f64)
            .ok_or_else(|| err("MoveTo requires a finite, non-negative part Size"))?;
        *out = size * 0.5;
    }
    Ok(BoxPose { frame, half })
}

// Intersect all 15 separating-axis constraints to get the open interval of
// upward translations that would intersect two oriented part bounds. Touching
// faces are allowed, so a model on a floor does not move on every call.
fn blocked_heights(a: &BoxPose, b: &BoxPose) -> Option<(f64, f64)> {
    let axes =
        |f: Frame| -> [[f64; 3]; 3] { std::array::from_fn(|i| [f.r[i], f.r[3 + i], f.r[6 + i]]) };
    let aa = axes(a.frame);
    let bb = axes(b.frame);
    let cross = |a: [f64; 3], b: [f64; 3]| {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    };
    let dot = |a: [f64; 3], b: [f64; 3]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>();
    let delta = std::array::from_fn(|i| a.frame.p[i] - b.frame.p[i]);
    let mut low = f64::NEG_INFINITY;
    let mut high = f64::INFINITY;
    for axis in aa
        .into_iter()
        .chain(bb)
        .chain(aa.into_iter().flat_map(|x| bb.map(|y| cross(x, y))))
    {
        if dot(axis, axis) < 1e-20 {
            continue;
        }
        let radius = (0..3)
            .map(|i| a.half[i] * dot(axis, aa[i]).abs() + b.half[i] * dot(axis, bb[i]).abs())
            .sum::<f64>();
        let distance = dot(axis, delta);
        if axis[1].abs() < 1e-12 {
            if distance.abs() >= radius - 1e-9 {
                return None;
            }
        } else {
            let first = (-radius - distance) / axis[1];
            let last = (radius - distance) / axis[1];
            low = low.max(first.min(last));
            high = high.min(first.max(last));
            if high <= low || high <= 0. {
                return None;
            }
        }
    }
    (high.is_finite() && high > 0.).then_some((low, high))
}

fn lift(ctx: &WorldRef, id: &str, ids: &[String], next: &[Record]) -> mlua::Result<f64> {
    // Only the containing WorldRoot participates; parts in storage and other
    // WorldModels must not obstruct this model.
    let Some(root) = containing_root(ctx, id)? else {
        return Ok(0.);
    };
    let moving = next
        .iter()
        .filter(|record| record.kind == Kind::Part)
        .map(|record| box_pose(ctx, &record.id, record.frame))
        .collect::<mlua::Result<Vec<_>>>()?;
    let excluded: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let mut intervals = Vec::new();
    let mut pairs = 0usize;
    for candidate in descendants(ctx, &root)? {
        if excluded.contains(candidate.as_str())
            || !world::is_a(ctx, &candidate, "BasePart")
            || world::is_a(ctx, &candidate, "Terrain")
            || world::get_property(ctx, &candidate, "CanCollide")
                .ok()
                .and_then(|v| v.as_bool())
                == Some(false)
        {
            continue;
        }
        if containing_root(ctx, &candidate)?.as_deref() != Some(root.as_str()) {
            continue;
        }
        // Only received geometry can obstruct placement. Meshes use their part
        // bounds; voxel terrain and exact mesh interiors are not handled here.
        let Ok(obstacle) =
            frame(ctx, &candidate, "CFrame").and_then(|f| box_pose(ctx, &candidate, f))
        else {
            continue;
        };
        for part in &moving {
            pairs += 1;
            if pairs > MAX_PAIRS {
                return Err(err("MoveTo collision query budget exceeded"));
            }
            if let Some(interval) = blocked_heights(part, &obstacle) {
                intervals.push(interval);
            }
        }
    }
    intervals.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let mut height = 0.;
    for (low, high) in intervals {
        if low >= height - 1e-8 {
            break;
        }
        if high > height {
            height = high;
        }
    }
    Ok(height)
}

fn move_to(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    if args.len() != 1 || !model(ctx, id) {
        return Err(err("Model:MoveTo expects one Vector3"));
    }
    let position = vector(&args[0], "Model:MoveTo")?;
    let ids = descendants(ctx, id)?;
    let primary = primary(ctx, id)?;
    if primary.as_ref().is_some_and(|part| !ids.contains(part)) {
        return Err(err("MoveTo requires PrimaryPart inside the model"));
    }
    let root = primary.or_else(|| {
        ids.iter()
            .find(|part| world::is_a(ctx, part, "BasePart"))
            .cloned()
    });
    let Some(root) = root else {
        return Ok(MultiValue::new());
    };
    // MoveTo targets the part origin, not its PivotOffset, and resets the root
    // orientation. Every other part keeps its transform relative to that root.
    let target = Frame {
        p: position,
        ..Frame::identity()
    };
    let delta = target.mul(frame(ctx, &root, "CFrame")?.inv())?;
    let mut next = records(ctx, &ids, false)?;
    for record in &mut next {
        record.frame = if record.id == root {
            target
        } else {
            delta.mul(record.frame)?
        };
    }
    let height = lift(ctx, id, &ids, &next)?;
    for record in &mut next {
        record.frame.p[1] += height;
        record.frame = Frame::from_cf(&record.frame.value())?;
    }
    // Prepare before committing any pose, including queue capacity validation.
    let intent = character_pivot_intents::prepare(lua, ctx, id, &next)?;
    let changes = world::apply_pose_batch(ctx, &writes(&next), false)?;
    if let Some(intent) = intent {
        queue_intent(lua, ctx, intent)?;
    }
    world::pose_events(lua, ctx, changes)?;
    Ok(MultiValue::new())
}

pub(super) fn install(ctx: &WorldRef) {
    for name in ["MoveTo", "moveTo", "move"] {
        world::register_method(ctx, "Model", name, move_to);
    }
}
