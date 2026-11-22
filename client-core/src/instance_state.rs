//! Received ISR updates (app 4, channel 5). Nothing here sends movement.
use crate::{
    replication::{compact_position, finite_f32, read_key},
    schema::Schema,
    wire::{Reader, WireError},
    world::WorldView,
};
use serde_json::{json, Map, Value};
use std::{collections::HashMap, sync::Arc};

const MAX_RECORDS: usize = 4096;
const MAX_PACKET_BYTES: usize = 1024 * 1024;
const MAX_TRACKED: usize = 100_000;

#[derive(Debug)]
pub struct Record {
    pub id: Option<String>,
    pub class_index: usize,
    pub fields: Map<String, Value>,
    pub lod: u8,
    pub time_delta: u8,
}
#[derive(Debug)]
pub struct Packet {
    pub timestamp: u32,
    pub header_value: i32,
    pub group_count: u32,
    pub records: Vec<Record>,
}

fn reference(r: &mut Reader<'_>) -> Result<Option<String>, WireError> {
    Ok(read_key(r)?.map(|(scope, index)| format!("{scope}:{index}")))
}

/// Validate every record and its exact body boundary before publishing values.
pub fn decode_packet(
    payload: &[u8],
    schema: &Schema,
    legacy_pv: bool,
) -> Result<Packet, WireError> {
    if payload.len() > MAX_PACKET_BYTES {
        return Err(WireError::new("ISR packet exceeds limit"));
    }
    let mut r = Reader::new(payload);
    if r.u8()? != 161 {
        return Err(WireError::new("Not an ISR packet"));
    }
    let timestamp = r.u32()?;
    let group_count = r.u32()?;
    let header_value = r.i32()?;
    if group_count as usize > MAX_RECORDS {
        return Err(WireError::new("ISR group count exceeds limit"));
    }
    let mut records = Vec::new();
    for _ in 0..group_count {
        let count = usize::from(r.u16()?);
        let lod = r.u8()?;
        let time_delta = r.u8()?;
        if lod > 3 || records.len() + count > MAX_RECORDS {
            return Err(WireError::new("Invalid ISR LOD or record count"));
        }
        for _ in 0..count {
            let id = reference(&mut r)?;
            let class_index = usize::from(r.u16()?);
            let bits = usize::from(r.u16()?);
            let class = schema
                .classes
                .get(class_index)
                .ok_or_else(|| WireError::new("Invalid ISR class"))?;
            // The server may legitimately send more dirty bits than the
            // schema's ISR table (newer properties). Fall back to the plain
            // class property list for the overflow indices.
            let capacity = class.isr_properties.len().max(class.properties.len());
            if bits > 256 || bits > capacity {
                return Err(WireError::new(format!(
                    "ISR dirty set exceeds schema class={} cid={} nprops={} bits={} pos={}",
                    class.name, class_index, capacity, bits, r.position()
                )));
            }
            let mut mask = [0u64; 4];
            for word in mask.iter_mut().take(bits.div_ceil(64)) {
                *word = r.u64()?;
            }
            if bits % 64 != 0 && mask[bits / 64] >> (bits % 64) != 0 {
                return Err(WireError::new("ISR dirty set has unused high bits"));
            }
            let length = usize::from(r.u16()?);
            let mut body = Reader::new(r.take(length)?);
            let mut fields = Map::new();
            for i in 0..bits {
                if mask[i / 64] & (1 << (i % 64)) == 0 {
                    continue;
                }
                let property = class
                    .isr_properties
                    .get(i)
                    .and_then(|pid| schema.properties.get(*pid as usize))
                    .or_else(|| {
                        class
                            .properties
                            .get(i)
                            .and_then(|pid| schema.properties.get(*pid as usize))
                    })
                    .ok_or_else(|| WireError::new("Invalid ISR property index"))?;
                let value = match property.type_id {
                    10 => json!(body.signed(32)?),
                    11 => finite_f32(body.f32()?)?,
                    28 | 69 => reference(&mut body)?.map_or(Value::Null, |id| json!({"$ref":id})),
                    57 if legacy_pv => read_pv(&mut body, lod)?,
                    57 => {
                        return Err(WireError::new(
                            "ISR PV format requires received DebugAdaptivePVCompressor=false",
                        ))
                    }
                    _ => {
                        return Err(WireError::new(format!(
                            "Unsupported ISR property type {}",
                            property.type_id
                        )))
                    }
                };
                if fields.insert(property.name.clone(), value).is_some() {
                    return Err(WireError::new("Duplicate ISR property"));
                }
            }
            body.finish()?;
            records.push(Record {
                id,
                class_index,
                fields,
                lod,
                time_delta,
            });
        }
    }
    r.finish()?;
    Ok(Packet {
        timestamp,
        header_value,
        group_count,
        records,
    })
}

