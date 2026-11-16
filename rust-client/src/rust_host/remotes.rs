//! Local RemoteEvent and RemoteFunction bridge for the Rust-owned Luau host.
//!
//! This module only records work for the existing session transport. It never
//! sends a packet itself and only accepts received, server-owned Instances.

use super::{
    scheduler::{self, SchedulerRef, TaskOutcome},
    signals, values,
    world::{self, WorldRef},
};
use mlua::{Function, Lua, MultiValue, Table, Value};
use rove_client_core::runtime_delta::normalize_remote_variant;
use serde_json::{json, Value as Json};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

const MAX_ARGUMENTS: usize = 256;
const MAX_QUEUE_ROWS: usize = 64;
const MAX_QUEUE_BYTES: usize = 256 * 1024;
const MAX_ROW_BYTES: usize = 128 * 1024;
const MAX_ARGUMENT_BYTES: usize = 65_536;
const MAX_ITEMS: usize = 10_000;
const MAX_DEPTH: usize = 16;
/// The host actor IO router reserves a separate JSON-safe token lane for Actor
/// VM calls, keeping root VM IDs disjoint across the session.
const ACTOR_REQUEST_ID_BASE: u64 = 1u64 << 52;

pub type RemoteStateRef = Rc<RefCell<RemoteState>>;

