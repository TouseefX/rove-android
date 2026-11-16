//! Local helpers over this VM's own Instance handles and callback stores.
//!
//! This module deliberately does not synthesize touch/prompt events, hidden
//! properties, or remote ownership. It only reflects values already held by
//! the current Lua/world state.

use super::{
    bindables, remotes,
    world::{self, WorldRef},
};
use mlua::{Function, Lua, MultiValue, ObjectLike, Thread, Value};
use serde_json::Value as Json;
use std::{cell::RefCell, collections::HashMap, rc::Rc};

type ThreadStore = Rc<RefCell<HashMap<(usize, String, String), Thread>>>;
type ScriptabilityStore = Rc<RefCell<HashMap<(usize, String, String), bool>>>;
const MAX_CALLBACK_THREADS: usize = 2048;
const MAX_SCRIPTABILITY_OVERRIDES: usize = 4096;

fn world_key(ctx: &WorldRef) -> usize {
    Rc::as_ptr(ctx) as usize
}

pub(super) fn canonical_hidden_property(class: &str, key: &str) -> String {
    match (class, key) {
        ("Fire", "size_xml") => "Size".into(),
        ("Humanoid", "Health_XML") => "Health".into(),
        _ => key.into(),
    }
}

fn is_reflected_hidden(class: &str, key: &str) -> bool {
    matches!(key, "Tags" | "NetworkOwnerV3" | "DefinesCapabilities")
        || matches!(
            (class, key),
            ("Fire", "size_xml") | ("Humanoid", "Health_XML")
        )
        || (key == "formFactorRaw"
            && world::class_chain(class)
                .iter()
                .any(|ancestor| ancestor == "FormFactorPart"))
        || (key == "isPlayerScript"
            && matches!(class, "Script" | "LocalScript" | "ModuleScript"))
}

fn schema_property_type(ctx: &WorldRef, class: &str, key: &str) -> Option<i64> {
    let world = ctx.borrow();
    let classes = world.snapshot.get("class_property_types")?.as_object()?;
    for ancestor in world::class_chain(class) {
        if let Some(value) = classes
            .get(&ancestor)
            .and_then(|items| items.get(key))
            .and_then(Json::as_i64)
        {
            return Some(value);
        }
    }
    None
}

fn schema_marks_hidden(ctx: &WorldRef, class: &str, key: &str) -> bool {
    matches!(schema_property_type(ctx, class, key), Some(8 | 46 | 47))
}

// Some native properties are reflected even though the snapshot does not
// capture their value. Keep descriptor knowledge separate from value reads so
// scriptability queries can work without inventing a value.
fn has_value_less_reflected_descriptor(class: &str, key: &str) -> bool {
    key == "isPlayerScript" && matches!(class, "Script" | "LocalScript" | "ModuleScript")
}

fn property_is_known(ctx: &WorldRef, id: &str, class: &str, key: &str) -> bool {
    if key == "Tags" {
        return ctx
            .borrow()
            .nodes
            .get(id)
            .is_some_and(|node| node.collection_tags_known);
    }
    world::constructor_default(class, key).is_ok()
        || world::get_property(ctx, id, key).is_ok()
        || schema_property_type(ctx, class, key).is_some()
        || has_value_less_reflected_descriptor(class, key)
}

fn scriptability_store(lua: &Lua) -> ScriptabilityStore {
    if lua.app_data_ref::<ScriptabilityStore>().is_none() {
        lua.set_app_data(ScriptabilityStore::default());
    }
    lua.app_data_ref::<ScriptabilityStore>()
        .expect("scriptability store installed")
        .clone()
}

fn scriptable(ctx: &WorldRef, id: &str, class: &str, key: &str, lua: &Lua) -> Option<bool> {
    let canonical = canonical_hidden_property(class, key);
    if !property_is_known(ctx, id, class, key) && !property_is_known(ctx, id, class, &canonical) {
        return None;
    }
    let key = (world_key(ctx), id.to_owned(), key.to_owned());
    scriptability_store(lua)
        .borrow()
        .get(&key)
        .copied()
        .or_else(|| {
            Some(!is_reflected_hidden(class, &key.2) && !schema_marks_hidden(ctx, class, &key.2))
        })
}

