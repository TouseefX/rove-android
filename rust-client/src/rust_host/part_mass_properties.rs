//! Shared mass resolution and bounded per-receipt observations.
use super::*;
use std::collections::BTreeMap;
const KEYS: [&str; 7] = [
    "Mass",
    "Size",
    "Shape",
    "Material",
    "MaterialVariant",
    "CustomPhysicalProperties",
    "CurrentPhysicalProperties",
];
pub(crate) fn is_input(key: &str) -> bool {
    KEYS.contains(&key)
}

pub(crate) struct Observation {
    class: String,
    world: Option<Weak<RefCell<world::World>>>,
    received: bool,
    local: u8,
    values: [Option<Json>; 7],
    revisions: [u64; 7],
}
impl Observation {
    pub(crate) fn capture(ctx: &WorldRef, id: &str) -> mlua::Result<Self> {
        if !ctx.borrow().nodes.is_live(id) || !world::is_a(ctx, id, "BasePart") {
            return Err(err("Mass requires a live BasePart"));
        }
        let (class, received, local) = {
            let w = ctx.borrow();
            let n = w.nodes.get(id).unwrap();
            (
                n.class_name.clone(),
                n.server_owned,
                KEYS.iter().enumerate().fold(0, |mask, (i, key)| {
                    mask | if n.local_overrides.contains_key(*key) {
                        1 << i
                    } else {
                        0
                    }
                }),
            )
        };
        Ok(Self {
            class,
            world: Some(Rc::downgrade(ctx)),
            received,
            local,
            values: std::array::from_fn(|i| world::get_property(ctx, id, KEYS[i]).ok()),
            revisions: std::array::from_fn(|i| {
                world::get_property_revision(ctx, id, KEYS[i]).unwrap_or(0)
            }),
        })
    }
    pub(crate) fn created(node: &Json) -> Self {
        let class = node["class_name"].as_str().unwrap_or("").to_owned();
        let received = node["server_owned"].as_bool().unwrap_or(true);
        let values = std::array::from_fn(|i| {
            node["properties"].get(KEYS[i]).cloned().or_else(|| {
                (!received)
                    .then(|| {
                        world::constructor_default(&class, KEYS[i])
                            .ok()
                            .map(|v| v.0)
                    })
                    .flatten()
            })
        });
        Self {
            class,
            world: None,
            received,
            local: 0,
            values,
            revisions: std::array::from_fn(|i| {
                node["property_revisions"][KEYS[i]].as_u64().unwrap_or(0)
            }),
        }
    }
    pub(crate) fn update(&mut self, fields: &BTreeMap<&str, &Json>, sequence: Option<u64>) {
        for (i, key) in KEYS.iter().enumerate() {
            // Schema-typed wire size is projected into Size before the host.
            // An untyped raw field must not change a public mass observation.
            let value = fields.get(key).copied();
            if let Some(value) = value {
                self.values[i] = Some(value.clone());
                if let Some(sequence) = sequence {
                    self.revisions[i] = sequence;
                    self.local &= !(1 << i);
                } else {
                    self.local |= 1 << i;
                }
            }
        }
    }
    fn value(&self, i: usize) -> mlua::Result<&Json> {
        self.values[i]
            .as_ref()
            .ok_or_else(|| err(format!("{} is unavailable", KEYS[i])))
    }
    pub(crate) fn material_report_current(&self, sequence: u64) -> bool {
        // Per-part custom data bypasses material inheritance entirely.
        if self.values[5].as_ref().is_some_and(|v| !v.is_null()) {
            return true;
        }
        let Some(ctx) = self.world.as_ref().and_then(Weak::upgrade) else {
            return true;
        };
        let material = self.values[3]
            .as_ref()
            .and_then(|v| v.get("$enum"))
            .filter(|e| e["type"] == "Material")
            .and_then(|e| e["value"].as_i64());
        let variant = self.values[4].as_ref().and_then(Json::as_str);
        crate::rust_host::material_history::report_current(&ctx, material, variant, sequence)
    }
    pub(crate) fn read(&self) -> mlua::Result<f64> {
        let current_report = self.received
            && self.local & 0b1111110 == 0
            && self.revisions[0] >= *self.revisions[1..].iter().max().unwrap()
            && self.material_report_current(self.revisions[0]);
        if current_report {
            if let Some(value) = self.values[0].as_ref().filter(|v| !v.is_null()) {
                return value
                    .as_f64()
                    .filter(|v| v.is_finite() && *v >= 0. && *v <= f32::MAX as f64)
                    .ok_or_else(|| err("received Mass must be finite and nonnegative"));
            }
        }
        let size = read_size(self.value(1)?)?;
        let mut volume = size[0] * size[1] * size[2];
        match read_shape(&self.class, self.values[2].as_ref())? {
            0 => {
                let d = size.into_iter().fold(f64::INFINITY, f64::min);
                volume = d * d * d * std::f64::consts::PI / 6.;
            }
            1 => {}
            2 => {
                let d = size[1].min(size[2]);
                volume = size[0] * d * d * std::f64::consts::PI / 4.;
            }
            3 => volume *= 0.5,
            4 => volume /= 3.,
            _ => unreachable!(),
        }
        let mass = volume * self.density()?;
        if !mass.is_finite() || mass <= 0. || mass > f32::MAX as f64 {
            return Err(err("Mass exceeds finite positive range"));
        }
        Ok(mass)
    }
    fn observed_physical(&self) -> mlua::Result<Option<[f64; 6]>> {
        if let Some(custom) = self.values[5].as_ref().filter(|v| !v.is_null()) {
            return canonical_physical(custom).map(Some);
        }
        if !self.received {
            if let Some(ctx) = self.world.as_ref().and_then(Weak::upgrade) {
                let material = self
                    .value(3)?
                    .get("$enum")
                    .filter(|e| e["type"] == "Material")
                    .and_then(|e| e["value"].as_i64())
                    .ok_or_else(|| err("material physical properties are unavailable"))?;
                let variant = self.values[4]
                    .as_ref()
                    .map(|v| {
                        v.as_str()
                            .ok_or_else(|| err("MaterialVariant must be a string"))
                    })
                    .transpose()?
                    .unwrap_or("");
                if let Some(value) =
                    crate::rust_host::material_service::local_physical(&ctx, material, variant)?
                {
                    return canonical_physical(&value).map(Some);
                }
            }
            // Authored parts follow their current catalog and defaults. A
            // previous effective snapshot cannot override a later catalog edit.
            return Ok(None);
        }

        let current = self.local & 0b111000 == 0
            && self.revisions[6] >= *self.revisions[3..=5].iter().max().unwrap()
            && self.material_report_current(self.revisions[6]);
        if current {
            if let Some(value) = self.values[6].as_ref().filter(|v| !v.is_null()) {
                return canonical_physical(value).map(Some);
            }
        }
        Ok(None)
    }
    pub(crate) fn physical(&self) -> mlua::Result<[f64; 6]> {
        if let Some(value) = self.observed_physical()? {
            return Ok(value);
        }
        if self.received {
            let ctx = self
                .world
                .as_ref()
                .and_then(Weak::upgrade)
                .ok_or_else(|| err("effective received physical properties are unavailable"))?;
            // A missing property is unknown, not the observed nil that permits
            // material inheritance. This also prevents invented defaults while
            // the part or MaterialService is still arriving.
            if !self.value(5)?.is_null() {
                return Err(err("received CustomPhysicalProperties must be known"));
            }
            let material = self
                .value(3)?
                .get("$enum")
                .filter(|e| e["type"] == "Material")
                .and_then(|e| e["value"].as_i64())
                .ok_or_else(|| err("material physical properties are unavailable"))?;
            let variant = self
                .value(4)?
                .as_str()
                .ok_or_else(|| err("MaterialVariant must be a string"))?;
            if let Some(value) =
                crate::rust_host::material_service::received_physical(&ctx, material, variant)?
            {
                return canonical_physical(&value);
            }
        }
        if self.world.is_none()
            && self.values[4]
                .as_ref()
                .and_then(Json::as_str)
                .is_some_and(|v| !v.is_empty())
        {
            return Err(err("custom material physical density is unavailable"));
        }
        let material = self
            .value(3)?
            .get("$enum")
            .filter(|e| e["type"] == "Material")
            .and_then(|e| e["value"].as_i64())
            .ok_or_else(|| err("material physical properties are unavailable"))?;
        let physical =
            crate::engine_physical_properties::PhysicalPropertiesValue::from_material(material)
                .ok_or_else(|| err("material physical properties are unavailable"))?;
        Ok(physical.components().map(f64::from))
    }
    fn density(&self) -> mlua::Result<f64> {
        Ok(self.physical()?[0])
    }
}
pub(crate) fn assembly_inputs_current(
    ctx: &WorldRef,
    id: &str,
    sequence: u64,
) -> mlua::Result<bool> {
    let observation = Observation::capture(ctx, id)?;
    Ok(observation
        .revisions
        .iter()
        .all(|revision| *revision <= sequence)
        && observation.material_report_current(sequence))
}

