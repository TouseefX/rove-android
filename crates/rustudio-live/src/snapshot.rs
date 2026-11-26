//! rove's world snapshot JSON, parsed and value-converted.
//!
//! The shape is rove `client-core`'s: `WorldStore::from_snapshot`
//! (`rove/client-core/src/world.rs:883`) reads `version`, `sequence`, every
//! other top-level key as metadata, and `nodes`; each node is a
//! `NodeRecord::parse` of `id`, `class_name`, `name` (optional), `parent`
//! (`null` for a service at the root), `properties` and `children`. This crate
//! parses the same text directly instead of depending on rove, because the
//! ported decoder (M2) writes into the same types and the two must not drift:
//! `WorldStore` is the reference these tests pin.
//!
//! **Values.** rove's replication carries `$`-tagged JSON (`$vector3`,
//! `$cframe`, `$ref`, `$enum`, `$udim`, `$udim2`, `$color3`, `$rect`,
//! `$brickcolor`, `$number_range`, `$color_sequence`, `$bytes_hex`,
//! `$shared_string`, `$number`, `$datetime`, `$faces`). A place file's shapes
//! (Rojo/Argon: bare arrays, bare scalars) are a *different* convention that
//! `rustudio_core::project_values` already reads against `api-dump.json`, so
//! this module does not duplicate it: the tags rove defines are read here and
//! map onto the same `Variant`s, and anything untagged (or a tag whose payload
//! is exactly a place-file shape, `$number_sequence` / `$color_sequence` /
//! `$font`) goes through `project_values::resolve`, so the property's own type
//! comes from `api-dump.json` in one place.
//!
//! A tag that cannot be honoured is an **error naming the property**, never a
//! silent default: a defaulted value is exactly what `docs/ENGINE-GAPS.md`
//! counts as `default-only`.

use rbx_dom_weak::types::{
    Color3, Color3uint8, CFrame, Enum, Faces, Matrix3, Rect, UDim, UDim2, Variant,
    Vector2, Vector3,
};
use serde_json::{json, Value as J};

use crate::BridgeError;

/// The one snapshot version this crate reads. rove's `WorldStore` requires the
/// same and treats anything else as an error rather than guessing.
pub const SUPPORTED_VERSION: u64 = 1;

/// A snapshot as parsed, before any DataModel work: nodes in the order the
/// snapshot lists them (which need not be parent-first).
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub version: u64,
    /// The receipt sequence the snapshot was taken at (0 when absent).
    pub sequence: u64,
    /// The rest of the top level (`job_id`, `local_player`, `services`,
    /// `class_parents`, …), kept as rove keeps it: session metadata, not
    /// DataModel content. `local_player` is the entry the builder reads.
    pub metadata: J,
    pub nodes: Vec<Node>,
}

/// One instance in the snapshot.
#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub class_name: String,
    /// rove omits the name when it is the class's default; the builder gives
    /// the class's own name then (`doc::default_instance_name`).
    pub name: Option<String>,
    /// `None`: a service (or whatever else) at the DataModel root.
    pub parent: Option<String>,
    pub properties: J,
    pub children: Vec<String>,
}

impl Snapshot {
    /// The local player's rove id, from the top-level `local_player`.
    pub fn local_player(&self) -> Option<&str> {
        self.metadata.get("local_player").and_then(J::as_str)
    }
}