/// Called from ordinary Instance reads and writes after method lookup. Hidden
/// snapshot fields retain their native default access and per-instance opt-in.
pub(super) fn ensure_property_scriptable(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    key: &str,
) -> mlua::Result<()> {
    let class = ctx
        .borrow()
        .nodes
        .get(id)
        .map(|node| node.class_name.clone())
        .unwrap_or_default();
    if scriptable(ctx, id, &class, key, lua) == Some(false) {
        return Err(mlua::Error::RuntimeError(
            "property is not scriptable".into(),
        ));
    }
    Ok(())
}

fn property_args(
    args: MultiValue,
    name: &str,
    arity: usize,
) -> mlua::Result<(Value, String, Option<Value>)> {
    let args: Vec<Value> = args.into_iter().collect();
    if args.len() != arity {
        return Err(mlua::Error::RuntimeError(format!(
            "{name} expects {arity} arguments"
        )));
    }
    let property = match &args[1] {
        Value::String(value) => value.to_str()?.to_string(),
        _ => {
            return Err(mlua::Error::RuntimeError(
                "property name must be a string".into(),
            ))
        }
    };
    Ok((args[0].clone(), property, args.get(2).cloned()))
}

fn hidden_get(lua: &Lua, ctx: &WorldRef, args: MultiValue) -> mlua::Result<MultiValue> {
    let (instance, raw_key, _) = property_args(args, "gethiddenproperty", 2)?;
    let id = checked_instance(&instance, ctx)?;
    let class = ctx
        .borrow()
        .nodes
        .get(&id)
        .map(|node| node.class_name.clone())
        .unwrap_or_default();
    let key = canonical_hidden_property(&class, &raw_key);
    if !property_is_known(ctx, &id, &class, &raw_key) && !property_is_known(ctx, &id, &class, &key)
    {
        return Err(mlua::Error::RuntimeError(format!(
            "unknown property '{raw_key}'"
        )));
    }
    let hidden =
        is_reflected_hidden(&class, &raw_key) || schema_marks_hidden(ctx, &class, &raw_key);
    let value = if raw_key == "Tags" {
        let bytes = ctx
            .borrow()
            .nodes
            .get(&id)
            .map(|node| node.collection_tags.join("\0"))
            .ok_or_else(|| mlua::Error::RuntimeError("Instance is unavailable".into()))?;
        Value::String(lua.create_string(bytes.as_bytes())?)
    } else {
        let value = world::get_property(ctx, &id, &key)?;
        world::value_from_json(lua, ctx, &value)?
    };
    Ok(MultiValue::from_vec(vec![value, Value::Boolean(hidden)]))
}

fn hidden_set(lua: &Lua, ctx: &WorldRef, args: MultiValue) -> mlua::Result<bool> {
    let (instance, raw_key, value) = property_args(args, "sethiddenproperty", 3)?;
    let id = checked_instance(&instance, ctx)?;
    let class = ctx
        .borrow()
        .nodes
        .get(&id)
        .map(|node| node.class_name.clone())
        .unwrap_or_default();
    let key = canonical_hidden_property(&class, &raw_key);
    if !property_is_known(ctx, &id, &class, &raw_key) && !property_is_known(ctx, &id, &class, &key)
    {
        return Err(mlua::Error::RuntimeError(format!(
            "unknown property '{raw_key}'"
        )));
    }
    if raw_key == "Tags"
        || raw_key == "NetworkOwnerV3"
        || raw_key == "DefinesCapabilities"
        || raw_key == "isPlayerScript"
    {
        return Err(mlua::Error::RuntimeError(format!(
            "property '{raw_key}' is read-only in this snapshot"
        )));
    }
    let value = value.expect("three argument property call");
    let json = super::values::to_json(lua, &value)?;
    world::set_property(lua, ctx, &id, &key, json)?;
    Ok(is_reflected_hidden(&class, &raw_key) || schema_marks_hidden(ctx, &class, &raw_key))
}

