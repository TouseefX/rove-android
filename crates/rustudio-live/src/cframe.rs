//! Every `$cframe` rove emits, read as one `CFrame`.
//!
//! rove has **two** CFrame spellings on the wire, and they arrive on the same
//! stream, in the same fields:
//!
//! * `{"$cframe": {"position": [x, y, z], "rotation": [r00 … r22]}}` — the
//!   fully decoded form, which `runtime_delta::normalize_cframe` produces when
//!   it can (`runtime_delta.rs:951`).
//! * `{"$cframe": {"position": [x, y, z], "rotation_id": n, "packed_rotation":
//!   "…hex…"}}` — the compact wire form from `replication.rs` type 27: a
//!   **basis selector** (1..=36, or 0 for "a packed quaternion follows"), plus
//!   the packed quaternion when the selector is 0.
//! * `{"$cframe": {"position": […], "rotation": n, "…"}}` — type 51, where the
//!   small `rotation` number is the selector/identity flag and the basis is
//!   implied.
//! * a bare object with no `$cframe` tag at all (`{"position": …, "rotation_id":
//!   …}`), because a create's node properties and an ISR record both spell it
//!   that way.
//!
//! Only the first was converted. The other three were refused, and the report
//! from the phone said so, in the same words for three properties that are all
//! a CFrame: `Part.CFrame ×57`, `Attachment.CFrame ×54`, `Tool.Grip ×48`,
//! `MeshPart.CFrame ×20` — 179 refusals that left every part in the place at
//! the origin of a default CFrame, which is a world that draws as void.
//!
//! The selector table and the quaternion unpacking are rove's, from
//! `rove/client-core/src/rpc.rs` (`property_selector_matrix`, `unpack_quaternion`)
//! and `runtime_delta.rs` (`normalize_cframe`): a 6-direction basis, 36 ordered
//! pairs, `selector - 1` split as `axis_a = code / 6`, `axis_b = code % 6`, and
//! the third axis from the cross product. The arithmetic is repeated here rather
//! than imported because `rove/` is not a workspace member on purpose (it is the
//! network reference, `docs/LIVE-JOIN.md`); the tests below pin its outputs.

use rbx_dom_weak::types::{CFrame, Matrix3, Vector3};
use serde_json::Value as J;

/// The six axis directions a selector's basis is built from, in rove's order
/// (`rpc.rs:1360`): +X, +Y, +Z, −X, −Y, −Z.
const AXES: [[f64; 3]; 6] = [
    [1., 0., 0.],
    [0., 1., 0.],
    [0., 0., 1.],
    [-1., 0., 0.],
    [0., -1., 0.],
    [0., 0., -1.],
];

/// The rotation matrix a basis selector names, **row-major**, exactly as rove
/// produces it (`property_selector_matrix`): `None` for a selector outside
/// `1..=36` and for a pair of axes that is not perpendicular.
pub fn selector_matrix(selector: usize) -> Option<[f64; 9]> {
    if !(1..=36).contains(&selector) {
        return None;
    }
    let code = selector - 1;
    let first = AXES[code / 6];
    let second = AXES[code % 6];
    let dot: f64 = (0..3).map(|i| first[i] * second[i]).sum();
    if dot != 0.0 {
        return None;
    }
    let cross = [
        first[1] * second[2] - first[2] * second[1],
        first[2] * second[0] - first[0] * second[2],
        first[0] * second[1] - first[1] * second[0],
    ];
    let mut out = [0.0f64; 9];
    for r in 0..3 {
        out[r * 3] = first[r];
        out[r * 3 + 1] = second[r];
        out[r * 3 + 2] = cross[r];
    }
    Some(out)
}

/// A rotation matrix from the compact wire's six packed bytes, row-major.
///
/// `q = [x, y, z, w]` with one component omitted (the largest, whose sign was
/// folded in), three of them as 15-bit signed ratios of `√½` (rove's
/// `unpack_quaternion`), and the matrix from the quaternion in rove's own
/// component order (`normalize_received_cframe`).
pub fn unpack_rotation(bytes: &[u8]) -> Option<[f64; 9]> {
    if bytes.len() != 6 {
        return None;
    }
    let first = u16::from_le_bytes([bytes[0], bytes[1]]) as u32;
    let second = u32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
    let omitted = (second >> 30) as usize;
    if omitted > 3 {
        return None;
    }
    let packed = [first & 0x7fff, (second >> 15) & 0x7fff, second & 0x7fff];
    let components = [[1usize, 2, 3], [0, 2, 3], [0, 1, 3], [0, 1, 2]][omitted];
    let mut values = [0.0f32; 4];
    for (index, packed) in components.into_iter().zip(packed) {
        let signed = if packed & 0x4000 != 0 {
            (packed as i32) - 0x8000
        } else {
            packed as i32
        };
        values[index] = (signed as f32 / 16383.0) * std::f32::consts::FRAC_1_SQRT_2;
    }
    values[omitted] = (1.0 - values.iter().map(|value| value * value).sum::<f32>())
        .max(0.0)
        .sqrt();
    let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() || norm <= 0.0 {
        return None;
    }
    for value in &mut values {
        *value /= norm;
    }
    let [x, y, z, w] = values.map(f64::from);
    Some([
        1.0 - 2.0 * (y * y + z * z),
        2.0 * (x * y - z * w),
        2.0 * (x * z + y * w),
        2.0 * (x * y + z * w),
        1.0 - 2.0 * (x * x + z * z),
        2.0 * (y * z - x * w),
        2.0 * (x * z - y * w),
        2.0 * (y * z + x * w),
        1.0 - 2.0 * (x * x + y * y),
    ])
}

