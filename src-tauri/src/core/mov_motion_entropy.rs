const MAX_COEFFICIENTS: usize = 1 << 28;
const MAX_BYTES: usize = 512 * 1024 * 1024;
const CONTEXTS: usize = 500_000;
const K: usize = 64;
fn dimensions(shape: [usize; 3], modes: &[u8]) -> Result<(usize, usize, usize), String> {
    if shape.contains(&0) {
        return Err("invalid MOV motion dimensions".into());
    }
    let plane = shape[1]
        .checked_mul(shape[2])
        .and_then(|n| n.checked_mul(K))
        .ok_or("MOV motion size overflow")?;
    let frame = plane.checked_mul(3).ok_or("MOV motion size overflow")?;
    let total = frame
        .checked_mul(shape[0])
        .ok_or("MOV motion size overflow")?;
    if total > MAX_COEFFICIENTS || modes.len() != total / (3 * K) || modes.iter().any(|&m| m > 2) {
        return Err("invalid MOV motion modes or coefficient limit".into());
    }
    Ok((plane, frame, total))
}
fn zeros<T: Clone>(len: usize, value: T) -> Result<Vec<T>, String> {
    let mut out = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|_| "MOV motion memory limit")?;
    out.resize(len, value);
    Ok(out)
}
fn cancelled(cancel: &mut dyn FnMut() -> bool) -> Result<(), String> {
    if cancel() {
        Err("error.cancelled".into())
    } else {
        Ok(())
    }
}
struct RangeEncode {
    low: u64,
    range: u32,
    cache: u8,
    cache_size: usize,
    bytes: Vec<u8>,
}
impl RangeEncode {
    fn new() -> Self {
        Self {
            low: 0,
            range: u32::MAX,
            cache: 0,
            cache_size: 1,
            bytes: Vec::new(),
        }
    }
    fn shift(&mut self) {
        if (self.low as u32) < 0xff000000 || self.low >> 32 != 0 {
            let carry = (self.low >> 32) as u8;
            let mut byte = self.cache;
            loop {
                self.bytes.push(byte.wrapping_add(carry));
                byte = 255;
                self.cache_size -= 1;
                if self.cache_size == 0 {
                    break;
                }
            }
            self.cache = (self.low as u32 >> 24) as u8;
        }
        self.cache_size += 1;
        self.low = ((self.low as u32) << 8) as u64;
    }
    fn bit(&mut self, bit: bool, probability: &mut u16) {
        let boundary = (self.range >> 12) * *probability as u32;
        if bit {
            self.low += boundary as u64;
            self.range -= boundary;
        } else {
            self.range = boundary;
        }
        update_probability(probability, bit);
        while self.range < 0x01000000 {
            self.range <<= 8;
            self.shift();
        }
    }
    fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift();
        }
        self.bytes
    }
}

struct RangeDecode<'a> {
    bytes: &'a [u8],
    at: usize,
    range: u32,
    code: u32,
}
impl<'a> RangeDecode<'a> {
    fn new(bytes: &'a [u8]) -> Result<Self, String> {
        if bytes.len() < 5 || bytes.first() != Some(&0) {
            return Err("truncated ProRes model range stream".into());
        }
        let mut decoder = Self {
            bytes,
            at: 0,
            range: u32::MAX,
            code: 0,
        };
        for _ in 0..5 {
            decoder.code = (decoder.code << 8) | decoder.next()? as u32;
        }
        Ok(decoder)
    }
    fn next(&mut self) -> Result<u8, String> {
        let byte = *self
            .bytes
            .get(self.at)
            .ok_or("truncated ProRes model codeword")?;
        self.at += 1;
        Ok(byte)
    }
    fn bit(&mut self, probability: &mut u16) -> Result<bool, String> {
        let boundary = (self.range >> 12) * *probability as u32;
        let bit = self.code >= boundary;
        if bit {
            self.code -= boundary;
            self.range -= boundary;
        } else {
            self.range = boundary;
        }
        update_probability(probability, bit);
        while self.range < 0x01000000 {
            self.range <<= 8;
            self.code = (self.code << 8) | self.next()? as u32;
        }
        Ok(bit)
    }
}
fn update_probability(probability: &mut u16, bit: bool) {
    if bit {
        *probability -= (*probability >> 5).max(1);
    } else {
        *probability += ((4096 - *probability) >> 5).max(1);
    }
    *probability = (*probability).clamp(1, 4095);
}