fn set_scriptability(lua: &Lua, ctx: &WorldRef, args: MultiValue) -> mlua::Result<Value> {
    let (instance, raw_key, enabled) = property_args(args, "setscriptable", 3)?;
    let id = checked_instance(&instance, ctx)?;
    let class = ctx
        .borrow()
        .nodes
        .get(&id)
        .map(|node| node.class_name.clone())
        .unwrap_or_default();
    let canonical = canonical_hidden_property(&class, &raw_key);
    if !property_is_known(ctx, &id, &class, &raw_key)
        && !property_is_known(ctx, &id, &class, &canonical)
    {
        // Match the native helper: an unknown reflected member has no prior
        // scriptability value and cannot be enabled by this override.
        return Ok(Value::Nil);
    }
    let Value::Boolean(enabled) = enabled.expect("three argument property call") else {
        return Err(mlua::Error::RuntimeError(
            "scriptable flag must be a boolean".into(),
        ));
    };
    let previous = scriptable(ctx, &id, &class, &raw_key, lua).unwrap_or(false);
    let store = scriptability_store(lua);
    let mut store = store.borrow_mut();
    let key = (world_key(ctx), id, raw_key);
    if !store.contains_key(&key) && store.len() >= MAX_SCRIPTABILITY_OVERRIDES {
        return Err(mlua::Error::RuntimeError(
            "scriptability override limit exceeded".into(),
        ));
    }
    store.insert(key, enabled);
    Ok(Value::Boolean(previous))
}

fn is_scriptable(lua: &Lua, ctx: &WorldRef, args: MultiValue) -> mlua::Result<Value> {
    let (instance, raw_key, _) = property_args(args, "isscriptable", 2)?;
    let id = checked_instance(&instance, ctx)?;
    let class = ctx
        .borrow()
        .nodes
        .get(&id)
        .map(|node| node.class_name.clone())
        .unwrap_or_default();
    Ok(scriptable(ctx, &id, &class, &raw_key, lua)
        .map(Value::Boolean)
        .unwrap_or(Value::Nil))
}

fn thread_store(lua: &Lua) -> ThreadStore {
    if lua.app_data_ref::<ThreadStore>().is_none() {
        lua.set_app_data(ThreadStore::default());
    }
    lua.app_data_ref::<ThreadStore>()
        .expect("callback thread store installed")
        .clone()
}

fn checked_instance(value: &Value, ctx: &WorldRef) -> mlua::Result<String> {
    if !world::instance_belongs_to(value, ctx) {
        return Err(mlua::Error::RuntimeError(
            "expected Instance from this world".into(),
        ));
    }
    let id = world::instance_id(value)
        .ok_or_else(|| mlua::Error::RuntimeError("expected Instance".into()))?;
    if !ctx
        .borrow()
        .nodes
        .get(&id)
        .is_some_and(|node| !node.destroyed)
    {
        return Err(mlua::Error::RuntimeError("live Instance required".into()));
    }
    Ok(id)
}

fn callback_property(ctx: &WorldRef, id: &str, property: &str) -> mlua::Result<&'static str> {
    let class = ctx
        .borrow()
        .nodes
        .get(id)
        .map(|node| node.class_name.clone())
        .unwrap_or_default();
    match (class.as_str(), property) {
        ("BindableFunction", "OnInvoke") => Ok("OnInvoke"),
        ("RemoteFunction", "OnClientInvoke") => Ok("OnClientInvoke"),
        _ => Err(mlua::Error::RuntimeError(
            "unsupported callback property".into(),
        )),
    }
}