fn signed(value: u32, bits: u32) -> i32 {
    ((value << (32 - bits)) as i32) >> (32 - bits)
}

fn rotation(r: &mut Reader<'_>, lod: u8, selector: u8) -> Result<[f64; 9], WireError> {
    if (1..=24).contains(&selector) {
        const PAIRS: [usize; 25] = [
            0, 1, 2, 4, 5, 6, 8, 9, 11, 12, 13, 15, 16, 19, 20, 22, 23, 24, 26, 27, 29, 30, 31, 33,
            34,
        ];
        const AXES: [[f64; 3]; 6] = [
            [1., 0., 0.],
            [0., 1., 0.],
            [0., 0., 1.],
            [-1., 0., 0.],
            [0., -1., 0.],
            [0., 0., -1.],
        ];
        let pair = PAIRS[selector as usize];
        let a = AXES[pair / 6];
        let b = AXES[pair % 6];
        let c = [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ];
        return Ok([a[0], b[0], c[0], a[1], b[1], c[1], a[2], b[2], c[2]]);
    }
    if selector != 0 {
        return Err(WireError::new(format!(
            "Unsupported ISR PV rotation selector {selector}"
        )));
    }
    let mut q = [0f32; 4];
    let (omitted, packed, width, scale) = if lod < 2 {
        let first = u32::from(r.u16()?);
        let second = r.u32()?;
        (
            (second >> 30) as usize,
            [first & 0x7fff, (second >> 15) & 0x7fff, second & 0x7fff],
            15,
            16383f32,
        )
    } else {
        let word = r.u32()?;
        (
            (word >> 30) as usize,
            [(word >> 20) & 1023, (word >> 10) & 1023, word & 1023],
            10,
            511f32,
        )
    };
    let mut input = 0;
    for (index, slot) in q.iter_mut().enumerate() {
        if index != omitted {
            *slot = (signed(packed[input], width) as f32 / scale) * 0.70710677f32;
            input += 1;
        }
    }
    if lod < 2 {
        let squared: f64 = q.iter().map(|&v| f64::from(v).powi(2)).sum();
        q[omitted] = (1. - squared).max(0.).sqrt() as f32;
        let norm = q.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>().sqrt();
        for value in &mut q {
            *value = (f64::from(*value) / norm) as f32;
        }
    } else {
        let v: Vec<f32> = q
            .iter()
            .enumerate()
            .filter_map(|(i, &v)| (i != omitted).then_some(v))
            .collect();
        let squared = (v[0] * v[0] + v[1] * v[1]) + v[2] * v[2];
        q[omitted] = (1f32 - squared).max(0.).sqrt();
    }
    let [x, y, z, w] = q.map(f64::from);
    Ok([
        1. - 2. * (y * y + z * z),
        2. * (x * y - z * w),
        2. * (x * z + y * w),
        2. * (x * y + z * w),
        1. - 2. * (x * x + z * z),
        2. * (y * z - x * w),
        2. * (x * z - y * w),
        2. * (y * z + x * w),
        1. - 2. * (x * x + y * y),
    ]
    .map(|v| f64::from(v as f32)))
}

fn velocity(r: &mut Reader<'_>, lod: u8) -> Result<[f64; 3], WireError> {
    let header = r.u8()?;
    if header == 0 {
        return Ok([0.; 3]);
    }
    if lod == 0 {
        let exponent = i32::from(header >> 4) - 1;
        if !(0..=14).contains(&exponent) {
            return Err(WireError::new("Invalid ISR velocity exponent"));
        }
        let word = r.u32()?;
        let quantized = [
            signed(word >> 20, 12),
            signed((word >> 8) & 4095, 12),
            signed(((word & 255) << 4) | u32::from(header & 15), 12),
        ];
        if quantized.contains(&-2048) {
            return Err(WireError::new("Invalid ISR velocity component"));
        }
        let scale = 2f64.powi(exponent) / 2047.;
        Ok(quantized.map(|v| f64::from(v) * scale))
    } else {
        let middle = u32::from(r.u8()?);
        let tail = u32::from(r.u8()?);
        let exponent = signed(u32::from((header >> 2) & 31), 5);
        let quantized = [
            signed((u32::from(header & 3) << 4) | (middle >> 4), 6),
            signed(((middle & 15) << 2) | (tail >> 6), 6),
            signed(tail & 63, 6),
        ];
        Ok(quantized.map(|v| f64::from((v as f32 / 31f32) * 2f32.powi(exponent))))
    }
}

