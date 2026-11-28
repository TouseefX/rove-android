//! The traced six-byte own-root rotation codec, in row-major CFrame order.
pub const IDENTITY: [f64; 9] = [1., 0., 0., 0., 1., 0., 0., 0., 1.];

pub fn validate(matrix: [f64; 9]) -> bool {
    if matrix.iter().any(|v| !v.is_finite()) {
        return false;
    }
    for i in 0..3 {
        for j in 0..3 {
            let dot: f64 = (0..3).map(|k| matrix[k * 3 + i] * matrix[k * 3 + j]).sum();
            if (dot - if i == j { 1. } else { 0. }).abs() > 2e-4 {
                return false;
            }
        }
    }
    let [a, b, c, d, e, f, g, h, i] = matrix;
    (a * (e * i - f * h) - b * (d * i - f * g) + c * (d * h - e * g) - 1.).abs() < 2e-4
}

pub fn encode(matrix: [f64; 9]) -> Result<[u8; 6], &'static str> {
    if !validate(matrix) {
        return Err("physics-rotation-invalid");
    }
    let [a, b, c, d, e, f, g, h, i] = matrix.map(|v| v as f32);
    let trace = (a + e) + i;
    let mut q = if trace > 0. {
        let root = (trace + 1.).sqrt();
        let s = 0.5 / root;
        [s * (h - f), s * (c - g), s * (d - b), root * 0.5]
    } else if a > e && a > i {
        let root = (((1. + a) - e) - i).sqrt();
        let s = -0.5 / root;
        [root * -0.5, s * (b + d), s * (c + g), s * (h - f)]
    } else if e > i {
        let root = (((1. + e) - a) - i).sqrt();
        let s = -0.5 / root;
        [s * (b + d), root * -0.5, s * (f + h), s * (c - g)]
    } else {
        let root = (((1. + i) - a) - e).sqrt();
        let s = -0.5 / root;
        [s * (c + g), s * (f + h), root * -0.5, s * (d - b)]
    };
    for _ in 0..2 {
        let sum = ((f64::from(q[0]).powi(2) + f64::from(q[1]).powi(2)) as f32 + q[2] * q[2])
            + q[3] * q[3];
        let norm = sum.sqrt();
        if !norm.is_finite() || norm <= 0. {
            return Err("physics-rotation-invalid");
        }
        q = q.map(|v| v / norm);
    }
    let mut omitted = 0;
    for index in 1..4 {
        if q[index].abs() > q[omitted].abs() {
            omitted = index;
        }
    }
    if q[omitted] < 0. {
        q = q.map(|v| -v);
    }
    let mut packed = [0u32; 3];
    let mut next = 0;
    for (index, value) in q.into_iter().enumerate() {
        if index == omitted {
            continue;
        }
        let scaled = (value / 0.70710677f32) * 16383.;
        packed[next] =
            ((scaled + if scaled >= 0. { 0.5 } else { -0.5 }).trunc() as i32 as u32) & 0x7fff;
        next += 1;
    }
    let mut result = [0u8; 6];
    result[..2].copy_from_slice(&(packed[0] as u16).to_le_bytes());
    result[2..]
        .copy_from_slice(&(((omitted as u32) << 30) | (packed[1] << 15) | packed[2]).to_le_bytes());
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rotation_matches_independent_native_python_codec() {
        let cases: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/physics-pivot-rotations.json"
        ))
        .unwrap();
        for case in cases.as_array().unwrap() {
            let matrix = std::array::from_fn(|i| case["rotation"][i].as_f64().unwrap());
            let hex: String = encode(matrix)
                .unwrap()
                .iter()
                .map(|v| format!("{v:02x}"))
                .collect();
            assert_eq!(hex, case["hex"], "{}", case["name"]);
        }
        assert!(!validate([0.; 9]));
        let mut reflection = IDENTITY;
        reflection[0] = -1.;
        assert!(!validate(reflection));
        let mut invalid = IDENTITY;
        invalid[1] = f64::NAN;
        assert!(!validate(invalid));
    }
}