fn callback_setter(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    property: &str,
    function: Function,
) -> mlua::Result<()> {
    let property = callback_property(ctx, id, property)?;
    let userdata = world::instance(lua, ctx, id)?;
    let Value::UserData(userdata) = userdata else {
        return Err(mlua::Error::RuntimeError(
            "callback target is unavailable".into(),
        ));
    };
    userdata.set(property, Value::Function(function))
}

fn callback_getter(lua: &Lua, ctx: &WorldRef, id: &str, property: &str) -> mlua::Result<Value> {
    let property = callback_property(ctx, id, property)?;
    let callback = match property {
        "OnInvoke" => bindables::get_on_invoke(lua, ctx, id)?,
        "OnClientInvoke" => remotes::get_on_client_invoke(lua, ctx, id)?,
        _ => None,
    };
    Ok(callback.map(Value::Function).unwrap_or(Value::Nil))
}

fn callbacks_set(lua: &Lua, ctx: &WorldRef, args: MultiValue) -> mlua::Result<()> {
    let args: Vec<Value> = args.into_iter().collect();
    if args.len() != 3 {
        return Err(mlua::Error::RuntimeError(
            "setcallback expects Instance, property and function".into(),
        ));
    }
    let id = checked_instance(&args[0], ctx)?;
    let property = match &args[1] {
        Value::String(value) => value.to_str()?.to_string(),
        _ => {
            return Err(mlua::Error::RuntimeError(
                "callback property name must be a string".into(),
            ))
        }
    };
    let Value::Function(function) = &args[2] else {
        return Err(mlua::Error::RuntimeError(
            "callback must be a function".into(),
        ));
    };
    callback_setter(lua, ctx, &id, &property, function.clone())
}

fn callbacks_get(lua: &Lua, ctx: &WorldRef, args: MultiValue) -> mlua::Result<Value> {
    let args: Vec<Value> = args.into_iter().collect();
    if args.len() != 2 {
        return Err(mlua::Error::RuntimeError(
            "getcallback expects Instance and property".into(),
        ));
    }
    let id = checked_instance(&args[0], ctx)?;
    let property = match &args[1] {
        Value::String(value) => value.to_str()?.to_string(),
        _ => {
            return Err(mlua::Error::RuntimeError(
                "callback property name must be a string".into(),
            ))
        }
    };
    callback_getter(lua, ctx, &id, &property)
}

fn set_callback_thread(lua: &Lua, ctx: &WorldRef, args: MultiValue) -> mlua::Result<()> {
    let args: Vec<Value> = args.into_iter().collect();
    if args.len() != 3 {
        return Err(mlua::Error::RuntimeError(
            "setcallbackthread expects Instance, property and thread".into(),
        ));
    }
    let id = checked_instance(&args[0], ctx)?;
    let property = match &args[1] {
        Value::String(value) => value.to_str()?.to_string(),
        _ => {
            return Err(mlua::Error::RuntimeError(
                "callback property name must be a string".into(),
            ))
        }
    };
    let property = callback_property(ctx, &id, &property)?.to_owned();
    let Value::Thread(thread) = &args[2] else {
        return Err(mlua::Error::RuntimeError(
            "callback thread must be a coroutine".into(),
        ));
    };
    let store = thread_store(lua);
    let mut store = store.borrow_mut();
    let scope = world_key(ctx);
    let key = (scope, id, property);
    let scoped_len = store.keys().filter(|(world, _, _)| *world == scope).count();
    if !store.contains_key(&key) && scoped_len >= MAX_CALLBACK_THREADS {
        return Err(mlua::Error::RuntimeError(
            "callback thread limit exceeded".into(),
        ));
    }
    store.insert(key, thread.clone());
    Ok(())
}