pub(crate) fn read(ctx: &WorldRef, id: &str) -> mlua::Result<f64> {
    Observation::capture(ctx, id)?.read()
}
fn read_size(value: &Json) -> mlua::Result<[f64; 3]> {
    point(value)
}
pub(super) fn size(ctx: &WorldRef, id: &str) -> mlua::Result<[f64; 3]> {
    read_size(&world::get_property(ctx, id, "Size")?)
}
fn read_shape(class: &str, value: Option<&Json>) -> mlua::Result<i64> {
    match class {
        "WedgePart" => Ok(3),
        "CornerWedgePart" => Ok(4),
        "Seat" | "VehicleSeat" => Ok(1),
        "Part" | "SpawnLocation" => value
            .and_then(|v| v.get("$enum"))
            .filter(|e| e["type"] == "PartType")
            .and_then(|e| e["value"].as_i64())
            .filter(|v| (0..=2).contains(v))
            .ok_or_else(|| err("mass geometry requires a known PartType")),
        _ => Err(err("mass geometry is unavailable for this part class")),
    }
}
pub(super) fn shape(ctx: &WorldRef, id: &str) -> mlua::Result<i64> {
    let class = ctx
        .borrow()
        .nodes
        .class_name(id)
        .map(str::to_owned)
        .ok_or_else(|| err("part unavailable"))?;
    read_shape(&class, world::get_property(ctx, id, "Shape").ok().as_ref())
}
/// Received fields must be valid observations; do not clamp corrupt snapshots.
fn canonical_physical(value: &Json) -> mlua::Result<[f64; 6]> {
    let values = value
        .get("$physical_properties")
        .unwrap_or(value)
        .as_array()
        .filter(|a| matches!(a.len(), 5 | 6))
        .ok_or_else(|| err("physical properties require five or six components"))?;
    let low = [0.0001f32 as f64, 0., 0., 0., 0., 0.];
    let high = [100., 2., 1., 100., 100., 1.];
    let mut numbers = [0.; 6];
    for (i, value) in values.iter().enumerate() {
        numbers[i] = value
            .as_f64()
            .filter(|n| n.is_finite() && *n >= low[i] && *n <= high[i])
            .ok_or_else(|| err("physical property component is outside its finite range"))?;
    }
    // Preserve valid observed precision for mass comparisons. Lua's value codec
    // performs the established f32 conversion when exposing the userdata.
    Ok(numbers)
}
pub(crate) fn effective(ctx: &WorldRef, id: &str) -> mlua::Result<Json> {
    Ok(json!({"$physical_properties":Observation::capture(ctx, id)?.physical()?}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{engine_world_projection::ProjectionContext, engine_world_view::SharedWorldView};
    use rove_client_core::{schema::Schema, world::WorldStore};
    use std::collections::HashMap;
    #[test]
    fn mass_queries_leave_unrelated_shared_parts_unmaterialized() {
        let mut nodes = vec![
            json!({"id":"game","class_name":"DataModel","name":"game"}),
            json!({"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game"}),
            json!({"id":"p","class_name":"Part","name":"p","parent":"ws","properties":{
                "Size":{"$vector3":[2,2,2]},"Shape":{"$enum":{"type":"PartType","value":1}},
                "CustomPhysicalProperties":{"$physical_properties":[1,0.3,0.5,1,1]},
                "Anchored":false,"Massless":false
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
        let lua = Lua::new();
        crate::rust_host::signals::install(&lua, &ctx).unwrap();
        crate::rust_host::signals::connect_owned(
            &lua,
            &ctx,
            "p",
            "property:Mass",
            lua.create_function(|_, (): ()| Ok(())).unwrap(),
            "mass-test",
            false,
        )
        .unwrap();
        super::mass_events::refresh(&lua).unwrap();
        let actions =
            [json!({"kind":"set","id":"p","sequence":1,"fields":{"Size":{"$vector3":[4,2,2]}}})];
        let _plan = crate::rust_host::property_events::prepare(&lua, &ctx, &actions, Some(0));
        assert_eq!(read(&ctx, "p").unwrap(), 8.);
        assert_eq!(
            mass_assembly_value(&ctx, "p", "AssemblyMass").unwrap(),
            json!(8.)
        );
        let w = ctx.borrow();
        for i in 0..1000 {
            assert!(w.nodes.peek(&format!("untouched{i}")).is_none());
        }
        assert!(w.effects.get("mutations").is_none());
    }
}
