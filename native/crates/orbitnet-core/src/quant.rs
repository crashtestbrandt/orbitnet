//! Wire quantizers: lossy per-property encodings that shrink blocks without touching row layout.
//!
//! History rows stay full native stride — quantization changes only (a) the bytes a property
//! occupies ON THE WIRE and (b) the value itself at capture time. Every quantized property is
//! **canonicalized** when captured: the native bytes are round-tripped through the wire encoding,
//! so the value a peer stores, compares, restores and re-simulates from IS the wire-representable
//! value. That single rule is what keeps the whole scheme deterministic:
//!
//! - masks stay byte-stable (a canonical row re-encodes to the same wire bits),
//! - the mispredict compare stays an exact byte compare (both peers hold canonical rows),
//! - resimulation stays bit-exact across peers (both sides simulate from canonical state).
//!
//! Canonicalization must therefore be a pure deterministic function, and every decoder must be
//! total over hostile bytes (clamped, never NaN/inf) — a poisoned float would otherwise walk
//! straight into the physics state.
//!
//! Palette (deliberately small; a property opts in per-registration via an `@` annotation):
//! - [`QuantKind::Ss3`] — smallest-three orientation packing: `Quat` 16 B → 6 B, and a
//!   rotation-only `Basis` 36 B → 6 B (via quaternion). ~1.2e-4 component resolution (15-bit),
//!   far below perceptible.
//! - [`QuantKind::Half`] — IEEE binary16 per component: `Vec3` 12 B → 6 B, `Vec2` 8 B → 4 B,
//!   `F32` 4 B → 2 B. ~0.05% relative resolution; for rates and directions, not positions.

use crate::protocol::{PropKind, PropSchema, QuantKind};

// ---------------------------------------------------------------------------
// f16 (IEEE binary16), software conversion — deterministic, no NaN/inf escape.
// ---------------------------------------------------------------------------

/// Convert an `f32` to binary16 bits, round-to-nearest-even, clamped finite.
///
/// Non-finite inputs quantize to 0 and magnitudes beyond f16 range clamp to ±65504 — poison must
/// die at the encoder, not replicate.
#[must_use]
pub fn f32_to_f16_bits(value: f32) -> u16 {
    if !value.is_finite() {
        return 0;
    }
    let clamped = value.clamp(-65504.0, 65504.0);
    let bits = clamped.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x007f_ffff;

    // Subnormal-or-zero range for f16: exponent below -14.
    let unbiased = exp - 127;
    if unbiased < -24 {
        return sign; // Rounds to zero.
    }
    if unbiased < -14 {
        // f32 normal → f16 subnormal: shift the implicit-1 mantissa into place with rounding.
        let full = mantissa | 0x0080_0000;
        let shift = (-14 - unbiased) as u32 + 13;
        let halfway = 1u32 << (shift - 1);
        let mut sub = full >> shift;
        let rem = full & ((1u32 << shift) - 1);
        if rem > halfway || (rem == halfway && sub & 1 == 1) {
            sub += 1;
        }
        return sign | sub as u16;
    }
    // Normal range: rebias and round the mantissa to 10 bits (round-to-nearest-even).
    let mut half_exp = (unbiased + 15) as u32;
    let mut half_man = mantissa >> 13;
    let rem = mantissa & 0x1fff;
    if rem > 0x1000 || (rem == 0x1000 && half_man & 1 == 1) {
        half_man += 1;
        if half_man == 0x400 {
            half_man = 0;
            half_exp += 1;
        }
    }
    if half_exp >= 31 {
        // Rounded past the largest finite f16; clamp.
        return sign | 0x7bff;
    }
    sign | ((half_exp as u16) << 10) | half_man as u16
}

/// Convert binary16 bits to `f32`. Total over hostile bits: inf/NaN patterns decode to 0.
#[must_use]
pub fn f16_bits_to_f32(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0f32 } else { 1.0 };
    let exp = (bits >> 10) & 0x1f;
    let man = bits & 0x03ff;
    match exp {
        0 => sign * f32::from(man) * (1.0 / 16_777_216.0), // subnormal: man * 2^-24
        31 => 0.0,                                         // inf/NaN off the wire: poison dies
        _ => {
            let m = 1.0 + f32::from(man) / 1024.0;
            sign * m * (f32::from(exp) - 15.0).exp2()
        }
    }
}