fn get_callback_thread(lua: &Lua, ctx: &WorldRef, args: MultiValue) -> mlua::Result<Value> {
    let args: Vec<Value> = args.into_iter().collect();
    if args.len() != 2 {
        return Err(mlua::Error::RuntimeError(
            "getcallbackthread expects Instance and property".into(),
        ));
    }
    let id = checked_instance(&args[0], ctx)?;
    let property = match &args[1] {
        Value::String(value) => value.to_str()?.to_string(),
        _ => {
            return Err(mlua::Error::RuntimeError(
                "callback property name must be a string".into(),
            ))
        }
    };
    let property = callback_property(ctx, &id, &property)?.to_owned();
    let key = (world_key(ctx), id, property);
    Ok(thread_store(lua)
        .borrow()
        .get(&key)
        .cloned()
        .map(Value::Thread)
        .unwrap_or(Value::Nil))
}

fn instance_cache(lua: &Lua, ctx: &WorldRef) -> mlua::Result<Value> {
    let mut ids = world::cached_instance_ids(lua, ctx)?;
    ids.sort();
    let table = lua.create_table_with_capacity(ids.len(), 0)?;
    for (index, id) in ids.iter().enumerate() {
        table.raw_set(index + 1, world::instance(lua, ctx, id)?)?;
    }
    Ok(Value::Table(table))
}

fn numeric_property(ctx: &WorldRef, id: &str, property: &str) -> mlua::Result<f64> {
    let value = world::get_property(ctx, id, property)?;
    let number = value
        .as_f64()
        .or_else(|| {
            value
                .get("$number")
                .and_then(Json::as_str)
                .and_then(|s| match s {
                    "inf" => Some(f64::INFINITY),
                    "-inf" => Some(f64::NEG_INFINITY),
                    "nan" => Some(f64::NAN),
                    _ => None,
                })
        })
        .ok_or_else(|| {
            mlua::Error::RuntimeError(format!("{property} is not a number in this snapshot"))
        })?;
    if !number.is_finite() {
        return Err(mlua::Error::RuntimeError(format!(
            "{property} is non-finite in this snapshot"
        )));
    }
    Ok(number)
}

fn network_owner(lua: &Lua, ctx: &WorldRef, args: MultiValue) -> mlua::Result<bool> {
    let args: Vec<Value> = args.into_iter().collect();
    if args.len() != 1 {
        return Err(mlua::Error::RuntimeError(
            "getnetworkowner expects one BasePart".into(),
        ));
    }
    let id = checked_instance(&args[0], ctx)?;
    if !world::is_a(ctx, &id, "BasePart") {
        return Err(mlua::Error::RuntimeError("live BasePart required".into()));
    }
    let peer = numeric_property(ctx, &id, "NetworkOwnerV3")?;
    let _ = lua;
    Ok(peer >= 4.0)
}

fn simulation_radius(ctx: &WorldRef) -> mlua::Result<f64> {
    let players = ctx
        .borrow()
        .services
        .get("Players")
        .cloned()
        .ok_or_else(|| {
            mlua::Error::RuntimeError("Players service unavailable in this snapshot".into())
        })?;
    let local_player = world::get_property(ctx, &players, "LocalPlayer")?
        .get("$ref")
        .and_then(Json::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            mlua::Error::RuntimeError("LocalPlayer is unavailable in this snapshot".into())
        })?;
    numeric_property(ctx, &local_player, "SimulationRadius")
}