fn bucket(v: i32) -> usize {
    if v == 0 {
        0
    } else {
        (32 - v.unsigned_abs().leading_zeros() as usize).min(7)
    }
}
#[allow(clippy::too_many_arguments)]
fn indexes(
    a: &[i16],
    at: usize,
    ch: usize,
    y: usize,
    x: usize,
    k: usize,
    mode: usize,
    width: usize,
    frame: usize,
) -> (usize, usize, usize, usize) {
    let left = if x > 0 { a[at - K] as i32 } else { 0 };
    let above = if y > 0 { a[at - width * K] as i32 } else { 0 };
    let temporal = if at >= frame { a[at - frame] as i32 } else { 0 };
    let previous = if k > 0 { a[at - 1] as i32 } else { 0 };
    let context = ch * 64 + k;
    let scale = bucket(left.abs().max(above.abs()));
    let zero = 512
        + ((context * 8 + scale) * 3 + mode) * 4
        + usize::from(temporal == 0) * 2
        + usize::from(previous == 0);
    let exponent = 20000 + ((context * 8 + scale) * 3 + mode) * 16;
    let mantissa = 95000 + (context * 8 + scale) * 256;
    let sign = 490000
        + (context * 3 + mode) * 8
        + usize::from(left < 0) * 4
        + usize::from(above < 0) * 2
        + usize::from(temporal < 0);
    (zero, exponent, mantissa, sign)
}
pub(crate) fn encode(
    values: &[i16],
    shape: [usize; 3],
    modes: &[u8],
    cancel: &mut dyn FnMut() -> bool,
) -> Result<Vec<u8>, String> {
    cancelled(cancel)?;
    let (plane, frame, total) = dimensions(shape, modes)?;
    if values.len() != total {
        return Err("MOV motion coefficient length mismatch".into());
    }
    let mut coder = RangeEncode::new();
    let mut probabilities = zeros(CONTEXTS, 2048u16)?;
    for f in 0..shape[0] {
        for ch in 0..3 {
            for y in 0..shape[1] {
                for x in 0..shape[2] {
                    let block = y * shape[2] + x;
                    if block & 127 == 0 {
                        cancelled(cancel)?;
                        if coder.bytes.len() > MAX_BYTES {
                            return Err("MOV motion entropy limit".into());
                        }
                    }
                    let base = f * frame + ch * plane + block * K;
                    let mode = modes[f * plane / K + block] as usize;
                    let zero = values[base..base + K].iter().all(|&v| v == 0);
                    let prior_zero = f > 0
                        && values[base - frame..base - frame + K]
                            .iter()
                            .all(|&v| v == 0);
                    let zctx = (ch * 3 + mode) * 2 + usize::from(prior_zero);
                    coder.bit(zero, &mut probabilities[zctx]);
                    if zero {
                        continue;
                    }
                    let copy =
                        f > 0 && values[base..base + K] == values[base - frame..base - frame + K];
                    coder.bit(copy, &mut probabilities[64 + zctx]);
                    if copy {
                        continue;
                    }
                    for k in 0..K {
                        let at = base + k;
                        let (z, e, m, s) = indexes(values, at, ch, y, x, k, mode, shape[2], frame);
                        let v = values[at] as i32;
                        coder.bit(v != 0, &mut probabilities[z]);
                        if v == 0 {
                            continue;
                        }
                        let mag = v.unsigned_abs();
                        let exp = 31 - mag.leading_zeros() as usize;
                        for b in 0..exp {
                            coder.bit(true, &mut probabilities[e + b]);
                        }
                        coder.bit(false, &mut probabilities[e + exp]);
                        for b in (0..exp).rev() {
                            coder.bit(mag & (1 << b) != 0, &mut probabilities[m + exp * 16 + b]);
                        }
                        coder.bit(v < 0, &mut probabilities[s]);
                    }
                }
            }
        }
    }
    cancelled(cancel)?;
    let result = coder.finish();
    if result.len() > MAX_BYTES {
        return Err("MOV motion entropy limit".into());
    }
    Ok(result)
}
pub(crate) fn decode(
    bytes: &[u8],
    shape: [usize; 3],
    modes: &[u8],
    cancel: &mut dyn FnMut() -> bool,
) -> Result<Vec<i16>, String> {
    cancelled(cancel)?;
    let (plane, frame, total) = dimensions(shape, modes)?;
    if bytes.len() > MAX_BYTES {
        return Err("MOV motion entropy limit".into());
    }
    let mut coder = RangeDecode::new(bytes)?;
    let mut probabilities = zeros(CONTEXTS, 2048u16)?;
    let mut values = zeros(total, 0i16)?;
    for f in 0..shape[0] {
        for ch in 0..3 {
            for y in 0..shape[1] {
                for x in 0..shape[2] {
                    let block = y * shape[2] + x;
                    if block & 127 == 0 {
                        cancelled(cancel)?;
                    }
                    let base = f * frame + ch * plane + block * K;
                    let mode = modes[f * plane / K + block] as usize;
                    let prior_zero = f > 0
                        && values[base - frame..base - frame + K]
                            .iter()
                            .all(|&v| v == 0);
                    let zctx = (ch * 3 + mode) * 2 + usize::from(prior_zero);
                    if coder.bit(&mut probabilities[zctx])? {
                        continue;
                    }
                    if coder.bit(&mut probabilities[64 + zctx])? {
                        if f == 0 {
                            return Err("invalid first-frame MOV motion copy".into());
                        }
                        values.copy_within(base - frame..base - frame + K, base);
                        continue;
                    }
                    for k in 0..K {
                        let at = base + k;
                        let (z, e, m, s) = indexes(&values, at, ch, y, x, k, mode, shape[2], frame);
                        let mut v = 0i32;
                        if coder.bit(&mut probabilities[z])? {
                            let mut exp = 0;
                            while coder.bit(&mut probabilities[e + exp])? {
                                exp += 1;
                                if exp >= 16 {
                                    return Err("MOV motion exponent overflow".into());
                                }
                            }
                            let mut mag = 1i32 << exp;
                            for b in (0..exp).rev() {
                                if coder.bit(&mut probabilities[m + exp * 16 + b])? {
                                    mag |= 1 << b;
                                }
                            }
                            v = if coder.bit(&mut probabilities[s])? {
                                -mag
                            } else {
                                mag
                            };
                        }
                        values[at] =
                            i16::try_from(v).map_err(|_| "MOV motion coefficient overflow")?;
                    }
                }
            }
        }
    }
    cancelled(cancel)?;
    if coder.at != bytes.len() {
        return Err("trailing MOV motion entropy bytes".into());
    }
    Ok(values)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dynamic_shapes_and_extreme_coefficients_round_trip() {
        for shape in [[1, 1, 1], [3, 2, 3], [4, 1, 5]] {
            let n = shape.iter().product::<usize>() * 3 * 64;
            let mut a = vec![0i16; n];
            for (i, v) in a.iter_mut().enumerate() {
                if i % 5 == 0 {
                    *v = (i as i16).wrapping_mul(797);
                }
            }
            a[0] = i16::MIN;
            a[1] = i16::MAX;
            let modes = (0..n / 192).map(|i| (i % 3) as u8).collect::<Vec<_>>();
            let encoded = encode(&a, shape, &modes, &mut || false).unwrap();
            assert_eq!(decode(&encoded, shape, &modes, &mut || false).unwrap(), a);
            for len in [0, 4, encoded.len() - 1] {
                assert!(decode(&encoded[..len], shape, &modes, &mut || false).is_err());
            }
            let mut extra = encoded;
            extra.push(0);
            assert!(decode(&extra, shape, &modes, &mut || false).is_err());
        }
    }
    #[test]
    fn validates_dimensions_modes_and_cancellation() {
        assert!(encode(&[], [usize::MAX, 2, 2], &[], &mut || false).is_err());
        assert!(encode(&[0; 192], [1, 1, 1], &[3], &mut || false).is_err());
        assert_eq!(
            encode(&[0; 192], [1, 1, 1], &[0], &mut || true).unwrap_err(),
            "error.cancelled"
        );
        assert!(decode(&[0; 5], [0, 1, 1], &[], &mut || false).is_err());
    }
    #[test]
    fn malformed_streams_do_not_panic() {
        let mut state = 21u32;
        for n in 5..128 {
            let mut b = vec![0; n];
            for v in b.iter_mut().skip(1) {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                *v = state as u8;
            }
            let _ = decode(&b, [2, 1, 2], &[0; 4], &mut || false);
        }
    }
}