// ---------------------------------------------------------------------------
// Smallest-three orientation packing (48 bits).
// ---------------------------------------------------------------------------

/// One over square root of two: the magnitude bound on the three smallest components.
const FRAC_1_SQRT_2: f32 = std::f32::consts::FRAC_1_SQRT_2;
/// 15-bit signed payload scale.
const SS3_SCALE: f32 = 16383.0;

/// Pack a unit quaternion `[x, y, z, w]` into 6 bytes (three 15-bit components + largest index).
///
/// Layout (little-endian u16 triple): payload `n` in the low 15 bits of word `n`; the largest
/// component's 2-bit index rides the top bits of words 0 and 1. The largest component is made
/// positive first (q and −q are the same rotation), so it needs no sign bit.
#[must_use]
pub fn quat_to_ss3(q: [f32; 4]) -> [u8; 6] {
    let mut q = q;
    // A non-finite or zero quaternion canonicalizes to identity rather than poisoning the wire.
    let norm_sq: f32 = q.iter().map(|c| c * c).sum();
    if !norm_sq.is_finite() || norm_sq < 1e-12 {
        q = [0.0, 0.0, 0.0, 1.0];
    } else {
        let inv = norm_sq.sqrt().recip();
        for c in &mut q {
            *c *= inv;
        }
    }
    let mut largest = 0usize;
    for i in 1..4 {
        if q[i].abs() > q[largest].abs() {
            largest = i;
        }
    }
    if q[largest] < 0.0 {
        for c in &mut q {
            *c = -*c;
        }
    }
    let mut words = [0u16; 3];
    let mut w = 0usize;
    for (i, &c) in q.iter().enumerate() {
        if i == largest {
            continue;
        }
        let scaled = (c / FRAC_1_SQRT_2 * SS3_SCALE)
            .round()
            .clamp(-SS3_SCALE, SS3_SCALE) as i32;
        words[w] = (scaled + 16383) as u16; // 0..=32766, fits 15 bits.
        w += 1;
    }
    words[0] |= ((largest & 1) as u16) << 15;
    words[1] |= (((largest >> 1) & 1) as u16) << 15;
    let mut out = [0u8; 6];
    out[0..2].copy_from_slice(&words[0].to_le_bytes());
    out[2..4].copy_from_slice(&words[1].to_le_bytes());
    out[4..6].copy_from_slice(&words[2].to_le_bytes());
    out
}

/// Unpack 6 bytes into a unit quaternion `[x, y, z, w]`. Total over hostile bytes.
#[must_use]
pub fn ss3_to_quat(bytes: [u8; 6]) -> [f32; 4] {
    let w0 = u16::from_le_bytes([bytes[0], bytes[1]]);
    let w1 = u16::from_le_bytes([bytes[2], bytes[3]]);
    let w2 = u16::from_le_bytes([bytes[4], bytes[5]]);
    let largest = ((w0 >> 15) & 1 | ((w1 >> 15) & 1) << 1) as usize;
    let decode = |word: u16| -> f32 {
        let payload = (word & 0x7fff) as i32;
        // Hostile payload 32767 clamps into the legal −16383..=16383 range.
        let centered = (payload - 16383).clamp(-16383, 16383) as f32;
        centered / SS3_SCALE * FRAC_1_SQRT_2
    };
    let smalls = [decode(w0), decode(w1), decode(w2)];
    let sum_sq: f32 = smalls.iter().map(|c| c * c).sum();
    let big = (1.0 - sum_sq).max(0.0).sqrt();
    let mut q = [0.0f32; 4];
    let mut s = 0usize;
    for (i, slot) in q.iter_mut().enumerate() {
        if i == largest {
            *slot = big;
        } else {
            *slot = smalls[s];
            s += 1;
        }
    }
    // Renormalize: quantization shrinks the vector slightly; hostile bytes can shrink it a lot.
    let norm_sq: f32 = q.iter().map(|c| c * c).sum();
    if norm_sq < 1e-12 {
        return [0.0, 0.0, 0.0, 1.0];
    }
    let inv = norm_sq.sqrt().recip();
    for c in &mut q {
        *c *= inv;
    }
    q
}