pub fn install(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    for name in [
        "gethiddenproperty",
        "sethiddenproperty",
        "isscriptable",
        "setscriptable",
    ] {
        let weak = Rc::downgrade(ctx);
        let function = lua.create_function(move |lua, args: MultiValue| {
            let ctx = weak
                .upgrade()
                .ok_or_else(|| mlua::Error::RuntimeError("world has been released".into()))?;
            match name {
                "gethiddenproperty" => hidden_get(lua, &ctx, args),
                "sethiddenproperty" => Ok(MultiValue::from_vec(vec![Value::Boolean(hidden_set(
                    lua, &ctx, args,
                )?)])),
                "isscriptable" => Ok(MultiValue::from_vec(vec![is_scriptable(lua, &ctx, args)?])),
                "setscriptable" => Ok(MultiValue::from_vec(vec![set_scriptability(
                    lua, &ctx, args,
                )?])),
                _ => unreachable!(),
            }
        })?;
        lua.globals().set(name, function)?;
    }
    let weak = Rc::downgrade(ctx);
    for name in [
        "setcallbackvalue",
        "setcallbackfunction",
        "setcallbackmember",
    ] {
        let weak = weak.clone();
        lua.globals().set(
            name,
            lua.create_function(move |lua, args: MultiValue| {
                callbacks_set(
                    lua,
                    &weak.upgrade().ok_or_else(|| {
                        mlua::Error::RuntimeError("world has been released".into())
                    })?,
                    args,
                )
            })?,
        )?;
    }
    for name in [
        "getcallbackvalue",
        "getcallbackfunction",
        "getcallbackmember",
    ] {
        let weak = weak.clone();
        lua.globals().set(
            name,
            lua.create_function(move |lua, args: MultiValue| {
                callbacks_get(
                    lua,
                    &weak.upgrade().ok_or_else(|| {
                        mlua::Error::RuntimeError("world has been released".into())
                    })?,
                    args,
                )
            })?,
        )?;
    }
    let weak = Rc::downgrade(ctx);
    lua.globals().set(
        "setcallbackthread",
        lua.create_function(move |lua, args: MultiValue| {
            set_callback_thread(
                lua,
                &weak
                    .upgrade()
                    .ok_or_else(|| mlua::Error::RuntimeError("world has been released".into()))?,
                args,
            )
        })?,
    )?;
    let weak = Rc::downgrade(ctx);
    lua.globals().set(
        "getcallbackthread",
        lua.create_function(move |lua, args: MultiValue| {
            get_callback_thread(
                lua,
                &weak
                    .upgrade()
                    .ok_or_else(|| mlua::Error::RuntimeError("world has been released".into()))?,
                args,
            )
        })?,
    )?;
    for name in ["getinstancelist", "getinstancecache"] {
        let weak = Rc::downgrade(ctx);
        lua.globals().set(
            name,
            lua.create_function(move |lua, args: MultiValue| {
                if !args.is_empty() {
                    return Err(mlua::Error::RuntimeError(format!(
                        "{name} takes no arguments"
                    )));
                }
                instance_cache(
                    lua,
                    &weak.upgrade().ok_or_else(|| {
                        mlua::Error::RuntimeError("world has been released".into())
                    })?,
                )
            })?,
        )?;
    }
    for name in ["getnetworkowner", "isnetworkowner"] {
        let weak = Rc::downgrade(ctx);
        lua.globals().set(
            name,
            lua.create_function(move |lua, args: MultiValue| {
                Ok(network_owner(
                    lua,
                    &weak.upgrade().ok_or_else(|| {
                        mlua::Error::RuntimeError("world has been released".into())
                    })?,
                    args,
                )?)
            })?,
        )?;
    }
    for name in ["getsimulationradius", "getsimradius"] {
        let weak = Rc::downgrade(ctx);
        lua.globals().set(
            name,
            lua.create_function(move |_, args: MultiValue| {
                if !args.is_empty() {
                    return Err(mlua::Error::RuntimeError(format!(
                        "{name} takes no arguments"
                    )));
                }
                Ok(simulation_radius(&weak.upgrade().ok_or_else(|| {
                    mlua::Error::RuntimeError("world has been released".into())
                })?)?)
            })?,
        )?;
    }
    let weak = Rc::downgrade(ctx);
    lua.globals().set(
        "clonereference",
        lua.create_function(move |lua, value: Value| {
            let ctx = weak
                .upgrade()
                .ok_or_else(|| mlua::Error::RuntimeError("world has been released".into()))?;
            let id = checked_instance(&value, &ctx)?;
            world::instance(lua, &ctx, &id)
        })?,
    )?;
    super::instance_cache::install(lua, ctx)?;
    Ok(())
}