fn hex_bytes(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in text.as_bytes().chunks(2) {
        let pair = std::str::from_utf8(pair).ok()?;
        out.push(u8::from_str_radix(pair, 16).ok()?);
    }
    Some(out)
}

fn position(v: &J) -> Option<[f64; 3]> {
    let a = v.as_array().filter(|a| a.len() == 3)?;
    let mut out = [0.0f64; 3];
    for (slot, x) in out.iter_mut().zip(a) {
        *slot = x.as_f64()?;
    }
    Some(out)
}

fn nine(v: &J) -> Option<[f64; 9]> {
    let a = v.as_array().filter(|a| a.len() == 9)?;
    let mut out = [0.0f64; 9];
    for (slot, x) in out.iter_mut().zip(a) {
        *slot = x.as_f64()?;
    }
    Some(out)
}

/// The `CFrame` a `$cframe` payload (or an untagged frame object) names.
///
/// Every shape rove emits is accepted, in the order they are cheapest to read:
/// a 9-number `rotation`; a `rotation_id` basis selector; a `rotation_id` of 0
/// with a `packed_rotation` quaternion; type 51's small `rotation` number used
/// as the selector; and a frame with a `position` and nothing else, which is the
/// identity basis at that position. `None` only when there is no position to
/// place it at, or when a selector/quaternion is present and invalid — an
/// invalid rotation is a fault, not an identity to invent.
pub fn frame(v: &J) -> Option<CFrame> {
    let position = position(v.get("position")?)?;
    let rotation = rotation_of(v)?;
    // rove's rotation is row-major (`rotation[row * 3 + column]`, which
    // `rpc.rs` validates as orthonormal that way). `rbx_types` stores a
    // `Matrix3` as its three *columns* (`m.x` is the first column), and every
    // reader in this workspace takes a CFrame's orientation as
    // `Xform::from_cframe` does, so rove's rows are stored as this matrix's
    // columns and the read below is the transpose. It is the same convention
    // the 9-array `$cframe` form already used.
    let m = Matrix3::new(
        Vector3::new(rotation[0] as f32, rotation[3] as f32, rotation[6] as f32),
        Vector3::new(rotation[1] as f32, rotation[4] as f32, rotation[7] as f32),
        Vector3::new(rotation[2] as f32, rotation[5] as f32, rotation[8] as f32),
    );
    Some(CFrame::new(
        Vector3::new(position[0] as f32, position[1] as f32, position[2] as f32),
        m,
    ))
}

/// The row-major basis of a frame object, whichever of rove's spellings it uses.
fn rotation_of(v: &J) -> Option<[f64; 9]> {
    // 1. The decoded form: nine numbers.
    if let Some(m) = v.get("rotation").and_then(nine) {
        return Some(m);
    }
    // 2. A basis selector. `runtime_delta` also writes `rotation_selector` and
    //    a decoded `rotation_mapping`, so the selector is read from either name.
    let selector = v
        .get("rotation_id")
        .or_else(|| v.get("rotation_selector"))
        .and_then(J::as_u64);
    if let Some(selector) = selector {
        if selector != 0 {
            return selector_matrix(selector as usize);
        }
        // 3. Selector 0: the rotation is the packed quaternion.
        let packed = v.get("packed_rotation").and_then(J::as_str)?;
        return hex_bytes(packed).as_deref().and_then(unpack_rotation);
    }
    // 4. Type 51's small `rotation`: a flag byte where 128 is "packed follows"
    //    and anything else is the selector. rove writes the selector straight
    //    into `rotation` for this kind.
    if let Some(rotation) = v.get("rotation").and_then(J::as_u64) {
        if rotation == 0 || rotation == 128 {
            return v
                .get("packed_rotation")
                .and_then(J::as_str)
                .and_then(|h| hex_bytes(h))
                .as_deref()
                .and_then(unpack_rotation)
                .or_else(identity);
        }
        return selector_matrix(rotation as usize);
    }
    // 5. A frame that only carries a position: identity.
    identity()
}

