//! Typed Humanoid observations and ordered received state transitions.
use super::{
    movement, property_events, signals,
    world::{self, WorldRef},
};
use mlua::{Lua, MultiValue, Value};
use serde_json::Value as Json;
use std::collections::HashMap;
fn err(text: &str) -> mlua::Error {
    mlua::Error::RuntimeError(text.into())
}
pub(super) fn decode(value: &Json) -> mlua::Result<i32> {
    let body = value.get("$enum");
    let number = body
        .map_or(value, |v| &v["value"])
        .as_f64()
        .filter(|v| v.is_finite() && v.fract() == 0. && (0.0..=18.0).contains(v))
        .ok_or_else(|| err("invalid HumanoidStateType value"))? as i32;
    let name = movement::HUMANOID_STATES
        .iter()
        .find(|(_, v)| *v == number)
        .map(|(name, _)| *name)
        .ok_or_else(|| err("invalid HumanoidStateType value"))?;
    if let Some(body) = body {
        if body.get("type").is_some_and(|t| t != "HumanoidStateType")
            || body.get("name").is_some_and(|n| n != name)
        {
            return Err(err("HumanoidStateType enum metadata mismatch"));
        }
    }
    Ok(number)
}
pub(super) fn observed(ctx: &WorldRef, id: &str) -> mlua::Result<Option<i32>> {
    if let Some(value) = ctx
        .borrow()
        .nodes
        .get(id)
        .and_then(|n| n.local_overrides.get("__HumanoidStateFeedback"))
        .cloned()
    {
        return decode(&value).map(Some);
    }
    world::get_property(ctx, id, "HumanoidStateType")
        .ok()
        .map(|v| decode(&v))
        .transpose()
}
fn active_event(value: i32) -> Option<&'static str> {
    match value {
        0 => Some("FallingDown"),
        1 => Some("Ragdoll"),
        2 => Some("GettingUp"),
        3 => Some("Jumping"),
        5 => Some("FreeFalling"),
        11 => Some("Strafing"),
        14 => Some("PlatformStanding"),
        _ => None,
    }
}
pub(super) fn emit(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    old: Option<i32>,
    next: i32,
) -> mlua::Result<()> {
    let Some(old) = old.filter(|old| *old != next) else {
        return Ok(());
    };
    signals::fire(
        lua,
        ctx,
        id,
        "StateChanged",
        MultiValue::from_vec(vec![
            movement::state_item(lua, old)?,
            movement::state_item(lua, next)?,
        ]),
    )?;
    for (state, active) in [(old, false), (next, true)] {
        if let Some(event) = active_event(state) {
            signals::fire(
                lua,
                ctx,
                id,
                event,
                MultiValue::from_vec(vec![Value::Boolean(active)]),
            )?;
        }
    }
    Ok(())
}
#[derive(Default)]
pub(super) struct Receipt(HashMap<usize, (String, Option<i32>, i32)>);
impl Receipt {
    pub(super) fn publish(&mut self, lua: &Lua, ctx: &WorldRef, index: usize) -> mlua::Result<()> {
        if let Some((id, old, next)) = self.0.remove(&index) {
            emit(lua, ctx, &id, old, next)?;
        }
        Ok(())
    }
}
pub(super) fn prepare(
    ctx: &WorldRef,
    actions: &[Json],
    previous: Option<u64>,
) -> mlua::Result<Receipt> {
    let fresh = |a: &Json| previous.is_none_or(|s| a["sequence"].as_u64().unwrap_or(0) > s);
    let mut receipt = Receipt::default();
    if !actions.iter().any(|a| {
        fresh(a)
            && property_events::relevant(a)
            && (a["fields"].get("HumanoidStateType").is_some()
                || a.get("property")
                    .or_else(|| a.get("key"))
                    .and_then(Json::as_str)
                    == Some("HumanoidStateType"))
    }) {
        return Ok(receipt);
    }
    let mut states = HashMap::<String, Option<i32>>::new();
    let mut created = HashMap::<&str, &str>::new();
    for (index, a) in actions.iter().enumerate().filter(|(_, a)| fresh(a)) {
        if a["kind"] == "create" {
            let node = &a["node"];
            if let (Some(id), Some(class)) = (node["id"].as_str(), node["class_name"].as_str()) {
                created.insert(id, class);
                states.remove(id);
                if class == "Humanoid" {
                    states.insert(
                        id.into(),
                        node["properties"]
                            .get("HumanoidStateType")
                            .map(decode)
                            .transpose()?,
                    );
                }
            }
            continue;
        }
        if !property_events::relevant(a) {
            continue;
        }
        let Some(id) = a["id"].as_str() else { continue };
        if !created.get(id).map_or_else(
            || world::is_a(ctx, id, "Humanoid"),
            |class| *class == "Humanoid",
        ) {
            continue;
        }
        let fields = property_events::fields(a);
        let Some(value) = fields.get("HumanoidStateType") else {
            continue;
        };
        let next = decode(value)?;
        if !states.contains_key(id) {
            states.insert(id.into(), observed(ctx, id)?);
        }
        let old = states.insert(id.into(), Some(next)).flatten();
        if old.is_some_and(|old| old != next) {
            receipt.0.insert(index, (id.into(), old, next));
        }
    }
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn all_state_items_use_the_common_enum_codec_and_lookup() {
        let lua = Lua::new();
        super::super::enums::install(&lua).unwrap();
        for (name, value) in movement::HUMANOID_STATES {
            let wire = json!({"$enum":{"type":"HumanoidStateType","name":name,"value":value}});
            assert_eq!(decode(&json!(value)).unwrap(), value);
            assert_eq!(decode(&wire).unwrap(), value);
            let item = movement::state_item(&lua, value).unwrap();
            assert_eq!(super::super::values::to_json(&lua, &item).unwrap(), wire);
            let decoded = super::super::values::from_json(&lua, &wire).unwrap();
            lua.globals().set("decoded", decoded).unwrap();
            lua.globals().set("expected", item).unwrap();
            assert!(lua
                .load("return rawequal(decoded,expected)")
                .eval::<bool>()
                .unwrap());
        }
        lua.load(
            r#"
            local e=Enum.HumanoidStateType
            local found=false
            for _,entry in ipairs(Enum:GetEnums()) do if entry==e then found=true end end
            assert(found and #e:GetEnumItems()==17)
            for _,item in ipairs(e:GetEnumItems()) do
                assert(e:FromName(item.Name)==item and e:FromValue(item.Value)==item)
            end
        "#,
        )
        .exec()
        .unwrap();
    }
    #[test]
    fn invalid_states_cannot_wrap_or_impersonate_another_enum() {
        for value in [
            json!(-1),
            json!(9),
            json!(17),
            json!(19),
            json!(8.5),
            json!(4294967304u64),
            json!(i64::MAX),
            json!("8"),
            Json::Null,
            json!({"$enum":{"type":"Material","name":"Running","value":8}}),
            json!({"$enum":{"type":"HumanoidStateType","name":"Jumping","value":8}}),
        ] {
            assert!(decode(&value).is_err(), "must reject {value}");
        }
    }
}
