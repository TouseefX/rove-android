//! Submit only received, schema-typed client-to-server signal events.
use super::{remotes, signals, world::WorldRef};
use mlua::{Lua, MultiValue, Value};
use serde_json::{json, Number, Value as Json};
use std::{collections::HashSet, rc::Rc};

const MAX_ROWS: usize = 64;
const MAX_ROW_BYTES: usize = 128 * 1024;
const MAX_QUEUE_BYTES: usize = 256 * 1024;

fn error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}

fn signature(ctx: &WorldRef, class: &str, event: &str) -> mlua::Result<Vec<Json>> {
    let world = ctx.borrow();
    let mut current = class.to_owned();
    let mut seen = HashSet::new();
    while seen.insert(current.clone()) {
        if let Some(types) = world.snapshot["class_event_signatures"][&current][event].as_array() {
            if types.len() > 256 || types.iter().any(|v| !v.as_u64().is_some_and(|n| n <= 255)) {
                return Err(error("invalid received signal signature"));
            }
            return Ok(types.clone());
        }
        let Some(parent) = world.snapshot["class_parents"][&current].as_str() else {
            break;
        };
        current = parent.to_owned();
    }
    Err(error("signal has no received replication signature"))
}

fn valid(value: &Json, kind: u64) -> bool {
    if matches!(kind, 28 | 69) {
        return value.is_null() || value.get("$ref").is_some();
    }
    if matches!(kind, 0 | 3) {
        return value.is_null();
    }
    match kind {
        2 | 8 | 33 => value.is_string() || value.get("$bytes_hex").is_some(),
        7 => value.get("$enum").is_some(),
        9 => value.is_boolean(),
        // The general remote codec preserves Luau numeric values as JSON
        // doubles. A reflected integer may therefore arrive as 3.0; retain
        // the exact wire value while checking it matches the received type.
        10 => {
            value.as_i64().is_some_and(|n| i32::try_from(n).is_ok())
                || value.as_f64().is_some_and(|n| {
                    n.is_finite()
                        && n.fract() == 0.0
                        && n >= i32::MIN as f64
                        && n <= i32::MAX as f64
                })
        }
        11 => value
            .as_f64()
            .is_some_and(|n| n.is_finite() && n.abs() <= f32::MAX as f64),
        12 => value.as_f64().is_some_and(f64::is_finite),
        13 | 14 | 18 | 19 | 21 | 22 | 27 | 29 | 30 | 31 | 32 | 37 | 40 | 48 | 56 => {
            // These structured values are always emitted by the bounded remote
            // encoder; only accept known, complete tagged forms.
            let tag = match kind {
                13 => "$udim",
                14 => "$udim2",
                18 => "$brickcolor",
                19 => "$color3",
                21 => "$vector2",
                22 => "$vector3",
                27 => "$cframe",
                29 => "$remote_array",
                30 => "$remote_array",
                31 => "$remote_map",
                32 => "$remote_map",
                37 => "$number_range",
                40 => "$rect",
                48 => "$datetime",
                56 => "$buffer_hex",
                _ => return false,
            };
            value.get(tag).is_some()
        }
        _ => false,
    }
}

fn signal_class(ctx: &WorldRef, id: &str) -> mlua::Result<(String, bool)> {
    let world = ctx.borrow();
    let node = world
        .nodes
        .get(id)
        .filter(|node| !node.destroyed)
        .ok_or_else(|| error("live signal Instance required"))?;
    Ok((node.class_name.clone(), node.server_owned))
}

fn diagnostic_token(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ':' | '_' | '-' | '.'))
        .take(max_chars)
        .collect()
}