fn identity() -> Option<[f64; 9]> {
    Some([1., 0., 0., 0., 1., 0., 0., 0., 1.])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// rove's own vector for selector 17 (`rpc.rs`'s
    /// `received_cframes_decode_selectors_and_packed_quaternions`).
    ///
    /// `Matrix3`'s `x`, `y`, `z` are its **rows**, and the conversion stores
    /// rove's rows as this matrix's columns (the transpose), which is what every
    /// other CFrame reader here does (`Xform::from_cframe`). So the check is
    /// that the stored matrix transposed is rove's matrix.
    #[test]
    fn a_selector_decodes_to_rove_s_own_matrix() {
        let expected = selector_matrix(17).expect("17 is a basis");
        let frame = frame(&json!({"position": [0, 0, 0], "rotation_id": 17}))
            .expect("a selector with a position is a CFrame");
        let m = frame.orientation;
        let stored = [
            m.x.x, m.x.y, m.x.z,
            m.y.x, m.y.y, m.y.z,
            m.z.x, m.z.y, m.z.z,
        ]
        .map(f64::from);
        let transposed: Vec<f64> = (0..3)
            .flat_map(|r| (0..3).map(move |c| expected[c * 3 + r]))
            .collect();
        for (actual, wanted) in stored.iter().zip(&transposed) {
            assert!((actual - wanted).abs() < 1e-6, "{stored:?} != {transposed:?}");
        }
    }

    /// The identity selector is 2 in the compact wire (rove's `runtime_delta`
    /// pins the same number).
    #[test]
    fn selector_two_is_the_identity() {
        let m = selector_matrix(2).expect("2 is +X with +Y");
        assert_eq!(m, [1., 0., 0., 0., 1., 0., 0., 0., 1.]);
    }

    /// A packed quaternion decodes to the rotation it was packed from — packed
    /// with rove's own algorithm (`rpc.rs::pack_quaternion`): normalise, omit
    /// the largest component, store the other three as 15-bit ratios of √½.
    #[test]
    fn a_packed_quaternion_decodes_to_its_matrix() {
        let theta = std::f64::consts::FRAC_PI_4; // a 45° turn about +Y
        let q = [0.0f64, (theta / 2.0).sin(), 0.0, (theta / 2.0).cos()];
        // The largest component is w, so it is the one omitted.
        let mut omitted = 0;
        for i in 1..4 {
            if q[i].abs() > q[omitted].abs() {
                omitted = i;
            }
        }
        let mut q = q;
        if q[omitted] < 0.0 {
            q = q.map(|v| -v);
        }
        let components = [[1usize, 2, 3], [0, 2, 3], [0, 1, 3], [0, 1, 2]][omitted];
        let mut words = [0u32; 3];
        for (index, component) in components.into_iter().enumerate() {
            let ratio = q[component] / f64::from(std::f32::consts::FRAC_1_SQRT_2);
            words[index] = (((ratio * 16383.0) as i32) & 0x7fff) as u32;
        }
        let second = ((omitted as u32) << 30) | (words[1] << 15) | words[2];
        let mut packed = [0u8; 6];
        packed[..2].copy_from_slice(&(words[0] as u16).to_le_bytes());
        packed[2..].copy_from_slice(&second.to_le_bytes());
        let hex: String = packed.iter().map(|b| format!("{b:02x}")).collect();
        let f = frame(&json!({"position": [1, 2, 3], "rotation_id": 0, "packed_rotation": hex}))
            .expect("a packed quaternion is a rotation");
        assert!((f.position.y - 2.0).abs() < 1e-6);
        // rove's matrix's row 0 is (cos θ, 0, sin θ) for a turn about +Y, and
        // this side stores rove's rows as columns.
        let [x, y, z, w] = q;
        let reference = [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y - z * w),
            2.0 * (x * z + y * w),
        ];
        let m = f.orientation;
        let first_column = [m.x.x, m.y.x, m.z.x].map(f64::from);
        for (actual, wanted) in first_column.iter().zip(&reference) {
            assert!(
                (actual - wanted).abs() < 2e-3,
                "row 0 {first_column:?} != {reference:?}"
            );
        }
        assert!((first_column[0] - theta.cos()).abs() < 2e-3);
        assert!((first_column[2] - theta.sin()).abs() < 2e-3);
    }

    #[test]
    fn a_seven_number_rotation_is_still_a_rotation() {
        let f = frame(&json!({
            "position": [0, 5, 0],
            "rotation": [1, 0, 0, 0, 1, 0, 0, 0, 1],
        }))
        .expect("nine numbers");
        assert!((f.position.y - 5.0).abs() < 1e-6);
    }

    /// A frame with no rotation at all is the identity basis, not a refusal:
    /// a `WorldPivotData` and a part saved at the origin are both this shape.
    #[test]
    fn a_position_only_frame_is_the_identity_at_that_position() {
        let f = frame(&json!({"position": [4, 0, -2]})).expect("a position alone");
        assert!((f.position.x - 4.0).abs() < 1e-6);
        assert!((f.position.z + 2.0).abs() < 1e-6);
        assert!((f.orientation.x.x - 1.0).abs() < 1e-6);
    }

    #[test]
    fn an_unpackable_rotation_is_not_an_identity() {
        assert!(frame(&json!({"position": [0, 0, 0], "rotation_id": 0, "packed_rotation": "00"}))
            .is_none());
        assert!(frame(&json!({"position": [0, 0, 0], "rotation_id": 99})).is_none());
        assert!(frame(&json!({"rotation_id": 2})).is_none(), "no position");
    }
}
