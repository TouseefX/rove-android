//! Wire names that project onto public engine properties.
use serde_json::{json, Value};

pub(crate) fn from_wire(class: &str, key: &str, value: &Value) -> Result<Option<Value>, String> {
    match (class, key) {
        (_, "archivable") => value.as_bool().map(|v| Some(json!(v)))
            .ok_or_else(|| "received archivable must be a boolean".into()),
        (_, "Color3uint8") if is_part(class) => {
            let color = value
                .get("$color3")
                .and_then(Value::as_array)
                .ok_or("received Color3uint8 must be a tagged Color3")?;
            if color.len() != 3 {
                return Err("received Color3uint8 must be a tagged Color3".into());
            }
            for component in color {
                let Some(number) = component.as_f64() else {
                    return Err("received Color3uint8 must be a tagged Color3".into());
                };
                if !number.is_finite() || !(0.0..=1.0).contains(&number) {
                    return Err("received Color3uint8 component is out of range".into());
                }
            }
            Ok(Some(value.clone()))
        }
        (_, "CollisionGroupReplicate") if is_part(class) => Ok(Some(collision_group(value)?)),
        _ => Ok(None),
    }
}

fn collision_group(value: &Value) -> Result<Value, String> {
    let bytes = if let Some(text) = value.as_str() {
        text.as_bytes().to_vec()
    } else if let Some(hex) = value.get("$bytes_hex").and_then(Value::as_str) {
        if hex.len() % 2 != 0 || hex.len() > 2 * 1024 * 1024 {
            return Err("invalid CollisionGroupReplicate byte-string length".into());
        }
        let mut bytes = Vec::with_capacity(hex.len() / 2);
        for pair in hex.as_bytes().chunks_exact(2) {
            let digit = |b: u8| match b {
                b'0'..=b'9' => Some(b - b'0'),
                b'a'..=b'f' => Some(b - b'a' + 10),
                b'A'..=b'F' => Some(b - b'A' + 10),
                _ => None,
            };
            let hi = digit(pair[0]).ok_or("invalid CollisionGroupReplicate byte-string hex")?;
            let lo = digit(pair[1]).ok_or("invalid CollisionGroupReplicate byte-string hex")?;
            bytes.push((hi << 4) | lo);
        }
        bytes
    } else {
        return Err(
            "received CollisionGroupReplicate must be a string or tagged byte-string".into(),
        );
    };
    if bytes.is_empty() {
        return Err("received CollisionGroupReplicate is missing its group ID".into());
    }
    if bytes.len() == 1 {
        return Ok(json!("Default"));
    }
    let name = &bytes[1..];
    Ok(match std::str::from_utf8(name) {
        Ok(name) => json!(name),
        Err(_) => {
            json!({"$bytes_hex": name.iter().map(|b| format!("{b:02x}")).collect::<String>()})
        }
    })
}

fn is_part(class: &str) -> bool {
    matches!(
        class,
        "BasePart"
            | "Part"
            | "MeshPart"
            | "UnionOperation"
            | "WedgePart"
            | "CornerWedgePart"
            | "TrussPart"
            | "Seat"
            | "VehicleSeat"
            | "SpawnLocation"
    ) || super::world::class_chain(class)
        .iter()
        .any(|base| base == "BasePart")
}
pub(crate) fn canonical_key(class: &str, wire_key: &str) -> Option<&'static str> {
    if wire_key == "archivable" { return Some("Archivable"); }
    let key = match wire_key {
        "Color3uint8" => "Color",
        "CollisionGroupReplicate" => "CollisionGroup",
        _ => return None,
    };
    is_part(class).then_some(key)
}
pub(crate) fn wire_key(class: &str, public_key: &str) -> Option<&'static str> {
    if public_key == "Archivable" { return Some("archivable"); }
    let key = match public_key {
        "Color" => "Color3uint8",
        "CollisionGroup" => "CollisionGroupReplicate",
        _ => return None,
    };
    is_part(class).then_some(key)
}

pub(crate) fn is_lightweight_asset(key: &str) -> bool {
    matches!(
        key,
        "ModelMeshData"
            | "ModelMeshCFrame"
            | "ModelMeshSize"
            | "MeshId"
            | "MeshContent"
            | "TextureID"
            | "TextureId"
            | "Texture"
            | "TextureContent"
            | "TexturePack"
            | "Image"
            | "ImageContent"
            | "ColorMap"
            | "ColorMapContent"
            | "NormalMap"
            | "NormalMapContent"
            | "MetalnessMap"
            | "MetalnessMapContent"
            | "RoughnessMap"
            | "RoughnessMapContent"
            | "SoundId"
            | "AudioContent"
            | "Video"
            | "VideoContent"
            | "AnimationId"
            | "AnimationContent"
    )
}