fn local_signal_details(lua: &Lua, ctx: &WorldRef, id: &str, class: &str) -> String {
    let node_id = diagnostic_token(id, 96);
    let class = diagnostic_token(class, 48);
    let origin = super::lifecycle::replication_origin_id(lua, id)
        .map(|origin_id| {
            let origin_token = diagnostic_token(&origin_id, 96);
            let world = ctx.borrow();
            match world.nodes.get(&origin_id) {
                Some(node) => format!(
                    "{origin_token}:{}:server_owned={}",
                    diagnostic_token(&node.class_name, 48),
                    node.server_owned
                ),
                None => origin_token,
            }
        })
        .unwrap_or_else(|| "none".into());
    format!(
        "replicatesignal requires a received server-owned signal Instance (class={class}, id={node_id}, server_owned=false, clone_origin={origin})"
    )
}

fn queue(ctx: &WorldRef, row: Json) -> mlua::Result<()> {
    let size = row.to_string().len();
    let mut world = ctx.borrow_mut();
    let events = world
        .effects
        .entry("remote_server_events")
        .or_insert_with(|| Json::Array(Vec::new()));
    let rows = events
        .as_array()
        .ok_or_else(|| error("signal replication queue is invalid"))?;
    let count = rows.len();
    let bytes = rows.iter().map(|row| row.to_string().len()).sum::<usize>();
    if count >= MAX_ROWS || size > MAX_ROW_BYTES || bytes.saturating_add(size) > MAX_QUEUE_BYTES {
        return Err(error("signal replication queue limit exceeded"));
    }
    world
        .effects
        .get_mut("remote_server_events")
        .unwrap()
        .as_array_mut()
        .unwrap()
        .push(row);
    Ok(())
}