/// Parses a snapshot. Unknown top-level keys are metadata, as in rove; a node
/// that lacks what identifies it, or repeats an id, fails here — before any
/// instance exists to leave half-built.
pub fn parse(text: &str) -> Result<Snapshot, String> {
    let root: J = serde_json::from_str(text).map_err(|e| format!("world snapshot is not JSON: {e}"))?;
    let object = root.as_object().ok_or("world snapshot must be an object")?;
    let version = object
        .get("version")
        .and_then(J::as_u64)
        .ok_or("world snapshot has no version")?;
    if version != SUPPORTED_VERSION {
        return Err(format!(
            "world snapshot has version {version}; this build reads version {SUPPORTED_VERSION}"
        ));
    }
    let sequence = match object.get("sequence") {
        None => 0,
        Some(v) => v.as_u64().ok_or("world snapshot sequence must be an unsigned integer")?,
    };
    let nodes = object
        .get("nodes")
        .and_then(J::as_array)
        .ok_or("world snapshot has no nodes array")?;
    let mut out = Vec::with_capacity(nodes.len());
    let mut seen = std::collections::HashSet::with_capacity(nodes.len());
    for (i, node) in nodes.iter().enumerate() {
        let node = node.as_object().ok_or_else(|| format!("node {i} is not an object"))?;
        let id = node
            .get("id")
            .and_then(J::as_str)
            .ok_or_else(|| format!("node {i} has no string id"))?
            .to_string();
        if !seen.insert(id.clone()) {
            return Err(format!("node {i}: duplicate id {id:?}"));
        }
        let class_name = node
            .get("class_name")
            .and_then(J::as_str)
            .ok_or_else(|| format!("node {id:?} has no class_name"))?;
        // rove's slots: a key that is *absent* and a key that is *null* mean
        // different things (`world.rs` parse_name_slot / parse_parent_slot),
        // so both are kept as absence here and a wrong type is refused.
        let name = match node.get("name") {
            None | Some(J::Null) => None,
            Some(J::String(s)) => Some(s.clone()),
            Some(_) => return Err(format!("node {id:?} has a name that is not a string or null")),
        };
        let parent = match node.get("parent") {
            None | Some(J::Null) => None,
            Some(J::String(s)) => Some(s.clone()),
            // rove's `WorldStore` writes a parent as a `$ref` object as well
            // as a plain string; both are the same edge.
            Some(J::Object(object)) if object.len() == 1 => match object.get("$ref").and_then(J::as_str) {
                Some(id) => Some(id.to_string()),
                None => return Err(format!("node {id:?} has a parent $ref that is not a string")),
            },
            Some(_) => return Err(format!(
                "node {id:?} has a parent that is not a string, a $ref object, or null"
            )),
        };
        let properties = node.get("properties").cloned().unwrap_or_else(|| J::Object(Default::default()));
        if !properties.is_object() {
            return Err(format!("node {id:?} has properties that are not an object"));
        }
        let children = match node.get("children") {
            None => Vec::new(),
            Some(J::Array(items)) => items
                .iter()
                .map(|c| {
                    c.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| format!("node {id:?} has a child that is not an id"))
                })
                .collect::<Result<_, _>>()?,
            Some(_) => return Err(format!("node {id:?} has children that are not an array")),
        };
        out.push(Node {
            id,
            class_name: class_name.to_string(),
            name,
            parent,
            properties,
            children,
        });
    }
    let mut metadata = object.clone();
    for key in ["version", "sequence", "nodes"] {
        metadata.remove(key);
    }
    Ok(Snapshot { version, sequence, metadata: J::Object(metadata), nodes: out })
}

// ---------------------------------------------------------------- the values

/// rove's `$`-tagged JSON → `rbx_types::Variant`, once per property.
///
/// A converter is per world so its counters mean something for that world; none
/// of it runs per frame.
#[derive(Debug, Default)]
pub struct ValueConverter {
    /// Values that went through a `$` tag.
    pub converted: usize,
    /// Values with no tag: read as a place-file value of the property's own type.
    pub scalar: usize,
    /// What could not be honoured, by property (also in the returned error).
    pub refused: Vec<String>,
}

impl ValueConverter {
    /// One property value. `class`/`property` are for the error text and for
    /// asking `api-dump.json` what type the property has.
    pub fn value(&mut self, property: &str, class: &str, v: &J) -> Result<Variant, String> {
        if let Some((tag, inner)) = tagged(v) {
            self.converted += 1;
            match self.tagged(tag, inner, property, class) {
                Ok(v) => return Ok(v),
                Err(detail) => {
                    self.refused.push(detail.clone());
                    return Err(detail);
                }
            }
        }
        self.scalar += 1;
        via_place_value(class, property, v)
    }