/// Convert a row-major 3×3 rotation matrix (the `Basis` wire layout) to a quaternion.
///
/// Shepperd's method over the largest diagonal pivot. Assumes an orthonormal rotation basis —
/// scale/shear does not survive `@ss3` (the annotation is for rotation frames).
#[must_use]
pub fn basis_to_quat(m: &[f32; 9]) -> [f32; 4] {
    // m[r*3+c] = row r, column c.
    let (m00, m01, m02) = (m[0], m[1], m[2]);
    let (m10, m11, m12) = (m[3], m[4], m[5]);
    let (m20, m21, m22) = (m[6], m[7], m[8]);
    let trace = m00 + m11 + m22;
    let q: [f32; 4] = if trace > 0.0 {
        let s = (trace + 1.0).sqrt() * 2.0;
        [(m21 - m12) / s, (m02 - m20) / s, (m10 - m01) / s, 0.25 * s]
    } else if m00 > m11 && m00 > m22 {
        let s = (1.0 + m00 - m11 - m22).max(0.0).sqrt() * 2.0;
        if s < 1e-12 {
            return [0.0, 0.0, 0.0, 1.0];
        }
        [0.25 * s, (m01 + m10) / s, (m02 + m20) / s, (m21 - m12) / s]
    } else if m11 > m22 {
        let s = (1.0 + m11 - m00 - m22).max(0.0).sqrt() * 2.0;
        if s < 1e-12 {
            return [0.0, 0.0, 0.0, 1.0];
        }
        [(m01 + m10) / s, 0.25 * s, (m12 + m21) / s, (m02 - m20) / s]
    } else {
        let s = (1.0 + m22 - m00 - m11).max(0.0).sqrt() * 2.0;
        if s < 1e-12 {
            return [0.0, 0.0, 0.0, 1.0];
        }
        [(m02 + m20) / s, (m12 + m21) / s, 0.25 * s, (m10 - m01) / s]
    };
    if q.iter().any(|c| !c.is_finite()) {
        return [0.0, 0.0, 0.0, 1.0];
    }
    q
}

/// Convert a quaternion to the row-major 3×3 rotation matrix (`Basis` wire layout).
#[must_use]
pub fn quat_to_basis(q: [f32; 4]) -> [f32; 9] {
    let [x, y, z, w] = q;
    let (xx, yy, zz) = (x * x, y * y, z * z);
    let (xy, xz, yz) = (x * y, x * z, y * z);
    let (wx, wy, wz) = (w * x, w * y, w * z);
    [
        1.0 - 2.0 * (yy + zz),
        2.0 * (xy - wz),
        2.0 * (xz + wy),
        2.0 * (xy + wz),
        1.0 - 2.0 * (xx + zz),
        2.0 * (yz - wx),
        2.0 * (xz - wy),
        2.0 * (yz + wx),
        1.0 - 2.0 * (xx + yy),
    ]
}

// ---------------------------------------------------------------------------
// Row-level plumbing: strides, canonicalize, wire encode/decode.
// ---------------------------------------------------------------------------

/// Bytes this property occupies on the wire.
#[must_use]
pub fn wire_stride(kind: PropKind, quant: QuantKind) -> usize {
    match (quant, kind) {
        (QuantKind::Ss3, PropKind::Quat | PropKind::Basis) => 6,
        (QuantKind::Half, PropKind::Vec3) => 6,
        (QuantKind::Half, PropKind::Vec2) => 4,
        (QuantKind::Half, PropKind::F32) => 2,
        _ => kind.stride(),
    }
}

/// Bytes a full row of `props` occupies on the wire.
#[must_use]
pub fn wire_row_stride(props: &[PropSchema]) -> usize {
    props.iter().map(|p| wire_stride(p.kind, p.quant)).sum()
}