#[derive(Default)]
pub struct RemoteState {
    next_request: u64,
    pending: HashMap<u64, PendingCall>,
    callbacks: HashMap<String, BoundCallback>,
    callback_runner: Option<Function>,
    invocations: Vec<IncomingCall>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeltaEventReport {
    pub received: usize,
    pub queued_invocations: usize,
    pub unresolved: usize,
    pub rejected_payloads: usize,
}

#[derive(Debug)]
enum EventDeliveryError {
    Runtime(mlua::Error),
    Payload {
        index: usize,
        failure: RemoteJsonFailure,
    },
}

#[derive(Debug)]
struct RemoteJsonFailure {
    path: String,
    shape: String,
    reason: String,
}

fn top_level_json_shape(value: &Json) -> String {
    match value {
        Json::Null => "null".to_owned(),
        Json::Bool(_) => "boolean".to_owned(),
        Json::Number(_) => "number".to_owned(),
        Json::String(_) => "string".to_owned(),
        Json::Array(_) => "array".to_owned(),
        Json::Object(map) if map.len() == 1 => {
            let tag = map.keys().next().map(String::as_str).unwrap_or("object");
            let clean = tag
                .chars()
                .take(64)
                .map(|ch| if ch.is_ascii_alphanumeric() || matches!(ch, '$' | '_' | '-') { ch } else { '?' })
                .collect::<String>();
            format!("tag {clean}")
        }
        Json::Object(_) => "object".to_owned(),
    }
}

fn remote_argument_diagnostic(
    direction: &str,
    method: &str,
    index: usize,
    failure: &RemoteJsonFailure,
) -> String {
    format!(
        "{direction} {method} rejected argument #{index}: {} at {} (type {})",
        failure.reason, failure.path, failure.shape
    )
}

fn record_remote_diagnostic(lua: &Lua, text: &str) {
    // This writes directly to the bounded host console queue. It deliberately
    // avoids calling the replaceable global `warn` or firing MessageOut while
    // processing authenticated transport input.
    let _ = super::source_helpers::record_host_warning(lua, text);
}

fn validate_remote_arguments(
    ctx: &WorldRef,
    values: &[Json],
) -> Result<(), (usize, RemoteJsonFailure)> {
    if values.len() > MAX_ARGUMENTS {
        return Err((
            MAX_ARGUMENTS + 1,
            RemoteJsonFailure {
                path: "$".to_owned(),
                shape: "argument-count limit".to_owned(),
                reason: "remote argument limit exceeded".to_owned(),
            },
        ));
    }
    let mut remaining = MAX_ITEMS;
    let mut bytes = 0usize;
    for (index, value) in values.iter().enumerate() {
        if let Err(failure) = validate_remote_json_detailed(value, 0, &mut remaining, "$".to_owned()) {
            return Err((index + 1, failure));
        }
        if validate_received_refs(ctx, value).is_err() {
            return Err((index + 1, RemoteJsonFailure {
                path: "$".to_owned(),
                shape: top_level_json_shape(value),
                reason: "invalid or unavailable Remote Instance reference".to_owned(),
            }));
        }
        bytes = bytes.saturating_add(value.to_string().len());
        if bytes > MAX_ARGUMENT_BYTES {
            return Err((index + 1, RemoteJsonFailure {
                path: "$".to_owned(),
                shape: top_level_json_shape(value),
                reason: "remote argument byte limit exceeded".to_owned(),
            }));
        }
    }
    Ok(())
}

fn remote_variant_budget(
    value: &Json,
    depth: usize,
    items: &mut usize,
    path: String,
) -> Result<(), RemoteJsonFailure> {
    if depth > MAX_DEPTH {
        return Err(remote_json_failure(value, path, "remote value nesting exceeds limit"));
    }
    if *items == 0 {
        return Err(remote_json_failure(value, path, "remote value item limit exceeded"));
    }
    *items -= 1;
    match value {
        Json::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                remote_variant_budget(
                    child,
                    depth + 1,
                    items,
                    child_remote_path(&path, &format!("[{}]", index + 1)),
                )?;
            }
        }
        Json::Object(values) => {
            for (key, child) in values {
                remote_variant_budget(
                    child,
                    depth + 1,
                    items,
                    child_remote_path(&path, &format!(".{key}")),
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Normalize raw typed values at the last boundary before they enter a
/// RemoteEvent callback. The shared receipt normalizer handles the usual path;
/// this bounded, idempotent step also covers events delivered through an
/// alternate receipt path. Values already normalized take the cheap borrow
/// path, and unsupported type IDs remain explicit unsupported values.
fn normalize_received_remote_arguments(
    values: &[Json],
) -> Result<Option<Vec<Json>>, (usize, RemoteJsonFailure)> {
    let Some((index, _)) = values
        .iter()
        .enumerate()
        .find(|(_, value)| value.get("$remote_variant").is_some())
    else {
        return Ok(None);
    };
    if values.len() > MAX_ARGUMENTS {
        return Err((
            MAX_ARGUMENTS + 1,
            RemoteJsonFailure {
                path: "$".to_owned(),
                shape: "argument-count limit".to_owned(),
                reason: "remote argument limit exceeded".to_owned(),
            },
        ));
    }
    let encoded_bytes = values.iter().fold(0usize, |total, value| {
        total.saturating_add(value.to_string().len())
    });
    if encoded_bytes > MAX_ARGUMENT_BYTES {
        return Err((
            index + 1,
            RemoteJsonFailure {
                path: "$".to_owned(),
                shape: top_level_json_shape(&values[index]),
                reason: "remote argument byte limit exceeded".to_owned(),
            },
        ));
    }
    let mut remaining = MAX_ITEMS;
    for (index, value) in values.iter().enumerate() {
        if let Err(failure) = remote_variant_budget(value, 0, &mut remaining, "$".to_owned()) {
            return Err((index + 1, failure));
        }
    }
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            normalize_remote_variant(value, 0).map_err(|reason| {
                (
                    index + 1,
                    RemoteJsonFailure {
                        path: "$".to_owned(),
                        shape: top_level_json_shape(value),
                        reason: format!("invalid typed remote value: {reason}"),
                    },
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

struct PendingCall {
    node: String,
    thread: mlua::Thread,
}
#[derive(Clone)]
struct BoundCallback {
    function: Function,
    policy: scheduler::TaskPolicy,
}

struct IncomingCall {
    node: String,
    request_id: i32,
    arguments: Json,
    thread: Option<mlua::Thread>,
    callback: Option<BoundCallback>,
    error: Option<String>,
}

fn app_state(lua: &Lua) -> mlua::Result<RemoteStateRef> {
    lua.app_data_ref::<RemoteStateRef>()
        .map(|state| state.clone())
        .ok_or_else(|| mlua::Error::RuntimeError("remote bridge is not installed".into()))
}
fn error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}
fn node_info(ctx: &WorldRef, id: &str) -> mlua::Result<(String, bool, bool)> {
    let world = ctx.borrow();
    let node = world
        .nodes
        .get(id)
        .ok_or_else(|| error("Instance is not present in this world"))?;
    Ok((node.class_name.clone(), node.server_owned, node.destroyed))
}
fn received_node(ctx: &WorldRef, id: &str, expected: &[&str]) -> mlua::Result<()> {
    let (class, server_owned, destroyed) = node_info(ctx, id)?;
    if destroyed || !server_owned || !expected.contains(&class.as_str()) {
        return Err(error(format!(
            "received {} receiver required",
            expected.join(" or ")
        )));
    }
    Ok(())
}
fn received_instance(ctx: &WorldRef, id: &str) -> mlua::Result<()> {
    let world = ctx.borrow();
    let node = world
        .nodes
        .get(id)
        .ok_or_else(|| error("Remote argument Instance was not received"))?;
    if node.destroyed || !node.server_owned {
        return Err(error("Remote argument Instance was not received"));
    }
    Ok(())
}

struct Budget {
    bytes_left: usize,
    items_left: usize,
    ancestors: HashSet<usize>,
}
// Python's reflection bridge recognizes only Luau's explicit newproxy tag.
// Unknown engine userdata must not silently turn into nil.
fn is_newproxy(lua: &Lua, value: &Value) -> mlua::Result<bool> {
    unsafe {
        lua.exec_raw((value.clone(),), |state| {
            let slot = mlua::ffi::lua_gettop(state);
            let yes = mlua::ffi::lua_userdatatag(state, slot) == mlua::ffi::LUA_UTAG_LIMIT + 1;
            mlua::ffi::lua_settop(state, slot - 1);
            mlua::ffi::lua_pushboolean(state, i32::from(yes));
        })
    }
}
fn encode_remote(
    lua: &Lua,
    ctx: &WorldRef,
    value: &Value,
    depth: usize,
    budget: &mut Budget,
) -> mlua::Result<Json> {
    if depth > MAX_DEPTH || budget.items_left == 0 {
        return Err(error("Remote value nesting or item limit exceeded"));
    }
    budget.items_left -= 1;
    match value {
        Value::Table(table) => encode_table(lua, ctx, table, depth, budget),
        Value::Thread(_) | Value::Function(_) | Value::Error(_) | Value::LightUserData(_) => {
            Err(error("Remote argument datatype is not yet supported"))
        }
        Value::UserData(_) if is_newproxy(lua, value)? => Ok(Json::Null),
        Value::UserData(_) if world::instance_id(value).is_some() => {
            if !world::instance_belongs_to(value, ctx) {
                return Err(error(
                    "Remote argument Instance belongs to a different world",
                ));
            }
            let json = values::to_json(lua, value)?;
            let id = json
                .get("$ref")
                .and_then(Json::as_str)
                .ok_or_else(|| error("Remote Instance argument was not received"))?;
            received_instance(ctx, id)?;
            Ok(json)
        }
        _ => {
            let json = values::to_json(lua, value)?;
            let mut supported_items = MAX_ITEMS;
            validate_remote_json(&json, depth, &mut supported_items)?;
            let size = json.to_string().len();
            if size > budget.bytes_left {
                return Err(error("Remote argument exceeds byte limit"));
            }
            budget.bytes_left -= size;
            Ok(json)
        }
    }
}
fn encode_table(
    lua: &Lua,
    ctx: &WorldRef,
    table: &Table,
    depth: usize,
    budget: &mut Budget,
) -> mlua::Result<Json> {
    let pointer = table.to_pointer() as usize;
    if !budget.ancestors.insert(pointer) {
        return Err(error("tables cannot be cyclic"));
    }
    let result = (|| {
        let length = table.raw_len();
        if length > budget.items_left {
            return Err(error("Remote array exceeds item limit"));
        }
        if length > 0 {
            let mut array = Vec::with_capacity(length);
            for i in 1..=length {
                array.push(encode_remote(
                    lua,
                    ctx,
                    &table.raw_get::<Value>(i)?,
                    depth + 1,
                    budget,
                )?);
            }
            Ok(json!({"$remote_array":array}))
        } else {
            let mut entries = Vec::new();
            let mut seen = HashSet::new();
            for pair in table.clone().pairs::<Value, Value>() {
                let (key, value) = pair?;
                let encoded_key = match key {
                    Value::String(s) => {
                        let bytes = s.as_bytes();
                        if bytes.len() > budget.bytes_left {
                            return Err(error("Remote argument exceeds byte limit"));
                        }
                        budget.bytes_left -= bytes.len();
                        match std::str::from_utf8(&bytes) {
                            Ok(text) => Json::String(text.to_owned()),
                            Err(_) => {
                                json!({"$bytes_hex": bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()})
                            }
                        }
                    }
                    Value::Integer(n) => Json::String(n.to_string()),
                    Value::Number(n)
                        if n.is_finite() && n >= i64::MIN as f64 && n < i64::MAX as f64 =>
                    {
                        Json::String((n as i64).to_string())
                    }
                    _ => return Err(error("Remote dictionary key datatype is not yet supported")),
                };
                let unique = encoded_key.to_string();
                if !seen.insert(unique) {
                    return Err(error(
                        "Remote dictionary keys collide after string conversion",
                    ));
                }
                let encoded_value = encode_remote(lua, ctx, &value, depth + 1, budget)?;
                entries.push(json!([encoded_key, encoded_value]));
                if entries.len() > budget.items_left {
                    return Err(error("Remote dictionary exceeds item limit"));
                }
            }
            if entries.is_empty() {
                Ok(json!({"$remote_array":[]}))
            } else {
                Ok(json!({"$remote_map":entries}))
            }
        }
    })();
    budget.ancestors.remove(&pointer);
    result
}
fn validate_remote_json(value: &Json, depth: usize, items: &mut usize) -> mlua::Result<()> {
    validate_remote_json_detailed(value, depth, items, "$".to_owned())
        .map_err(|failure| error(failure.reason))
}

fn child_remote_path(path: &str, component: &str) -> String {
    let candidate = format!("{path}{component}");
    candidate.chars().take(96).collect()
}

fn remote_json_failure(value: &Json, path: String, reason: &str) -> RemoteJsonFailure {
    RemoteJsonFailure {
        path,
        shape: top_level_json_shape(value),
        reason: reason.to_owned(),
    }
}

fn validate_remote_json_detailed(
    value: &Json,
    depth: usize,
    items: &mut usize,
    path: String,
) -> Result<(), RemoteJsonFailure> {
    if depth > MAX_DEPTH || *items == 0 {
        return Err(remote_json_failure(value, path, "Remote value nesting or item limit exceeded"));
    }
    *items -= 1;
    match value {
        Json::Null | Json::Bool(_) | Json::String(_) | Json::Number(_) => Ok(()),
        Json::Array(values) => {
            if values.len() > MAX_ITEMS {
                return Err(remote_json_failure(value, path, "Remote array exceeds item limit"));
            }
            for (index, item) in values.iter().enumerate() {
                validate_remote_json_detailed(
                    item,
                    depth + 1,
                    items,
                    child_remote_path(&path, &format!("[{}]", index + 1)),
                )?;
            }
            Ok(())
        }
        Json::Object(map) if map.len() == 1 => {
            let (tag, body) = map.iter().next().unwrap();
            match tag.as_str() {
                "$ref" => {
                    if body.is_string() || body.is_null() {
                        Ok(())
                    } else {
                        Err(remote_json_failure(value, path, "invalid Remote Instance reference"))
                    }
                }
                "$bytes_hex" | "$buffer_hex" => {
                    let Some(s) = body.as_str() else {
                        return Err(remote_json_failure(value, path, "invalid binary remote value"));
                    };
                    let limit = if tag == "$buffer_hex" {
                        131_072
                    } else {
                        MAX_ARGUMENT_BYTES * 2
                    };
                    if s.len() > limit
                        || s.len() % 2 != 0
                        || !s.bytes().all(|b| b.is_ascii_hexdigit())
                    {
                        return Err(remote_json_failure(value, path, "Remote binary value exceeds byte limit"));
                    }
                    Ok(())
                }
                "$vector3" | "$color3" => validate_numbers(body, 3)
                    .map_err(|_| remote_json_failure(value, path, "invalid remote vector or color")),
                "$vector2" | "$number_range" => validate_numbers(body, 2)
                    .map_err(|_| remote_json_failure(value, path, "invalid remote vector or range")),
                "$udim" => {
                    if validate_numbers(body, 2).is_err() {
                        return Err(remote_json_failure(value, path, "invalid remote UDim"));
                    }
                    let scale = body.as_array().and_then(|v| v.get(1)).and_then(Json::as_f64)
                        .ok_or_else(|| remote_json_failure(value, path.clone(), "invalid remote UDim"))?;
                    if scale.fract() == 0.0 && scale >= i32::MIN as f64 && scale <= i32::MAX as f64
                    {
                        Ok(())
                    } else {
                        Err(remote_json_failure(value, path, "invalid remote UDim"))
                    }
                }
                "$udim2" | "$rect" => {
                    let Some(parts) = body.as_array().filter(|v| v.len() == 2) else {
                        return Err(remote_json_failure(value, path, "invalid remote value"));
                    };
                    validate_remote_json_detailed(&parts[0], depth + 1, items, child_remote_path(&path, ".first"))?;
                    validate_remote_json_detailed(&parts[1], depth + 1, items, child_remote_path(&path, ".second"))
                }
                "$cframe" => {
                    let (Some(p), Some(r)) = (body.get("position"), body.get("rotation")) else {
                        return Err(remote_json_failure(value, path, "invalid remote CFrame"));
                    };
                    if validate_numbers(p, 3).is_err() || validate_numbers(r, 9).is_err() {
                        Err(remote_json_failure(value, path, "invalid remote CFrame"))
                    } else {
                        Ok(())
                    }
                }
                "$brickcolor" => {
                    let Some(n) = body.as_f64() else {
                        return Err(remote_json_failure(value, path, "invalid remote BrickColor"));
                    };
                    if n.is_finite() && n.fract() == 0.0 && (0.0..=65535.0).contains(&n) {
                        Ok(())
                    } else {
                        Err(remote_json_failure(value, path, "invalid remote BrickColor"))
                    }
                }
                "$datetime" => {
                    let Some(n) = body.as_f64() else {
                        return Err(remote_json_failure(value, path, "invalid remote DateTime"));
                    };
                    if n.is_finite()
                        && n.fract() == 0.0
                        && (-17_987_443_200_000.0..=253_402_300_799_999.0).contains(&n)
                    {
                        Ok(())
                    } else {
                        Err(remote_json_failure(value, path, "invalid remote DateTime"))
                    }
                }
                "$remote_array" => {
                    let Some(array) = body.as_array() else {
                        return Err(remote_json_failure(value, path, "invalid remote array"));
                    };
                    if array.len() > MAX_ITEMS {
                        return Err(remote_json_failure(value, path, "Remote array exceeds item limit"));
                    }
                    for (index, item) in array.iter().enumerate() {
                        validate_remote_json_detailed(item, depth + 1, items, child_remote_path(&path, &format!("[{}]", index + 1)))?;
                    }
                    Ok(())
                }
                "$remote_map" => {
                    let Some(entries) = body.as_array() else {
                        return Err(remote_json_failure(value, path, "invalid remote dictionary"));
                    };
                    if entries.len() > MAX_ITEMS {
                        return Err(remote_json_failure(value, path, "Remote dictionary exceeds item limit"));
                    }
                    for (index, entry) in entries.iter().enumerate() {
                        let Some(pair) = entry.as_array().filter(|v| v.len() == 2) else {
                            return Err(remote_json_failure(value, child_remote_path(&path, &format!("{{entry {}}}", index + 1)), "invalid remote dictionary entry"));
                        };
                        if !pair[0].is_string()
                            && pair[0].get("$bytes_hex").and_then(Json::as_str).is_none()
                        {
                            return Err(remote_json_failure(&pair[0], child_remote_path(&path, &format!("{{entry {} key}}", index + 1)), "invalid remote dictionary key"));
                        }
                        validate_remote_json_detailed(&pair[1], depth + 1, items, child_remote_path(&path, &format!("{{entry {} value}}", index + 1)))?;
                    }
                    Ok(())
                }
                _ if tag.starts_with('$') => Err(remote_json_failure(value, path, "unsupported remote value")),
                _ => validate_remote_json_detailed(body, depth + 1, items, child_remote_path(&path, ".value")),
            }
        }
        Json::Object(map) => {
            if map.len() > MAX_ITEMS {
                return Err(remote_json_failure(value, path, "Remote dictionary exceeds item limit"));
            }
            for (index, child) in map.values().enumerate() {
                validate_remote_json_detailed(child, depth + 1, items, child_remote_path(&path, &format!("{{entry {} value}}", index + 1)))?;
            }
            Ok(())
        }
    }
}
fn validate_received_refs(ctx: &WorldRef, value: &Json) -> mlua::Result<()> {
    match value {
        Json::Object(map) if map.len() == 1 && map.contains_key("$ref") => {
            if let Some(id) = map.get("$ref").and_then(Json::as_str) {
                received_instance(ctx, id)?;
            }
        }
        Json::Array(items) => {
            for item in items {
                validate_received_refs(ctx, item)?;
            }
        }
        Json::Object(map) => {
            for item in map.values() {
                validate_received_refs(ctx, item)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn validate_numbers(value: &Json, count: usize) -> mlua::Result<()> {
    let values = value
        .as_array()
        .filter(|values| values.len() == count)
        .ok_or_else(|| error("invalid remote engine datatype"))?;
    if values.iter().all(|v| {
        v.as_f64()
            .is_some_and(|number| number.is_finite() && number.abs() <= f32::MAX as f64)
    }) {
        Ok(())
    } else {
        Err(error("invalid remote engine datatype"))
    }
}
pub(super) fn encode_args(lua: &Lua, ctx: &WorldRef, args: &MultiValue) -> mlua::Result<Vec<Json>> {
    if args.len() > MAX_ARGUMENTS {
        return Err(error("RemoteEvent argument or queue limit exceeded"));
    }
    let mut budget = Budget {
        bytes_left: MAX_ARGUMENT_BYTES,
        items_left: MAX_ITEMS,
        ancestors: HashSet::new(),
    };
    args.iter()
        .map(|v| encode_remote(lua, ctx, v, 0, &mut budget))
        .collect()
}
fn effect_len(ctx: &WorldRef, key: &str) -> usize {
    ctx.borrow()
        .effects
        .get(key)
        .and_then(Json::as_array)
        .map_or(0, Vec::len)
}
fn effect_bytes(ctx: &WorldRef, key: &str) -> usize {
    ctx.borrow()
        .effects
        .get(key)
        .and_then(Json::as_array)
        .map_or(0, |rows| rows.iter().map(|v| v.to_string().len()).sum())
}
fn fire_server(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    received_node(ctx, id, &["RemoteEvent", "UnreliableRemoteEvent"])?;
    if effect_len(ctx, "remote_server_events") >= MAX_QUEUE_ROWS {
        return Err(error("RemoteEvent argument or queue limit exceeded"));
    }
    let event = json!({"node":id,"arguments":encode_args(lua, ctx, &args)?});
    let size = event.to_string().len();
    if size > MAX_ROW_BYTES || effect_bytes(ctx, "remote_server_events") + size > MAX_QUEUE_BYTES {
        return Err(error("RemoteEvent byte queue limit exceeded"));
    }
    world::emit(ctx, "remote_server_events", event);
    Ok(MultiValue::new())
}
fn invoke_server(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    received_node(ctx, id, &["RemoteFunction"])?;
    prune_inactive_callers(lua, ctx)?;
    if args.len() > MAX_ARGUMENTS || effect_len(ctx, "remote_server_calls") >= MAX_QUEUE_ROWS {
        return Err(error("RemoteFunction argument or pending limit exceeded"));
    }
    let encoded = encode_args(lua, ctx, &args).map_err(|e|error(format!("InvokeServer arguments: {e}")))?;
    let state = app_state(lua)?;
    let (request_id, thread) = {
        let mut state = state.borrow_mut();
        if state.pending.len() >= 512 {
            return Err(error("RemoteFunction argument or pending limit exceeded"));
        }
        if state.next_request >= ACTOR_REQUEST_ID_BASE {
            return Err(error("RemoteFunction token exhausted"));
        }
        state.next_request = state.next_request.max(1);
        let request = state.next_request;
        state.next_request += 1;
        let thread = lua.current_thread();
        state.pending.insert(
            request,
            PendingCall {
                node: id.to_owned(),
                thread: thread.clone(),
            },
        );
        (request, thread)
    };
    let call = json!({"node":id,"request_id":request_id,"arguments":encoded});
    let size = call.to_string().len();
    if size > MAX_ROW_BYTES || effect_bytes(ctx, "remote_server_calls") + size > MAX_QUEUE_BYTES {
        state.borrow_mut().pending.remove(&request_id);
        return Err(error("RemoteFunction byte queue limit exceeded"));
    }
    if scheduler::park(lua, &thread).is_err() {
        state.borrow_mut().pending.remove(&request_id);
        return Err(error(
            "InvokeServer requires a scheduled persistent client thread",
        ));
    }
    world::emit(ctx, "remote_server_calls", call);
    let mut marker = MultiValue::new();
    marker.push_back(Value::Integer(request_id as i64));
    Ok(marker)
}

fn invoke_server_getter(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    let (class, owned, destroyed) = node_info(ctx, id)?;
    if class != "RemoteFunction" {
        return Ok(None);
    }
    if !owned || destroyed {
        return Err(error("received RemoteFunction receiver required"));
    }
    let weak = Rc::downgrade(ctx);
    let instance_id = id.to_owned();
    let begin = lua.create_function(move |lua, args: MultiValue| {
        let ctx = weak
            .upgrade()
            .ok_or_else(|| error("world has been released"))?;
        invoke_server(lua, &ctx, &instance_id, args)
    })?;
    let factory: Function = lua.load(
        "return function(begin)\n\
            return function(self, ...)\n\
                if not coroutine.isyieldable() then error('RemoteFunction:InvokeServer must run in a yieldable coroutine', 2) end\n\
                begin(...)\n\
                local result = table.pack(coroutine.yield())\n\
                if not result[1] then error(result[2] or 'InvokeServer failed', 0) end\n\
                return table.unpack(result, 2, result.n)\n\
            end\n\
        end",
    ).eval()?;
    Ok(Some(Value::Function(factory.call::<Function>(begin)?)))
}

pub fn install(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    if lua.app_data_ref::<RemoteStateRef>().is_none() {
        lua.set_app_data(Rc::new(RefCell::new(RemoteState::default())));
    }
    let state = app_state(lua)?;
    if state.borrow().callback_runner.is_none() {
        let runner = lua
            .load(
                "local invoke = pcall\n\
                 return function(callback, ...) return invoke(callback, ...) end",
            )
            .eval::<Function>()?;
        state.borrow_mut().callback_runner = Some(runner);
    }
    world::register_method(ctx, "RemoteEvent", "FireServer", fire_server);
    world::register_method(ctx, "UnreliableRemoteEvent", "FireServer", fire_server);
    world::register_getter(ctx, "InvokeServer", invoke_server_getter);
    world::register_getter(ctx, "OnClientEvent", on_client_event_getter);
    world::register_getter(ctx, "OnClientInvoke", on_client_invoke_getter);
    world::register_setter(ctx, "OnClientInvoke", set_on_client_invoke);
    Ok(())
}

fn on_client_invoke_getter(_lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    if node_info(ctx, id)?.0 == "RemoteFunction" {
        return Err(error("OnClientInvoke is a write-only callback"));
    }
    Ok(None)
}
fn on_client_event_getter(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    let (class, owned, destroyed) = node_info(ctx, id)?;
    if class != "RemoteEvent" && class != "UnreliableRemoteEvent" {
        return Ok(None);
    }
    if !owned || destroyed {
        return Err(error("received RemoteEvent receiver required"));
    }
    Ok(Some(signals::signal(lua, ctx, id, "OnClientEvent")?))
}
fn set_on_client_invoke(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    if node_info(ctx, id)?.0 != "RemoteFunction" {
        return Ok(false);
    }
    let state = app_state(lua)?;
    let callback = match value {
        Value::Nil => None,
        Value::Function(function) => Some(BoundCallback { function, policy: scheduler::current_policy(lua) }),
        _ => return Err(error("OnClientInvoke must be a function or nil")),
    };
    let mut state = state.borrow_mut();
    if let Some(callback) = callback {
        state.callbacks.insert(id.to_owned(), callback.clone());
        for call in state.invocations.iter_mut().filter(|call| call.node == id) {
            call.callback = Some(callback.clone());
        }
    } else {
        state.callbacks.remove(id);
        for call in state.invocations.iter_mut().filter(|call| call.node == id) {
            call.callback = None;
        }
    }
    Ok(true)
}

/// Return the actual callback previously assigned through Instance.OnClientInvoke.
/// Local service adapters use this for reflection helpers without duplicating
/// or exposing the private per-VM callback table.
pub fn get_on_client_invoke(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Function>> {
    if node_info(ctx, id)?.0 != "RemoteFunction" {
        return Ok(None);
    }
    let state = app_state(lua)?;
    let callback = state.borrow().callbacks.get(id).map(|callback| callback.function.clone());
    Ok(callback)
}

/// Fire an event that came from the already authenticated session transport.
pub fn fire_client_event(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    arguments: &Json,
) -> mlua::Result<()> {
    fire_client_event_inner(lua, ctx, id, arguments).map_err(|failure| match failure {
        EventDeliveryError::Runtime(error) => error,
        EventDeliveryError::Payload { index, failure } => {
            error(remote_argument_diagnostic("server-to-client", "RemoteEvent.OnClientEvent", index, &failure))
        }
    })
}

fn fire_client_event_inner(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    arguments: &Json,
) -> Result<(), EventDeliveryError> {
    received_node(ctx, id, &["RemoteEvent", "UnreliableRemoteEvent"])
        .map_err(EventDeliveryError::Runtime)?;
    let values = arguments
        .as_array()
        .ok_or_else(|| EventDeliveryError::Runtime(error("invalid RemoteEvent argument tuple")))?;
    // Match ordinary signal delivery: events with no live consumer expire here.
    // In particular, unavailable datatypes must not terminate world ingestion
    // before a LocalScript has even connected to this RemoteEvent.
    if !signals::has_receivers(lua, ctx, id, "OnClientEvent")
        .map_err(EventDeliveryError::Runtime)?
    {
        return Ok(());
    }
    let normalized_values = normalize_received_remote_arguments(values)
        .map_err(|(index, failure)| EventDeliveryError::Payload { index, failure })?;
    let values = normalized_values.as_deref().unwrap_or(values);
    if let Err((index, failure)) = validate_remote_arguments(ctx, values) {
        return Err(EventDeliveryError::Payload { index, failure });
    }
    let mut args = MultiValue::new();
    for (index, value) in values.iter().enumerate() {
        args.push_back(
            values::from_json(lua, value).map_err(|_| EventDeliveryError::Payload {
                index: index + 1,
                failure: RemoteJsonFailure {
                    path: "$".to_owned(),
                    shape: top_level_json_shape(value),
                    reason: "remote value decoder rejected payload".to_owned(),
                },
            })?,
        );
    }
    signals::fire(lua, ctx, id, "OnClientEvent", args).map_err(EventDeliveryError::Runtime)
}

/// Queue an authenticated server request to the LocalScript OnClientInvoke
/// callback. The owning host calls `step_client_invocations` from its scheduler
/// pump and sends the returned rows through its existing session bridge.
pub fn queue_client_invocation(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    request_id: i32,
    arguments: Json,
) -> mlua::Result<()> {
    received_node(ctx, id, &["RemoteFunction"])?;
    let args = arguments
        .as_array()
        .filter(|v| v.len() <= MAX_ARGUMENTS)
        .ok_or_else(|| error("invalid RemoteOnInvokeClient request"))?;
    let argument_error = match validate_remote_arguments(ctx, args) {
        Ok(()) => None,
        Err((index, failure)) => Some(remote_argument_diagnostic(
            "server-to-client", "RemoteFunction.RemoteOnInvokeClient", index, &failure,
        )),
    };
    if let Some(message) = argument_error.as_deref() {
        record_remote_diagnostic(lua, message);
    }
    let state = app_state(lua)?;
    let mut state = state.borrow_mut();
    if state
        .invocations
        .iter()
        .any(|call| call.node == id && call.request_id == request_id)
    {
        return Err(error("duplicate pending remote invocation"));
    }
    if state.invocations.len() >= 512 {
        return Err(error("remote invocation limit exceeded"));
    }
    let callback = state.callbacks.get(id).cloned();
    state.invocations.push(IncomingCall {
        node: id.to_owned(),
        request_id,
        arguments,
        thread: None,
        callback,
        error: argument_error,
    });
    Ok(())
}

/// Apply server event actions from a validated replication delta. `previous`
/// filters duplicate events when the same delta is retried; it must be sampled
/// before World::apply_delta advances its sequence.
pub fn process_delta_events(
    lua: &Lua,
    ctx: &WorldRef,
    delta: &Json,
    previous_sequence: u64,
) -> mlua::Result<DeltaEventReport> {
    let actions = delta
        .get("actions")
        .and_then(Json::as_array)
        .ok_or_else(|| error("world delta actions must be an array"))?;
    let mut report = DeltaEventReport::default();
    for action in actions {
        if action.get("kind").and_then(Json::as_str) != Some("event") {
            continue;
        }
        let sequence = action
            .get("sequence")
            .and_then(Json::as_u64)
            .ok_or_else(|| error("server event sequence is missing"))?;
        if sequence <= previous_sequence {
            continue;
        }
        report.received += 1;
        let id = action
            .get("id")
            .and_then(Json::as_str)
            .ok_or_else(|| error("server event Instance id is missing"))?;
        let name = action
            .get("name")
            .and_then(Json::as_str)
            .ok_or_else(|| error("server event name is missing"))?;
        let args = action
            .get("arguments")
            .and_then(Json::as_array)
            .ok_or_else(|| error("server event arguments must be an array"))?;
        if super::player_lifecycle::consumes_event(lua, ctx, sequence, id, name, args) {
            continue;
        }
        let Ok((class, _, destroyed)) = node_info(ctx, id) else {
            report.unresolved += 1;
            continue;
        };
        if destroyed {
            report.unresolved += 1;
            continue;
        }
        if class == "RemoteFunction" && name == "RemoteOnInvokeClient" {
            if args.len() != 2 {
                return Err(error("invalid RemoteOnInvokeClient request"));
            }
            let request = args[0]
                .as_i64()
                .filter(|v| i32::try_from(*v).is_ok())
                .ok_or_else(|| error("remote request ID exceeds int32"))?;
            queue_client_invocation(lua, ctx, id, request as i32, args[1].clone())?;
            report.queued_invocations += 1;
        } else if (class == "RemoteEvent" || class == "UnreliableRemoteEvent")
            && name == "OnClientEvent"
        {
            match fire_client_event_inner(lua, ctx, id, &Json::Array(args.clone())) {
                Ok(()) => {}
                Err(EventDeliveryError::Runtime(error)) => return Err(error),
                Err(EventDeliveryError::Payload { index, failure }) => {
                    let message = remote_argument_diagnostic(
                        "server-to-client", "RemoteEvent.OnClientEvent", index, &failure,
                    );
                    record_remote_diagnostic(lua, &message);
                    report.rejected_payloads += 1;
                }
            }
        } else if signals::has_receivers(lua, ctx, id, name)? {
            let mut values = MultiValue::new();
            for value in args {
                values.push_back(values::from_json(lua, value)?);
            }
            signals::fire(lua, ctx, id, name, values)?;
        }
    }
    cleanup_destroyed_instances(lua, ctx)?;
    Ok(report)
}

/// Check only remotes holding VM state. An unrelated world instance cannot
/// require remote cleanup, and most receive ticks have no active remotes.
pub fn cleanup_destroyed_instances(lua: &Lua, ctx: &WorldRef) -> mlua::Result<usize> {
    let state = app_state(lua)?;
    let ids = {
        let state = state.borrow();
        let world = ctx.borrow();
        state.callbacks.keys().map(String::as_str)
            .chain(state.pending.values().map(|call| call.node.as_str()))
            .chain(state.invocations.iter().map(|call| call.node.as_str()))
            .filter(|id| world.nodes.get(id).is_none_or(|node| {
                node.destroyed || node.class_name != "RemoteFunction"
            }))
            .map(str::to_owned)
            .collect::<HashSet<_>>()
    };
    let mut cleaned = 0;
    for id in ids {
        cleaned += cleanup_instance(lua, ctx, &id)?;
    }
    Ok(cleaned)
}

/// Current client-side RemoteFunction calls waiting for a matching host reply.
/// IDs come from actual queued InvokeServer operations.
pub fn active_requests(lua: &Lua) -> mlua::Result<Json> {
    let state = app_state(lua)?;
    let state = state.borrow();
    let mut ids: Vec<u64> = state.pending.keys().copied().collect();
    ids.sort_unstable();
    Ok(json!(ids))
}

/// Remove pending InvokeServer bookkeeping when the scheduler has already
/// completed, failed, or cancelled its parked coroutine. Destroyed instances
/// are handled by `cleanup_destroyed_instances` separately.
pub fn prune_inactive_requests(
    lua: &Lua,
    ctx: &WorldRef,
    _scheduler_ref: &SchedulerRef,
) -> mlua::Result<usize> {
    prune_inactive_callers(lua, ctx)
}

pub(super) fn prune_inactive_callers(lua: &Lua, ctx: &WorldRef) -> mlua::Result<usize> {
    let Some(state) = lua.app_data_ref::<RemoteStateRef>().map(|state| state.clone()) else {
        return Ok(0);
    };
    let mut stale = HashSet::new();
    state.borrow_mut().pending.retain(|request, call| {
        let active = scheduler::is_waiting(lua, &call.thread);
        if !active {
            stale.insert(*request);
        }
        active
    });
    if stale.is_empty() {
        return Ok(0);
    }
    let mut world = ctx.borrow_mut();
    if let Some(Json::Array(calls)) = world.effects.get_mut("remote_server_calls") {
        calls.retain(|call| {
            call.get("request_id")
                .and_then(Json::as_u64)
                .is_none_or(|request| !stale.contains(&request))
        });
    }
    Ok(stale.len())
}

/// Resume queued OnClientInvoke callbacks under the normal frame budget.
/// Returned rows match `remote_client_replies` consumed by the session owner.
pub fn dispatch_client_invocations(lua: &Lua, _ctx: &WorldRef) -> mlua::Result<()> {
    let state = app_state(lua)?;
    let runner = state
        .borrow()
        .callback_runner
        .clone()
        .ok_or_else(|| error("RemoteFunction callback runner is unavailable"))?;
    let pending: Vec<(usize, BoundCallback, Json)> = state
        .borrow()
        .invocations
        .iter()
        .enumerate()
        .filter_map(|(index, call)| {
            (call.thread.is_none() && call.error.is_none())
                .then(|| {
                    call.callback
                        .clone()
                        .map(|callback| (index, callback, call.arguments.clone()))
                })
                .flatten()
        })
        .collect();
    for (index, callback, encoded_args) in pending {
        let array = encoded_args
            .as_array()
            .ok_or_else(|| error("invalid RemoteOnInvokeClient arguments"))?;
        let mut args = MultiValue::with_capacity(array.len() + 1);
        args.push_back(Value::Function(callback.function));
        for value in array {
            args.push_back(values::from_json(lua, value)?);
        }
        let thread = scheduler::enqueue_with_policy(
            lua,
            runner.clone(),
            args,
            callback.policy,
        )?;
        if let Some(call) = state.borrow_mut().invocations.get_mut(index) {
            if call.thread.is_none() {
                call.thread = Some(thread);
            }
        }
    }
    Ok(())
}

/// Retire an Actor's unbound copy only after the root VM produced the actual
/// reply. Bound/running Actor callbacks retain their local side effects.
pub(crate) fn retire_answered_unbound_invocations(
    lua: &Lua,
    answered: &HashSet<(String, i32)>,
) -> mlua::Result<usize> {
    let state=app_state(lua)?;
    let mut state=state.borrow_mut();
    let before=state.invocations.len();
    state.invocations.retain(|call| !(call.callback.is_none() && call.thread.is_none()
        && call.error.is_none() && answered.contains(&(call.node.clone(),call.request_id))));
    Ok(before-state.invocations.len())
}

/// Collect completed OnClientInvoke callbacks after the ordinary scheduler
/// frame. This preserves yielding callbacks and keeps all VM work in one pump.
pub fn collect_client_invocations(
    lua: &Lua,
    ctx: &WorldRef,
    scheduler_ref: &SchedulerRef,
) -> mlua::Result<Vec<Json>> {
    let state = app_state(lua)?;
    let mut replies = Vec::new();
    let mut state = state.borrow_mut();
    let mut index = 0;
    while index < state.invocations.len() {
        if let Some(message) = state.invocations[index].error.take() {
            let call = state.invocations.remove(index);
            replies.push(
                json!({"node":call.node,"request_id":call.request_id,"ok":false,"error":message}),
            );
            continue;
        }
        let Some(thread) = state.invocations[index].thread.as_ref() else {
            index += 1;
            continue;
        };
        let Some(outcome) = scheduler::take_outcome(scheduler_ref, thread) else {
            index += 1;
            continue;
        };
        let call = state.invocations.remove(index);
        let (ok, values_json, err) = match outcome {
            TaskOutcome::Completed(mut values_out) => {
                let succeeded = match values_out.pop_front() {
                    Some(Value::Boolean(succeeded)) => succeeded,
                    _ => {
                        replies.push(json!({
                            "node":call.node,
                            "request_id":call.request_id,
                            "ok":false,
                            "error":"RemoteFunction callback produced an invalid result envelope"
                        }));
                        continue;
                    }
                };
                if !succeeded {
                    let message = match values_out.pop_front() {
                        Some(Value::String(message)) => message.to_string_lossy().to_string(),
                        Some(Value::Integer(value)) => value.to_string(),
                        Some(Value::Number(value)) => value.to_string(),
                        Some(Value::Error(error)) => error.to_string(),
                        // Don't invoke an authored __tostring outside the
                        // callback's scheduler/CPU budget while draining replies.
                        Some(_) => "RemoteFunction callback failed with a non-string error".into(),
                        None => "RemoteFunction callback failed".into(),
                    };
                    (
                        false,
                        Json::Null,
                        Some(message.chars().take(4096).collect()),
                    )
                } else {
                    let mut values_json = Vec::with_capacity(values_out.len());
                    let mut remaining = MAX_ITEMS;
                    let mut budget = Budget {
                        bytes_left: MAX_ARGUMENT_BYTES,
                        items_left: MAX_ITEMS,
                        ancestors: HashSet::new(),
                    };
                    let mut failed = None;
                    for value in values_out.iter() {
                        match encode_remote(lua, ctx, value, 0, &mut budget) {
                            Ok(v) => {
                                if let Err(e) = validate_remote_json(&v, 0, &mut remaining) {
                                    failed = Some(e.to_string());
                                    break;
                                }
                                values_json.push(v);
                            }
                            Err(e) => {
                                failed = Some(e.to_string());
                                break;
                            }
                        }
                    }
                    if let Some(e) = failed {
                        (false, Json::Null, Some(e))
                    } else {
                        (true, Json::Array(values_json), None)
                    }
                }
            }
            TaskOutcome::Error(message) => (
                false,
                Json::Null,
                Some(message.chars().take(4096).collect()),
            ),
            TaskOutcome::Cancelled => (
                false,
                Json::Null,
                Some("RemoteFunction callback was cancelled".into()),
            ),
            TaskOutcome::TimedOut(_) => (
                false,
                Json::Null,
                Some("RemoteFunction callback exceeded its CPU budget".into()),
            ),
            TaskOutcome::Yielded(_) => {
                index += 1;
                state.invocations.insert(index.saturating_sub(1), call);
                continue;
            }
        };
        let mut reply = json!({"node":call.node,"request_id":call.request_id,"ok":ok});
        if ok {
            reply["values"] = values_json;
        } else {
            reply["error"] = json!(err.unwrap_or_else(|| "RemoteFunction callback failed".into()));
        }
        replies.push(reply);
    }
    Ok(replies)
}

/// Match only the pending request ID and receiver from the host response. Bad
/// batches are rejected before any waiter is resumed.
pub fn complete_server_results(
    lua: &Lua,
    ctx: &WorldRef,
    responses: &[Json],
) -> mlua::Result<usize> {
    if responses.len() > 128 {
        return Err(error("invalid RemoteFunction response batch"));
    }
    let state = app_state(lua)?;
    let mut parsed = Vec::new();
    let pending_calls: HashMap<u64, (String, mlua::Thread)> = state
        .borrow()
        .pending
        .iter()
        .map(|(request, pending)| (*request, (pending.node.clone(), pending.thread.clone())))
        .collect();
    let mut diagnostics = Vec::new();
    {
        let mut seen = HashSet::new();
        for response in responses {
            let request = response
                .get("request_id")
                .and_then(Json::as_u64)
                .ok_or_else(|| error("invalid RemoteFunction response request_id"))?;
            let node = response
                .get("node")
                .and_then(Json::as_str)
                .ok_or_else(|| error("invalid RemoteFunction response node"))?;
            if !seen.insert((request, node.to_owned())) {
                continue;
            }
            let Some((pending_node, thread)) = pending_calls.get(&request) else {
                continue;
            };
            if pending_node != node {
                continue;
            }
            let ok = response
                .get("ok")
                .and_then(Json::as_bool)
                .ok_or_else(|| error("invalid RemoteFunction response status"))?;
            let mut args = MultiValue::new();
            if ok {
                let values_json = response
                    .get("values")
                    .and_then(Json::as_array)
                    .ok_or_else(|| error("invalid RemoteFunction return tuple"))?;
                let mut items = MAX_ITEMS;
                let mut invalid_result = (values_json.len() > MAX_ARGUMENTS).then(|| {
                    remote_argument_diagnostic("server-to-client", "RemoteFunction.InvokeServer result", MAX_ARGUMENTS + 1,
                        &RemoteJsonFailure { path: "$".to_owned(), shape: "argument-count limit".to_owned(), reason: "return tuple exceeds argument limit".to_owned() })
                });
                let mut bytes = 0usize;
                for (index, value) in values_json.iter().enumerate().take(MAX_ARGUMENTS) {
                    if invalid_result.is_some() {
                        break;
                    }
                    let detailed = validate_remote_json_detailed(value, 0, &mut items, "$".to_owned());
                    bytes = bytes.saturating_add(value.to_string().len());
                    if let Err(failure) = detailed {
                        invalid_result = Some(remote_argument_diagnostic(
                            "server-to-client",
                            "RemoteFunction.InvokeServer result",
                            index + 1,
                            &failure,
                        ));
                        break;
                    }
                    if validate_received_refs(ctx, value).is_err() || bytes > MAX_ARGUMENT_BYTES {
                        invalid_result = Some(remote_argument_diagnostic(
                            "server-to-client", "RemoteFunction.InvokeServer result", index + 1,
                            &RemoteJsonFailure {
                                path: "$".to_owned(),
                                shape: top_level_json_shape(value),
                                reason: if bytes > MAX_ARGUMENT_BYTES { "remote argument byte limit exceeded" } else { "invalid or unavailable Remote Instance reference" }.to_owned(),
                            },
                        ));
                        break;
                    }
                    match values::from_json(lua, value) {
                        Ok(value) => args.push_back(value),
                        Err(_) => {
                            invalid_result = Some(remote_argument_diagnostic(
                                "server-to-client",
                                "RemoteFunction.InvokeServer result",
                                index + 1,
                                &RemoteJsonFailure {
                                    path: "$".to_owned(),
                                    shape: top_level_json_shape(value),
                                    reason: "remote value decoder rejected payload".to_owned(),
                                },
                            ));
                            break;
                        }
                    }
                }
                if let Some(message) = invalid_result {
                    diagnostics.push(message.clone());
                    args = MultiValue::new();
                    args.push_back(Value::Boolean(false));
                    args.push_back(Value::String(lua.create_string(&message)?));
                } else {
                    args.push_front(Value::Boolean(true));
                }
            } else {
                let message = response
                    .get("error")
                    .and_then(Json::as_str)
                    .ok_or_else(|| error("invalid RemoteFunction error result"))?;
                args.push_back(Value::Boolean(false));
                args.push_back(Value::String(
                    lua.create_string(&message.chars().take(4096).collect::<String>())?,
                ));
            }
            parsed.push((request, thread.clone(), args));
        }
    }
    for diagnostic in diagnostics {
        record_remote_diagnostic(lua, &diagnostic);
    }
    for (_, thread, args) in &parsed {
        if scheduler::is_waiting(lua, thread) {
            scheduler::resume(lua, thread.clone(), args.clone())?;
        }
    }
    let mut state = state.borrow_mut();
    for (request, _, _) in &parsed {
        state.pending.remove(request);
    }
    Ok(parsed.len())
}

/// Transfer the queued server-side remote effects to the session owner. Each
/// returned row is removed exactly once; packet delivery remains the owner's
/// responsibility.
pub fn drain_outbound(ctx: &WorldRef) -> Json {
    let mut world = ctx.borrow_mut();
    let events = world
        .effects
        .remove("remote_server_events")
        .unwrap_or_else(|| Json::Array(Vec::new()));
    let calls = world
        .effects
        .remove("remote_server_calls")
        .unwrap_or_else(|| Json::Array(Vec::new()));
    json!({"remote_server_events":events,"remote_server_calls":calls})
}

/// Release per-instance callback and request records after the engine removes
/// the corresponding received node.
pub fn cleanup_instance(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<usize> {
    let state = app_state(lua)?;
    let (pending, invocation_count) = {
        let mut state = state.borrow_mut();
        state.callbacks.remove(id);
        let mut invocation_count = 0;
        for call in state.invocations.iter_mut().filter(|call| call.node == id) {
            call.callback = None;
            call.error = Some("RemoteFunction was destroyed during invocation".into());
            invocation_count += 1;
        }
        let pending: Vec<(u64, mlua::Thread)> = state
            .pending
            .iter()
            .filter_map(|(request, call)| {
                (call.node == id).then_some((*request, call.thread.clone()))
            })
            .collect();
        for (request, _) in &pending {
            state.pending.remove(request);
        }
        (pending, invocation_count)
    };
    for (request, thread) in &pending {
        let mut args = MultiValue::new();
        args.push_back(Value::Boolean(false));
        args.push_back(Value::String(
            lua.create_string("RemoteFunction was destroyed during InvokeServer")?,
        ));
        if scheduler::is_waiting(lua, thread) {
            scheduler::resume(lua, thread.clone(), args)?;
        }
        if let Some(rows) = ctx
            .borrow_mut()
            .effects
            .get_mut("remote_server_calls")
            .and_then(Json::as_array_mut)
        {
            rows.retain(|row| row.get("request_id").and_then(Json::as_u64) != Some(*request));
        }
    }
    Ok(invocation_count + pending.len())
}

#[cfg(test)]
#[path = "remotes_tests.rs"]
mod tests;