pub(super) fn install(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    let weak = Rc::downgrade(ctx);
    lua.globals().set(
        "replicatesignal",
        lua.create_function(move |lua, mut args: MultiValue| {
            let ctx = weak
                .upgrade()
                .ok_or_else(|| error("signal world has been released"))?;
            let signal = args.pop_front().unwrap_or(Value::Nil);
            let (id, event) = signals::replicated_key(lua, &ctx, &signal)?;
            let (class, received) = signal_class(&ctx, &id)?;
            if !received {
                return Err(error(local_signal_details(lua, &ctx, &id, &class)));
            }
            if ctx.borrow().snapshot["persistent"] != true {
                return Err(error("persistent client session required"));
            }
            let types = signature(&ctx, &class, &event)?;
            if types.len() != args.len() {
                return Err(error(
                    "signal argument count does not match received signature",
                ));
            }
            // Match the pinned VM's native Int32 conversion, including its
            // fractional/overflow behavior, instead of Rust's saturating cast.
            let values: Vec<Value> = args.into_iter().collect();
            let mut serialized = MultiValue::new();
            for (value, kind) in values.into_iter().zip(types.iter()) {
                if kind.as_u64() == Some(10) {
                    if !matches!(&value, Value::Integer(_) | Value::Number(_))
                        || matches!(&value, Value::Number(n) if !n.is_finite())
                    {
                        return Err(error("signal Int32 argument must be a finite number"));
                    }
                    let integer: i32 = unsafe {
                        lua.exec_raw((value,), |state| {
                            let top = mlua::ffi::lua_gettop(state);
                            let integer =
                                mlua::ffi::lua_tointegerx_(state, top, std::ptr::null_mut());
                            mlua::ffi::lua_settop(state, top - 1);
                            mlua::ffi::lua_pushnumber(state, integer as f64);
                        })?
                    };
                    serialized.push_back(Value::Integer(integer as i64));
                } else {
                    serialized.push_back(value);
                }
            }
            let mut encoded = remotes::encode_args(lua, &ctx, &serialized)?;
            for (value, kind) in encoded.iter_mut().zip(types.iter()) {
                if kind.as_u64() == Some(10) {
                    // Luau numbers are encoded by the shared remote serializer
                    // as JSON doubles. The reflected wire format uses Int32,
                    // so canonicalize exact in-range values to JSON integers.
                    let integer = value
                        .as_i64()
                        .or_else(|| {
                            value
                                .as_f64()
                                .filter(|n| {
                                    n.is_finite()
                                        && n.fract() == 0.0
                                        && *n >= i32::MIN as f64
                                        && *n <= i32::MAX as f64
                                })
                                .map(|n| n as i64)
                        })
                        .filter(|n| i32::try_from(*n).is_ok())
                        .ok_or_else(|| error("signal Int32 argument must be an exact integer"))?;
                    *value = Json::Number(Number::from(integer));
                }
                if !valid(value, kind.as_u64().unwrap_or(u64::MAX)) {
                    return Err(error("signal argument does not match received signature"));
                }
            }
            queue(
                &ctx,
                json!({"node":id,"event":event,"argument_types":types,"arguments":encoded}),
            )
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rust_host::world::World;
    use serde_json::json;
    use std::{cell::RefCell, rc::Rc};

    fn setup(snapshot: Json) -> (Lua, WorldRef) {
        let ctx = Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()));
        let lua = Lua::new();
        super::super::install_bindings(&lua, &ctx).unwrap();
        (lua, ctx)
    }

    #[test]
    fn sends_only_exact_received_typed_events_and_coerces_integral_numbers() {
        let snapshot = json!({"version":1,"sequence":0,"persistent":true,
            "class_event_signatures":{"TextButton":{"MouseWheelForward":[10,10]}},
            "nodes":[{"id":"game","class_name":"DataModel","name":"game"},
                {"id":"button","class_name":"TextButton","name":"Button","parent":"game","server_owned":true,"properties":{}}]});
        let (lua, ctx) = setup(snapshot);
        lua.load("replicatesignal(game.Button.MouseWheelForward, 3.0, -2147483648)")
            .exec()
            .unwrap();
        assert_eq!(
            ctx.borrow().effects["remote_server_events"],
            json!([{
                "node":"button","event":"MouseWheelForward","argument_types":[10,10],"arguments":[3,-2147483648]
            }])
        );
        lua.load("replicatesignal(game.Button.MouseWheelForward,3.5,-2.9)")
            .exec()
            .unwrap();
        assert_eq!(
            ctx.borrow().effects["remote_server_events"][1]["arguments"],
            json!([3, -2])
        );
        lua.load("replicatesignal(game.Button.MouseWheelForward,3,2147483648)")
            .exec()
            .unwrap();
        assert!(
            ctx.borrow().effects["remote_server_events"][2]["arguments"][1]
                .as_i64()
                .is_some_and(|n| i32::try_from(n).is_ok())
        );
        assert!(!lua
            .load("return pcall(replicatesignal,game.Button.MouseWheelForward,0/0,2)")
            .eval::<bool>()
            .unwrap());
        assert!(
            lua.load("return pcall(replicatesignal,game.Button.MouseWheelForward,3)")
                .eval::<bool>()
                .unwrap()
                == false
        );
        assert!(!lua
            .load("local ok=pcall(replicatesignal,game.Button.MouseButton1Click); return ok")
            .eval::<bool>()
            .unwrap());
        assert_eq!(
            ctx.borrow().effects["remote_server_events"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn rejects_local_nodes_forged_handles_wrong_worlds_and_bad_references() {
        let snapshot = json!({"version":1,"sequence":0,"persistent":true,
            "class_event_signatures":{"TextButton":{"MouseWheelForward":[10]}},
            "nodes":[{"id":"game","class_name":"DataModel","name":"game"},
                {"id":"local","class_name":"TextButton","name":"Local","parent":"game","server_owned":false,"properties":{}}]});
        let (lua, ctx) = setup(snapshot);
        let (ok, message): (bool, String) = lua
            .load("local ok,message=pcall(replicatesignal,game.Local.MouseWheelForward,1); return ok,tostring(message)")
            .eval()
            .unwrap();
        assert!(!ok);
        assert!(message.contains("class=TextButton"), "{message}");
        assert!(message.contains("id=local"), "{message}");
        assert!(message.contains("server_owned=false"), "{message}");
        assert!(message.contains("clone_origin=none"), "{message}");
        assert!(
            lua.load("return pcall(replicatesignal,setmetatable({},{}),1)")
                .eval::<bool>()
                .unwrap()
                == false
        );
        assert!(ctx.borrow().effects.get("remote_server_events").is_none());
    }
}