fn read_pv(r: &mut Reader<'_>, lod: u8) -> Result<Value, WireError> {
    let flags = r.u8()?;
    let position = if flags & 128 == 0 {
        compact_position(r)?
    } else {
        [json!(0.), json!(0.), json!(0.)]
    };
    let matrix = rotation(r, lod, flags & 31)?;
    let linear = if flags & 64 == 0 {
        velocity(r, lod)?
    } else {
        [0.; 3]
    };
    let angular = if flags & 32 == 0 {
        velocity(r, lod)?
    } else {
        [0.; 3]
    };
    Ok(
        json!({"position":position,"rotation":matrix,"decoded_velocities":[linear,angular],
        "rotation_selector":flags & 31,"wire_format":"isr_legacy_pv","lod":lod}),
    )
}

#[derive(Clone, Copy)]
struct Stamp {
    timestamp: u32,
    creation: Option<u64>,
}
pub(crate) struct Prepared {
    pub records: Vec<Record>,
    stamps: Vec<(Arc<str>, Stamp)>,
}
#[derive(Default)]
pub(crate) struct Receiver {
    legacy: bool,
    stamps: HashMap<Arc<str>, Stamp>,
    received: u64,
    decoded: u64,
    rejected: u64,
    unavailable: u64,
    applied: u64,
    unresolved: u64,
    stale: u64,
    lods: [u64; 4],
    last_timestamp: Option<u32>,
    last_error: Option<String>,
}
impl Receiver {
    pub fn configure(&mut self, legacy: bool) {
        self.legacy = legacy;
    }
    pub fn unavailable(&mut self) {
        self.received += 1;
        self.unavailable += 1;
    }
    pub fn prepare(
        &mut self,
        payload: &[u8],
        schema: &Schema,
        world: &WorldView,
    ) -> Option<Prepared> {
        self.received += 1;
        let packet = match decode_packet(payload, schema, self.legacy) {
            Ok(packet) => packet,
            Err(error) => {
                self.rejected += 1;
                self.last_error = Some(error.to_string());
                return None;
            }
        };
        self.decoded += 1;
        self.last_timestamp = Some(packet.timestamp);
        // Retain only current instances, without walking the entire world.
        if self.received % 256 == 0 || self.stamps.len() + packet.records.len() > MAX_TRACKED {
            self.stamps.retain(|id, stamp| {
                world.read_only_node(id).is_some_and(|node| {
                    node.created_sequence() == stamp.creation
                        && node.record_field("destroyed") != Some(json!(true))
                })
            });
        }
        let mut prepared = Prepared {
            records: Vec::new(),
            stamps: Vec::new(),
        };
        for record in packet.records {
            self.lods[record.lod as usize] += 1;
            let Some(node) = record.id.as_deref().and_then(|id| world.read_only_node(id)) else {
                self.unresolved += 1;
                continue;
            };
            if node.class_name() != schema.classes[record.class_index].name
                || node.record_field("destroyed") == Some(json!(true))
            {
                self.unresolved += 1;
                continue;
            }
            let stamp = Stamp {
                timestamp: packet.timestamp,
                creation: node.created_sequence(),
            };
            if self.stamps.get(node.id()).is_some_and(|old| {
                old.creation == stamp.creation
                    && stamp.timestamp.wrapping_sub(old.timestamp) >= 0x8000_0000
            }) {
                self.stale += 1;
                continue;
            }
            if !record.fields.is_empty() {
                prepared.stamps.push((node.shared_id(), stamp));
                prepared.records.push(record);
            }
        }
        Some(prepared)
    }
    pub fn committed(&mut self, prepared: Prepared) {
        self.applied += prepared.stamps.len() as u64;
        self.stamps.extend(prepared.stamps);
    }
    pub fn report(&self) -> Value {
        json!({"packets_received":self.received,"packets_decoded":self.decoded,"packets_rejected":self.rejected,
            "packets_before_world":self.unavailable,"records_applied":self.applied,"unresolved_records":self.unresolved,
            "stale_records_ignored":self.stale,"records_by_lod":self.lods,"tracked_instances":self.stamps.len(),
            "legacy_pv_enabled":self.legacy,"last_packet_timestamp":self.last_timestamp,"last_decode_error":self.last_error})
    }
}

#[cfg(test)]
#[path = "instance_state_tests.rs"]
mod tests;