    fn tagged(&mut self, tag: &str, inner: &J, property: &str, class: &str) -> Result<Variant, String> {
        let ctx = || format!("(for {class}.{property})");
        let nums = |n: usize| -> Result<Vec<f64>, String> {
            let a = inner
                .as_array()
                .filter(|a| a.len() == n)
                .ok_or_else(|| format!("{tag} wants {n} numbers {}", ctx()))?;
            a.iter()
                .map(|x| x.as_f64().ok_or_else(|| format!("{tag}: {x} is not a number {}", ctx())))
                .collect()
        };
        let f32s = |n: usize| -> Result<Vec<f32>, String> {
            Ok(nums(n)?.into_iter().map(|x| x as f32).collect())
        };
        Ok(match tag {
            "$vector3" => {
                let a = f32s(3)?;
                Variant::Vector3(Vector3::new(a[0], a[1], a[2]))
            }
            "$vector2" => {
                let a = f32s(2)?;
                Variant::Vector2(Vector2::new(a[0], a[1]))
            }
            // rove's rotation is row-major: `rotation[row * 3 + column]`
            // (`rove/client-core/src/physics.rs` indexes it that way, and
            // validates it as a right-handed orthonormal matrix). rbx_types
            // stores `Matrix3` as its three *columns*, so the read below is the
            // transpose — for the identity, which is what a place's parts
            // mostly carry, it is indistinguishable; the crate's tests pin a
            // 90° rotation so the convention cannot drift.
            "$cframe" => {
                let position = inner
                    .get("position")
                    .and_then(J::as_array)
                    .filter(|a| a.len() == 3)
                    .ok_or_else(|| format!("$cframe wants position [x,y,z] {}", ctx()))?;
                let rotation = inner
                    .get("rotation")
                    .and_then(J::as_array)
                    .filter(|a| a.len() == 9)
                    .ok_or_else(|| format!("$cframe wants rotation [r00..r22], 9 numbers {}", ctx()))?;
                let p = |a: &Vec<J>, i: usize| a[i].as_f64().unwrap_or(0.0) as f32;
                let m = Matrix3::new(
                    Vector3::new(p(rotation, 0), p(rotation, 3), p(rotation, 6)),
                    Vector3::new(p(rotation, 1), p(rotation, 4), p(rotation, 7)),
                    Vector3::new(p(rotation, 2), p(rotation, 5), p(rotation, 8)),
                );
                Variant::CFrame(CFrame::new(
                    Vector3::new(p(position, 0), p(position, 1), p(position, 2)),
                    m,
                ))
            }
            "$udim" => Variant::UDim(UDim::new(
                inner.get("scale").and_then(J::as_f64).unwrap_or(0.0) as f32,
                inner.get("offset").and_then(J::as_i64).unwrap_or(0) as i32,
            )),
            "$udim2" => {
                let read = |key: &str| -> (f32, i32) {
                    let part = inner.get(key);
                    let scale = part
                        .and_then(|u| u.get("scale"))
                        .and_then(J::as_f64)
                        .unwrap_or(0.0) as f32;
                    let offset = part
                        .and_then(|u| u.get("offset"))
                        .and_then(J::as_i64)
                        .unwrap_or(0) as i32;
                    (scale, offset)
                };
                let (xs, xo) = read("x");
                let (ys, yo) = read("y");
                Variant::UDim2(UDim2::new(UDim::new(xs, xo), UDim::new(ys, yo)))
            }
            "$color3" => {
                let a = f32s(3)?;
                Variant::Color3(Color3::new(a[0], a[1], a[2]))
            }
            // rove sends a Color3uint8 as 0–1 floats (the `$color3` shape);
            // an integer 0–255 triple is accepted too.
            "$color3uint8" => {
                let a = nums(3)?;
                let byte = |x: f64| {
                    if x <= 1.0 && x >= 0.0 {
                        (x * 255.0).round() as u8
                    } else {
                        x.round().clamp(0.0, 255.0) as u8
                    }
                };
                Variant::Color3uint8(Color3uint8::new(byte(a[0]), byte(a[1]), byte(a[2])))
            }
            "$rect" => {
                let a = f32s(4)?;
                Variant::Rect(Rect::new(Vector2::new(a[0], a[1]), Vector2::new(a[2], a[3])))
            }
            // rove's replication writes `{"$brickcolor": number}`; a place
            // file and a snapshot's name form are read by the same reader
            // rather than a second one here (`BrickColor` has no `to_number`,
            // and the number form is the one that arrives on the wire).
            "$brickcolor" => via_place_value(class, property, inner)?,
            // Payload shapes a place file uses too: one reader, not two.
            "$number_range" | "$number_sequence" | "$color_sequence" | "$font" | "$axes"
            | "$physical_properties" => via_place_value(class, property, inner)?,
            "$faces" => {
                let mut bits = 0u8;
                let pairs = [
                    ("Right", Faces::RIGHT),
                    ("Top", Faces::TOP),
                    ("Back", Faces::BACK),
                    ("Left", Faces::LEFT),
                    ("Bottom", Faces::BOTTOM),
                    ("Front", Faces::FRONT),
                ];
                match inner {
                    // rove's wire form: {"$faces": 63} — one byte, the bit
                    // per face (replication.rs writes `r.u8()`).
                    J::Number(_) => {
                        bits = inner
                            .as_u64()
                            .filter(|b| *b <= u64::from(u8::MAX))
                            .ok_or_else(|| format!("$faces wants one byte, got {inner} {}", ctx()))?
                            as u8;
                    }
                    // A named form: {"Top": true, …}, as a place file spells it.
                    J::Object(o) => {
                        for (name, on) in o {
                            let bit = pairs
                                .iter()
                                .find(|(n, _)| n == name)
                                .map(|(_, b)| *b)
                                .ok_or_else(|| format!("$faces names {name:?}, which is not a face {}", ctx()))?;
                            if on.as_bool().unwrap_or(false) {
                                bits |= bit.bits();
                            }
                        }
                    }
                    // A place file's list form is read by project_values.
                    J::Array(_) => return via_place_value(class, property, inner),
                    _ => return Err(format!("$faces wants a byte, an object or a list of faces {}", ctx())),
                }
                // from_bits is a Result, not an Option: a bit outside the six
                // faces is a wrong world, said by name.
                Variant::Faces(Faces::from_bits(bits).map_err(|_| format!("$faces has an invalid bit set {}: {bits}", ctx()))?)
            }
            "$enum" => match inner {
                J::Number(_) => Variant::Enum(Enum::from_u32(
                    inner.as_f64().ok_or_else(|| format!("$enum wants a number {}", ctx()))? as u32,
                )),
                J::String(_) => via_place_value(class, property, inner)?,
                _ => return Err(format!("$enum wants a number or an item name {}", ctx())),
            },
            "$number" => Variant::Float64(
                inner.as_f64().ok_or_else(|| format!("$number wants a number {}", ctx()))?,
            ),
            "$string" | "$shared_string" | "$content" => Variant::String(
                inner
                    .as_str()
                    .ok_or_else(|| format!("{tag} wants a string {}", ctx()))?
                    .to_string(),
            ),
            "$bytes_hex" => {
                let hex = inner
                    .as_str()
                    .filter(|s| s.len() % 2 == 0)
                    .ok_or_else(|| format!("$bytes_hex wants an even-length hex string {}", ctx()))?;
                let mut bytes = Vec::with_capacity(hex.len() / 2);
                for pair in hex.as_bytes().chunks(2) {
                    let pair = std::str::from_utf8(pair).map_err(|_| format!("$bytes_hex is not hex {}", ctx()))?;
                    bytes.push(
                        u8::from_str_radix(pair, 16)
                            .map_err(|_| format!("$bytes_hex is not hex {}", ctx()))?,
                    );
                }
                Variant::BinaryString(rbx_dom_weak::types::BinaryString::from(bytes))
            }
            // Not converted yet, and said so rather than defaulted: a
            // DateTime property is not something a world is drawn with.
            "$datetime" => return Err(format!("$datetime is not converted yet {}", ctx())),
            other => return Err(format!("no conversion for {other} {}", ctx())),
        })
    }
}