fn read_f32(bytes: &[u8]) -> f32 {
    f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn write_f32(bytes: &mut [u8], value: f32) {
    bytes.copy_from_slice(&value.to_le_bytes());
}

/// Encode one property's native bytes to its wire form, appending to `out`.
fn encode_prop(prop: &PropSchema, native: &[u8], out: &mut Vec<u8>) {
    match (prop.quant, prop.kind) {
        (QuantKind::Ss3, PropKind::Quat) => {
            let q = [
                read_f32(&native[0..4]),
                read_f32(&native[4..8]),
                read_f32(&native[8..12]),
                read_f32(&native[12..16]),
            ];
            out.extend_from_slice(&quat_to_ss3(q));
        }
        (QuantKind::Ss3, PropKind::Basis) => {
            let mut m = [0.0f32; 9];
            for (i, slot) in m.iter_mut().enumerate() {
                *slot = read_f32(&native[i * 4..i * 4 + 4]);
            }
            out.extend_from_slice(&quat_to_ss3(basis_to_quat(&m)));
        }
        (QuantKind::Half, PropKind::Vec3 | PropKind::Vec2 | PropKind::F32) => {
            for chunk in native.chunks_exact(4) {
                out.extend_from_slice(&f32_to_f16_bits(read_f32(chunk)).to_le_bytes());
            }
        }
        _ => out.extend_from_slice(native),
    }
}

/// Decode one property's wire bytes into its native slot. Returns bytes consumed, or `None` if
/// `wire` is too short.
fn decode_prop(prop: &PropSchema, wire: &[u8], native: &mut [u8]) -> Option<usize> {
    let width = wire_stride(prop.kind, prop.quant);
    if wire.len() < width {
        return None;
    }
    match (prop.quant, prop.kind) {
        (QuantKind::Ss3, PropKind::Quat) => {
            let q = ss3_to_quat(wire[0..6].try_into().expect("checked width"));
            for (i, &c) in q.iter().enumerate() {
                write_f32(&mut native[i * 4..i * 4 + 4], c);
            }
        }
        (QuantKind::Ss3, PropKind::Basis) => {
            let m = quat_to_basis(ss3_to_quat(wire[0..6].try_into().expect("checked width")));
            for (i, &c) in m.iter().enumerate() {
                write_f32(&mut native[i * 4..i * 4 + 4], c);
            }
        }
        (QuantKind::Half, PropKind::Vec3 | PropKind::Vec2 | PropKind::F32) => {
            for (i, chunk) in wire[..width].chunks_exact(2).enumerate() {
                let bits = u16::from_le_bytes([chunk[0], chunk[1]]);
                write_f32(&mut native[i * 4..i * 4 + 4], f16_bits_to_f32(bits));
            }
        }
        _ => native.copy_from_slice(&wire[..width]),
    }
    Some(width)
}

/// Round-trip one property's native bytes through its wire form, in place.
///
/// This is capture-time canonicalization: after it, the row holds exactly the value a peer will
/// reconstruct from the wire, which is what keeps masks, mispredict compares and resimulation
/// byte-exact across peers. A [`QuantKind::None`] property is untouched.
pub fn canonicalize_value(kind: PropKind, quant: QuantKind, native: &mut [u8]) {
    if quant == QuantKind::None {
        return;
    }
    let prop = PropSchema {
        name: String::new(),
        kind,
        role: crate::protocol::PropRole::State,
        offset: 0,
        quant,
    };
    // Iterate to a FIXED POINT of decode∘encode, not just one round trip. The subtlety is ss3:
    // a near-balanced quaternion (all components ≈ 0.5) can round so a stored small exceeds the
    // sqrt-reconstructed largest, flipping the largest-index choice on re-encode — one pass
    // would leave a row whose re-encoding decodes to different bytes, and the sender/receiver
    // rows would disagree by one quantum for as long as the pose holds. The flip shrinks the
    // imbalance each round, so this converges in one iteration almost always and a few at the
    // pathological boundary; the cap is a safety net, not an expected path.
    let mut wire: Vec<u8> = Vec::with_capacity(8);
    let mut previous: Vec<u8> = Vec::with_capacity(native.len());
    for _ in 0..8 {
        previous.clear();
        previous.extend_from_slice(native);
        wire.clear();
        encode_prop(&prop, native, &mut wire);
        let _ = decode_prop(&prop, &wire, native);
        if native == previous.as_slice() {
            break;
        }
    }
}

/// Encode a full native row to its wire form, appending to `out`.
pub fn encode_row(props: &[PropSchema], row: &[u8], out: &mut Vec<u8>) {
    for prop in props {
        let end = prop.offset + prop.kind.stride();
        encode_prop(prop, &row[prop.offset..end], out);
    }
}

/// Decode a full wire row into a native row. Returns `None` when `wire` is short or `row` is not
/// exactly the native stride.
#[must_use]
pub fn decode_row(props: &[PropSchema], wire: &[u8], row: &mut [u8]) -> Option<usize> {
    let mut cursor = 0usize;
    for prop in props {
        let end = prop.offset + prop.kind.stride();
        if end > row.len() {
            return None;
        }
        let consumed = decode_prop(prop, &wire[cursor..], &mut row[prop.offset..end])?;
        cursor += consumed;
    }
    Some(cursor)
}

/// Total wire bytes the masked properties occupy.
#[must_use]
pub fn masked_wire_size(props: &[PropSchema], mask: &[bool]) -> usize {
    props
        .iter()
        .zip(mask)
        .filter(|(_, &changed)| changed)
        .map(|(prop, _)| wire_stride(prop.kind, prop.quant))
        .sum()
}

/// Append the masked properties of a native row, wire-encoded, in schema order.
pub fn write_masked_wire(props: &[PropSchema], mask: &[bool], row: &[u8], out: &mut Vec<u8>) {
    for (prop, &changed) in props.iter().zip(mask) {
        if changed {
            let end = prop.offset + prop.kind.stride();
            encode_prop(prop, &row[prop.offset..end], out);
        }
    }
}

/// Copy the masked properties from a wire payload (schema order, changed-only) into a native row.
///
/// Returns wire bytes consumed, or `None` on a short/hostile payload.
#[must_use]
pub fn apply_masked_wire(
    props: &[PropSchema],
    mask: &[bool],
    payload: &[u8],
    row: &mut [u8],
) -> Option<usize> {
    let mut cursor = 0usize;
    for (prop, &changed) in props.iter().zip(mask) {
        if !changed {
            continue;
        }
        let end = prop.offset + prop.kind.stride();
        if end > row.len() {
            return None;
        }
        let consumed = decode_prop(prop, &payload[cursor..], &mut row[prop.offset..end])?;
        cursor += consumed;
    }
    Some(cursor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{PropRole, SchemaBuilder};

    #[test]
    fn f16_round_trips_representable_values() {
        for v in [
            0.0f32,
            1.0,
            -1.0,
            0.5,
            1.5,
            65504.0,
            -65504.0,
            0.000061035156,
        ] {
            let bits = f32_to_f16_bits(v);
            let back = f16_bits_to_f32(bits);
            assert_eq!(back, v, "f16 must round-trip {v} exactly");
            // Idempotent: a decoded value re-encodes to the same bits.
            assert_eq!(f32_to_f16_bits(back), bits);
        }
    }

    #[test]
    fn f16_clamps_poison_and_overflow() {
        assert_eq!(f16_bits_to_f32(f32_to_f16_bits(f32::NAN)), 0.0);
        assert_eq!(f16_bits_to_f32(f32_to_f16_bits(f32::INFINITY)), 0.0);
        assert_eq!(f16_bits_to_f32(f32_to_f16_bits(1.0e9)), 65504.0);
        assert_eq!(f16_bits_to_f32(f32_to_f16_bits(-1.0e9)), -65504.0);
        // Hostile inf/NaN bit patterns decode to zero, never propagate.
        assert_eq!(f16_bits_to_f32(0x7c00), 0.0);
        assert_eq!(f16_bits_to_f32(0x7e00), 0.0);
        assert_eq!(f16_bits_to_f32(0xfc00), 0.0);
    }

    #[test]
    fn f16_error_is_bounded() {
        let mut v = -100.0f32;
        while v < 100.0 {
            let back = f16_bits_to_f32(f32_to_f16_bits(v));
            let bound = (v.abs() * 0.001).max(1e-4);
            assert!(
                (back - v).abs() <= bound,
                "f16 error too large at {v}: {back}"
            );
            v += 0.137;
        }
    }

    #[test]
    fn ss3_round_trips_rotations_within_tolerance() {
        let cases: [[f32; 4]; 6] = [
            [0.0, 0.0, 0.0, 1.0],
            [1.0, 0.0, 0.0, 0.0],
            [0.5, 0.5, 0.5, 0.5],
            [0.3, -0.4, 0.5, 0.707],
            [-0.1, 0.2, -0.3, 0.9],
            [0.7, 0.1, -0.1, -0.7],
        ];
        for raw in cases {
            let norm: f32 = raw.iter().map(|c| c * c).sum::<f32>().sqrt();
            let q: Vec<f32> = raw.iter().map(|c| c / norm).collect();
            let q: [f32; 4] = [q[0], q[1], q[2], q[3]];
            let back = ss3_to_quat(quat_to_ss3(q));
            // Compare as rotations: |dot| near 1 (q and -q are the same rotation).
            let dot: f32 = q.iter().zip(&back).map(|(a, b)| a * b).sum();
            assert!(
                dot.abs() > 0.999_999,
                "ss3 rotation error too large: {q:?} -> {back:?} (dot {dot})"
            );
            let n: f32 = back.iter().map(|c| c * c).sum::<f32>().sqrt();
            assert!((n - 1.0).abs() < 1e-5, "decoded quat must be unit: {n}");
        }
    }

    #[test]
    fn ss3_is_total_over_hostile_bytes() {
        for pattern in [[0xffu8; 6], [0u8; 6], [0xab, 0xcd, 0xef, 0x01, 0x23, 0x45]] {
            let q = ss3_to_quat(pattern);
            assert!(
                q.iter().all(|c| c.is_finite()),
                "hostile ss3 decoded to poison: {q:?}"
            );
            let n: f32 = q.iter().map(|c| c * c).sum::<f32>().sqrt();
            assert!((n - 1.0).abs() < 1e-5);
        }
        // Poison quats canonicalize to identity rather than encoding garbage.
        let identity = ss3_to_quat(quat_to_ss3([f32::NAN, 0.0, 0.0, f32::INFINITY]));
        let dot: f32 = identity
            .iter()
            .zip(&[0.0f32, 0.0, 0.0, 1.0])
            .map(|(a, b)| a * b)
            .sum();
        assert!(dot.abs() > 0.999_999);
    }

    #[test]
    fn canonicalize_reaches_a_fixed_point_even_on_balanced_quats() {
        // The pathological ss3 case: all components ≈ 0.5, where rounding can push a stored
        // small above the reconstructed largest and flip the index choice. Canonicalization must
        // still land on bytes that survive another round trip unchanged — the sender's row and
        // the receiver's decode must agree byte-exact.
        let cases: [[f32; 4]; 4] = [
            [0.5, 0.5, 0.5, 0.5],
            [0.500001, 0.499999, 0.5, 0.5],
            [-0.5, 0.5, -0.5, 0.5],
            [0.5000, 0.5001, 0.4999, 0.5000],
        ];
        for q in cases {
            let mut row = [0u8; 16];
            for (i, &c) in q.iter().enumerate() {
                row[i * 4..i * 4 + 4].copy_from_slice(&c.to_le_bytes());
            }
            canonicalize_value(PropKind::Quat, QuantKind::Ss3, &mut row);
            let once = row;
            canonicalize_value(PropKind::Quat, QuantKind::Ss3, &mut row);
            assert_eq!(row, once, "canonicalize must be a fixed point for {q:?}");
            // And the wire trip must reproduce the canonical bytes exactly (receiver == sender).
            let prop = PropSchema {
                name: String::new(),
                kind: PropKind::Quat,
                role: PropRole::State,
                offset: 0,
                quant: QuantKind::Ss3,
            };
            let mut wire = Vec::new();
            encode_prop(&prop, &row, &mut wire);
            let mut decoded = [0u8; 16];
            assert_eq!(decode_prop(&prop, &wire, &mut decoded), Some(6));
            assert_eq!(
                decoded, row,
                "wire decode must reproduce the canonical row for {q:?}"
            );
        }
    }

    #[test]
    fn basis_quat_conversions_invert_for_rotations() {
        // A rotation about a skew axis, built from an exactly-representable quat round trip.
        let q = ss3_to_quat(quat_to_ss3([0.36, 0.48, 0.6, 0.52]));
        let m = quat_to_basis(q);
        let q2 = basis_to_quat(&m);
        let dot: f32 = q.iter().zip(&q2).map(|(a, b)| a * b).sum();
        assert!(
            dot.abs() > 0.999_99,
            "basis<->quat drifted: {q:?} vs {q2:?}"
        );
    }

    fn quantized_schema() -> SchemaBuilder {
        let mut schema = SchemaBuilder::new();
        schema.push("pos", PropKind::Vec3, PropRole::State); // lossless
        schema.push_quantized("orient", PropKind::Quat, PropRole::State, QuantKind::Ss3);
        schema.push_quantized("vel", PropKind::Vec3, PropRole::State, QuantKind::Half);
        schema.push("flags", PropKind::I32, PropRole::State);
        schema
    }

    fn canonical_row(schema: &SchemaBuilder) -> Vec<u8> {
        let mut row = vec![0u8; schema.row_stride()];
        // pos
        row[0..4].copy_from_slice(&1.5f32.to_le_bytes());
        row[4..8].copy_from_slice(&(-2.25f32).to_le_bytes());
        row[8..12].copy_from_slice(&74.0f32.to_le_bytes());
        // orient (normalized in-canonicalization)
        row[12..16].copy_from_slice(&0.3f32.to_le_bytes());
        row[16..20].copy_from_slice(&(-0.4f32).to_le_bytes());
        row[20..24].copy_from_slice(&0.5f32.to_le_bytes());
        row[24..28].copy_from_slice(&0.707f32.to_le_bytes());
        // vel
        row[28..32].copy_from_slice(&3.125f32.to_le_bytes());
        row[32..36].copy_from_slice(&(-0.5f32).to_le_bytes());
        row[36..40].copy_from_slice(&12.75f32.to_le_bytes());
        // flags
        row[40..44].copy_from_slice(&7i32.to_le_bytes());
        for prop in schema.props() {
            let end = prop.offset + prop.kind.stride();
            canonicalize_value(prop.kind, prop.quant, &mut row[prop.offset..end]);
        }
        row
    }

    #[test]
    fn canonical_rows_survive_the_wire_byte_exact() {
        let schema = quantized_schema();
        let row = canonical_row(&schema);
        assert_eq!(wire_row_stride(schema.props()), 12 + 6 + 6 + 4);

        let mut wire = Vec::new();
        encode_row(schema.props(), &row, &mut wire);
        assert_eq!(wire.len(), wire_row_stride(schema.props()));

        let mut decoded = vec![0u8; schema.row_stride()];
        assert_eq!(
            decode_row(schema.props(), &wire, &mut decoded),
            Some(wire.len())
        );
        assert_eq!(
            decoded, row,
            "a canonical row must survive the wire byte-exact"
        );
    }

    #[test]
    fn masked_wire_round_trips_changed_props() {
        let schema = quantized_schema();
        let base = canonical_row(&schema);
        let mut row = base.clone();
        // Change vel only.
        row[28..32].copy_from_slice(&9.5f32.to_le_bytes());
        for prop in schema.props() {
            let end = prop.offset + prop.kind.stride();
            canonicalize_value(prop.kind, prop.quant, &mut row[prop.offset..end]);
        }

        let mut mask = Vec::new();
        crate::columnar::changed_mask(schema.props(), &base, &row, &mut mask);
        assert_eq!(mask, vec![false, false, true, false]);
        assert_eq!(masked_wire_size(schema.props(), &mask), 6);

        let mut payload = Vec::new();
        write_masked_wire(schema.props(), &mask, &row, &mut payload);
        let mut rebuilt = base.clone();
        assert_eq!(
            apply_masked_wire(schema.props(), &mask, &payload, &mut rebuilt),
            Some(payload.len())
        );
        assert_eq!(rebuilt, row);
    }

    #[test]
    fn decode_row_rejects_short_wire() {
        let schema = quantized_schema();
        let row = canonical_row(&schema);
        let mut wire = Vec::new();
        encode_row(schema.props(), &row, &mut wire);
        wire.truncate(wire.len() - 1);
        let mut decoded = vec![0u8; schema.row_stride()];
        assert_eq!(decode_row(schema.props(), &wire, &mut decoded), None);
    }
}
