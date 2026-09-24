const MAGIC: &[u8; 8] = b"SPCX\0\0\0\x01";
const MAX_COEFFICIENTS: usize = 1 << 28;
const SCAN: [usize; 64] = [
    0, 1, 8, 9, 2, 3, 10, 11, 16, 17, 24, 25, 18, 19, 26, 27, 4, 5, 12, 20, 13, 6, 7, 14, 21, 28,
    29, 22, 15, 23, 30, 31, 32, 33, 40, 48, 41, 34, 35, 42, 49, 56, 57, 50, 43, 36, 37, 44, 51, 58,
    59, 52, 45, 38, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];
fn validate(
    shape: [usize; 3],
    quant: &[[[u8; 64]; 3]],
    len: usize,
) -> Result<(usize, usize, usize), String> {
    if shape.contains(&0) || quant.len() != shape[0] {
        return Err("invalid MOV cross-channel dimensions".into());
    }
    let blocks = shape[1]
        .checked_mul(shape[2])
        .ok_or("MOV cross-channel size overflow")?;
    let plane = blocks
        .checked_mul(64)
        .ok_or("MOV cross-channel size overflow")?;
    let total = plane
        .checked_mul(3)
        .and_then(|n| n.checked_mul(shape[0]))
        .ok_or("MOV cross-channel size overflow")?;
    if total > MAX_COEFFICIENTS || len != total || quant.iter().flatten().flatten().any(|&q| q == 0)
    {
        return Err("invalid MOV cross-channel coefficients or quantization".into());
    }
    Ok((blocks, plane, total))
}
fn zeros<T: Clone>(length: usize, value: T) -> Result<Vec<T>, String> {
    let mut v = Vec::new();
    v.try_reserve_exact(length)
        .map_err(|_| "MOV cross-channel memory limit")?;
    v.resize(length, value);
    Ok(v)
}
fn cancelled(cancel: &mut dyn FnMut() -> bool) -> Result<(), String> {
    if cancel() {
        Err("error.cancelled".into())
    } else {
        Ok(())
    }
}
fn bits(v: i32) -> u32 {
    if v == 0 {
        0
    } else {
        2 * (32 - v.unsigned_abs().leading_zeros()) + 1
    }
}
fn slope(numerator: f64, denominator: f64) -> i16 {
    if !numerator.is_finite() || !denominator.is_finite() || denominator <= 0.0 {
        0
    } else {
        (32.0 * numerator / denominator)
            .round()
            .clamp(-256.0, 256.0) as i16
    }
}
fn prediction(y: i16, u: i16, n1: i16, n2: i16, qy: u8, qu: u8, qv: u8) -> i64 {
    let numerator =
        i64::from(n1) * i64::from(y) * i64::from(qy) + i64::from(n2) * i64::from(u) * i64::from(qu);
    let denominator = 32 * i64::from(qv);
    (numerator + denominator / 2).div_euclid(denominator)
}
pub(crate) fn encode(
    values: &[i16],
    shape: [usize; 3],
    quant: &[[[u8; 64]; 3]],
    cancel: &mut dyn FnMut() -> bool,
) -> Result<(Vec<i16>, Vec<u8>), String> {
    cancelled(cancel)?;
    let (blocks, plane, total) = validate(shape, quant, values.len())?;
    let count = total / 64;
    let mut out = Vec::new();
    out.try_reserve_exact(total)
        .map_err(|_| "MOV cross-channel memory limit")?;
    out.extend_from_slice(values);
    let mut modes = zeros(count, 0u8)?;
    let mut first = zeros(count, 0i16)?;
    let mut second = zeros(count, 0i16)?;
    for (f, frame_quant) in quant.iter().enumerate() {
        for ch in 1..3 {
            for block in 0..blocks {
                if block & 127 == 0 {
                    cancelled(cancel)?;
                }
                let y_at = f * 3 * plane + block * 64;
                let u_at = y_at + plane;
                let at = y_at + ch * plane;
                let index = (f * 3 + ch) * blocks + block;
                let mut best = values[at..at + 64]
                    .iter()
                    .map(|&v| bits(i32::from(v)))
                    .sum::<u32>();
                if best <= 13 {
                    continue;
                }
                let mut xx = 0.0;
                let mut uu = 0.0;
                let mut xu = 0.0;
                let mut xv = 0.0;
                let mut uv = 0.0;
                for k in 1..64 {
                    let natural = SCAN[k];
                    let q = f64::from(frame_quant[ch][natural]);
                    let x = f64::from(values[y_at + k]) * f64::from(frame_quant[0][natural]) / q;
                    let u = f64::from(values[u_at + k]) * f64::from(frame_quant[1][natural]) / q;
                    let v = f64::from(values[at + k]);
                    xx += x * x;
                    uu += u * u;
                    xu += x * u;
                    xv += x * v;
                    uv += u * v;
                }
                let det = xx * uu - xu * xu;
                let joint = if det > 1.0 {
                    (slope(xv * uu - uv * xu, det), slope(uv * xx - xv * xu, det))
                } else {
                    (0, 0)
                };
                let candidates = [
                    (2u8, slope(xv, xx), 0i16),
                    (3, 0, slope(uv, uu)),
                    (4, joint.0, joint.1),
                ];
                for &(mode, n1, n2) in &candidates[..if ch == 1 { 1 } else { 3 }] {
                    let mut trial = [0i16; 64];
                    let mut score = if mode == 4 { 26 } else { 13 };
                    let mut valid = true;
                    for k in 0..64 {
                        let natural = SCAN[k];
                        let pred = if k == 0 {
                            0
                        } else {
                            prediction(
                                values[y_at + k],
                                values[u_at + k],
                                n1,
                                n2,
                                frame_quant[0][natural],
                                frame_quant[1][natural],
                                frame_quant[ch][natural],
                            )
                        };
                        let delta = i64::from(values[at + k]) - pred;
                        let Ok(value) = i16::try_from(delta) else {
                            valid = false;
                            break;
                        };
                        trial[k] = value;
                        score += bits(i32::from(value));
                    }
                    if valid && score < best {
                        best = score;
                        modes[index] = mode;
                        first[index] = n1;
                        second[index] = n2;
                        out[at..at + 64].copy_from_slice(&trial);
                    }
                }
            }
        }
    }
    cancelled(cancel)?;
    let mut raw = Vec::new();
    raw.try_reserve_exact(count * 5)
        .map_err(|_| "MOV cross-channel memory limit")?;
    raw.extend_from_slice(&modes);
    for parameters in [&first, &second] {
        for &value in parameters {
            raw.extend_from_slice(&value.to_le_bytes());
        }
    }
    let encoded = zstd::bulk::compress(&raw, 12).map_err(|e| {
        crate::locale::message("codec.mov_cross_channel_compression", &[e.to_string()])
    })?;
    let mut side = Vec::from(*MAGIC);
    side.extend_from_slice(&encoded);
    cancelled(cancel)?;
    Ok((out, side))
}
pub(crate) fn decode(
    mut values: Vec<i16>,
    side: &[u8],
    shape: [usize; 3],
    quant: &[[[u8; 64]; 3]],
    cancel: &mut dyn FnMut() -> bool,
) -> Result<Vec<i16>, String> {
    cancelled(cancel)?;
    let (blocks, plane, total) = validate(shape, quant, values.len())?;
    let count = total / 64;
    let expected = count * 5;
    if side.len() < 9
        || side.get(..8) != Some(MAGIC.as_slice())
        || side.len() > expected + expected / 256 + 1024
    {
        return Err("invalid MOV cross-channel parameter stream".into());
    }
    use std::io::Read;
    let mut decoder = zstd::stream::read::Decoder::with_buffer(&side[8..])
        .map_err(|e| {
            crate::locale::message("codec.mov_cross_channel_parameters", &[e.to_string()])
        })?
        .single_frame();
    let mut raw = Vec::new();
    raw.try_reserve_exact(expected + 1)
        .map_err(|_| "MOV cross-channel memory limit")?;
    decoder
        .by_ref()
        .take((expected + 1) as u64)
        .read_to_end(&mut raw)
        .map_err(|e| {
            crate::locale::message("codec.mov_cross_channel_parameters", &[e.to_string()])
        })?;
    if raw.len() != expected {
        return Err("MOV cross-channel parameter length mismatch".into());
    }
    if !decoder.finish().is_empty() {
        return Err("trailing MOV cross-channel parameter bytes".into());
    }
    let parameter = |index: usize, offset: usize| {
        i16::from_le_bytes([raw[offset + index * 2], raw[offset + index * 2 + 1]])
    };
    for (f, frame_quant) in quant.iter().enumerate() {
        for ch in 0..3 {
            for block in 0..blocks {
                if block & 127 == 0 {
                    cancelled(cancel)?;
                }
                let index = (f * 3 + ch) * blocks + block;
                let mode = raw[index];
                let n1 = parameter(index, count);
                let n2 = parameter(index, count * 3);
                let valid = match mode {
                    0 => n1 == 0 && n2 == 0,
                    2 => ch > 0 && n2 == 0,
                    3 => ch == 2 && n1 == 0,
                    4 => ch == 2,
                    _ => false,
                };
                if !valid || !(-256..=256).contains(&n1) || !(-256..=256).contains(&n2) {
                    return Err("invalid MOV cross-channel predictor".into());
                }
                if mode == 0 {
                    continue;
                }
                let y_at = f * 3 * plane + block * 64;
                let u_at = y_at + plane;
                let at = y_at + ch * plane;
                for k in 1..64 {
                    let natural = SCAN[k];
                    let pred = prediction(
                        values[y_at + k],
                        values[u_at + k],
                        n1,
                        n2,
                        frame_quant[0][natural],
                        frame_quant[1][natural],
                        frame_quant[ch][natural],
                    );
                    values[at + k] = i16::try_from(i64::from(values[at + k]) + pred)
                        .map_err(|_| "MOV cross-channel reconstruction overflow")?;
                }
            }
        }
    }
    cancelled(cancel)?;
    Ok(values)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> (Vec<i16>, [usize; 3], Vec<[[u8; 64]; 3]>) {
        let shape = [2, 2, 3];
        let plane = shape[1] * shape[2] * 64;
        let mut v = vec![0i16; shape[0] * 3 * plane];
        for f in 0..shape[0] {
            for i in 0..plane {
                let y = ((i * 37 + f * 11) % 257) as i16 - 128;
                v[f * 3 * plane + i] = y * 4;
                v[f * 3 * plane + plane + i] = y * 2;
                v[f * 3 * plane + 2 * plane + i] = -y;
            }
        }
        (v, shape, vec![[[4; 64]; 3]; shape[0]])
    }
    #[test]
    fn correlated_channels_round_trip_and_reduce_magnitudes() {
        let (a, shape, q) = sample();
        let (c, side) = encode(&a, shape, &q, &mut || false).unwrap();
        assert!(c.iter().filter(|&&v| v != 0).count() < a.iter().filter(|&&v| v != 0).count());
        assert_eq!(decode(c, &side, shape, &q, &mut || false).unwrap(), a);
    }
    #[test]
    fn quantization_alignment_and_extreme_values_round_trip() {
        let (mut a, shape, mut q) = sample();
        for (i, v) in a.iter_mut().enumerate() {
            if i % 17 == 0 {
                *v = i16::MIN;
            }
            if i % 19 == 0 {
                *v = i16::MAX;
            }
        }
        for f in &mut q {
            for (ch, p) in f.iter_mut().enumerate() {
                for (k, v) in p.iter_mut().enumerate() {
                    *v = (1 + (k * 11 + ch * 31) % 255) as u8;
                }
            }
        }
        let (c, s) = encode(&a, shape, &q, &mut || false).unwrap();
        assert_eq!(decode(c, &s, shape, &q, &mut || false).unwrap(), a);
    }
    #[test]
    fn rejects_bad_shapes_streams_parameters_and_cancellation() {
        let (a, shape, q) = sample();
        assert!(encode(&a, [usize::MAX, 1, 1], &q, &mut || false).is_err());
        assert_eq!(
            encode(&a, shape, &q, &mut || true).unwrap_err(),
            "error.cancelled"
        );
        let (c, side) = encode(&a, shape, &q, &mut || false).unwrap();
        assert!(decode(c.clone(), &side[..8], shape, &q, &mut || false).is_err());
        let count = a.len() / 64;
        let mut raw = zstd::bulk::decompress(&side[8..], count * 5).unwrap();
        raw[0] = 4;
        let mut bad = Vec::from(*MAGIC);
        bad.extend_from_slice(&zstd::bulk::compress(&raw, 1).unwrap());
        assert!(decode(c, &bad, shape, &q, &mut || false).is_err());
    }
    #[test]
    fn rejects_concatenated_empty_and_skippable_zstd_frames() {
        let values = vec![0i16; 192];
        let shape = [1, 1, 1];
        let q = [[[4u8; 64]; 3]];
        let (coded, side) = encode(&values, shape, &q, &mut || false).unwrap();
        let mut empty = side.clone();
        empty.extend_from_slice(&zstd::bulk::compress(&[], 1).unwrap());
        assert!(decode(coded.clone(), &empty, shape, &q, &mut || false).is_err());
        let mut skipped = side;
        skipped.extend_from_slice(&0x184d2a50u32.to_le_bytes());
        skipped.extend_from_slice(&4u32.to_le_bytes());
        skipped.extend_from_slice(b"junk");
        assert!(decode(coded, &skipped, shape, &q, &mut || false).is_err());
    }
}