/// The one `$` key of a tagged value, and its payload.
fn tagged(v: &J) -> Option<(&str, &J)> {
    let object = v.as_object()?;
    if object.len() != 1 {
        return None;
    }
    let (key, inner) = object.iter().next()?;
    key.starts_with('$').then(|| (key.as_str(), inner))
}

/// A value read as a place-file value of the property's own type, through
/// `rustudio_core::project_values` (the one reader of `api-dump.json`).
fn via_place_value(class: &str, property: &str, v: &J) -> Result<Variant, String> {
    rustudio_core::project_values::resolve(class, property, v)
        .map(|(_name, variant)| variant)
        .map_err(|detail| format!("{detail} (from {})", short(v)))
}

/// A value for an error line: a big blob does not belong in a log.
fn short(v: &J) -> String {
    let s = v.to_string();
    if s.len() <= 120 {
        s
    } else {
        format!("{}…", &s[..120])
    }
}

/// One property, converted: the error carries the class and property so the
/// builder can report it and keep the default.
pub fn convert_property(
    converter: &mut ValueConverter,
    class: &str,
    property: &str,
    value: &J,
) -> Result<Variant, BridgeError> {
    converter.value(property, class, value).map_err(|detail| BridgeError::Value {
        class: class.to_string(),
        property: property.to_string(),
        detail,
    })
}

/// `{"Enum": n}` — the place-file form of an enum value, for a caller that has
/// a number and a class/property.
pub fn enum_json(n: u32) -> J {
    json!({ "Enum": n })
}
