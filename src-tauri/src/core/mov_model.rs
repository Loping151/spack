use super::mov_exact::{find_segments_with_cancel, pack_plane, unpack_plane};

const MAGIC: &[u8; 8] = b"SPPM\0\0\0\x01";
const SHARED_MAGIC: &[u8; 8] = b"SPPS\0\0\0\x01";
const LIMIT: usize = 512 * 1024 * 1024;
const MAX_SEGMENTS: usize = 1_000_000;
const MAX_COEFFICIENTS: usize = 1 << 30;
const CONTEXTS: usize = 90100;
const HEADER: usize = 28;
const DESCRIPTOR: usize = 12;
const DICTIONARY_CAPACITY: usize = 1 << 19;

#[derive(Clone)]
pub struct SharedContext {
    probabilities: Vec<u16>,
    dictionary: BlockDictionary,
    files: u64,
}
impl Default for SharedContext {
    fn default() -> Self {
        Self {
            probabilities: vec![2048; CONTEXTS],
            dictionary: BlockDictionary::default(),
            files: 0,
        }
    }
}

#[derive(Clone, Default)]
struct BlockDictionary {
    values: Vec<[i16; 64]>,
    hashes: Vec<u64>,
    index: std::collections::HashMap<u64, u32>,
    next: usize,
}
impl BlockDictionary {
    fn hash(values: &[i16; 64]) -> u64 {
        let mut hash = 0x9e3779b97f4a7c15u64;
        for chunk in values.chunks_exact(4) {
            let word = chunk[0] as u16 as u64
                | ((chunk[1] as u16 as u64) << 16)
                | ((chunk[2] as u16 as u64) << 32)
                | ((chunk[3] as u16 as u64) << 48);
            hash = (hash ^ word).wrapping_mul(0xbf58476d1ce4e5b9);
            hash ^= hash >> 29;
        }
        hash
    }
    fn find(&self, values: &[i16; 64]) -> Option<usize> {
        let index = *self.index.get(&Self::hash(values))? as usize;
        (self.values[index] == *values).then_some(index)
    }
    fn insert(&mut self, values: [i16; 64]) {
        let hash = Self::hash(&values);
        if let Some(&index) = self.index.get(&hash) {
            if self.values[index as usize] == values {
                return;
            }
        }
        let index = if self.values.len() < DICTIONARY_CAPACITY {
            self.values.push(values);
            self.hashes.push(hash);
            self.values.len() - 1
        } else {
            let index = self.next;
            self.next = (self.next + 1) % DICTIONARY_CAPACITY;
            let old = self.hashes[index];
            if self.index.get(&old) == Some(&(index as u32)) {
                self.index.remove(&old);
            }
            self.values[index] = values;
            self.hashes[index] = hash;
            index
        };
        self.index.insert(hash, index as u32);
    }
    fn add_plane(&mut self, coefficients: &[i16], blocks: usize) {
        for block in 0..blocks {
            let mut values = [0i16; 64];
            for frequency in 0..64 {
                values[frequency] = coefficients[frequency * blocks + block];
            }
            self.insert(values);
        }
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
        if bytes.len() < 5 {
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
fn magnitude_bits(value: i32) -> usize {
    let magnitude = value.unsigned_abs();
    if magnitude == 0 {
        0
    } else {
        32 - magnitude.leading_zeros() as usize
    }
}
fn zero_context(
    channel: usize,
    frequency: usize,
    previous: i32,
    left: i32,
    above: i32,
    temporal: bool,
) -> usize {
    128 + (channel * 64 + frequency) * 16
        + usize::from(previous == 0) * 8
        + usize::from(left == 0) * 4
        + usize::from(above == 0) * 2
        + usize::from(temporal)
}
fn emit_value(
    coder: &mut RangeEncode,
    probabilities: &mut [u16],
    value: i32,
    context: usize,
    zero: usize,
    left: i32,
    previous: i32,
) {
    coder.bit(value != 0, &mut probabilities[zero]);
    if value == 0 {
        return;
    }
    let magnitude = value.unsigned_abs();
    let exponent = 31 - magnitude.leading_zeros() as usize;
    for bit in 0..exponent {
        coder.bit(true, &mut probabilities[4096 + context * 128 + bit]);
    }
    coder.bit(false, &mut probabilities[4096 + context * 128 + exponent]);
    for bit in (0..exponent).rev() {
        coder.bit(
            magnitude & (1 << bit) != 0,
            &mut probabilities[32768 + context * 256 + exponent * 16 + bit],
        );
    }
    coder.bit(
        value < 0,
        &mut probabilities
            [85000 + context * 4 + usize::from(left < 0) * 2 + usize::from(previous < 0)],
    );
}
fn read_value(
    coder: &mut RangeDecode<'_>,
    probabilities: &mut [u16],
    context: usize,
    zero: usize,
    left: i32,
    previous: i32,
) -> Result<i32, String> {
    if !coder.bit(&mut probabilities[zero])? {
        return Ok(0);
    }
    let mut exponent = 0;
    while coder.bit(&mut probabilities[4096 + context * 128 + exponent])? {
        exponent += 1;
        if exponent > 15 {
            return Err("ProRes model coefficient overflow".into());
        }
    }
    let mut magnitude = 1u32 << exponent;
    for bit in (0..exponent).rev() {
        if coder.bit(&mut probabilities[32768 + context * 256 + exponent * 16 + bit])? {
            magnitude |= 1 << bit;
        }
    }
    let negative = coder.bit(
        &mut probabilities
            [85000 + context * 4 + usize::from(left < 0) * 2 + usize::from(previous < 0)],
    )?;
    Ok(if negative {
        -(magnitude as i32)
    } else {
        magnitude as i32
    })
}

fn report(
    progress: &mut dyn FnMut(u64, u64) -> bool,
    done: usize,
    total: usize,
) -> Result<(), String> {
    if (done.is_multiple_of(128) || done == total) && !progress(done as u64, total as u64) {
        Err("error.cancelled".into())
    } else {
        Ok(())
    }
}

pub fn encode(raw: &[u8]) -> Result<Vec<u8>, String> {
    encode_with_cancel(raw, &|| false)
}

pub fn encode_with_cancel(raw: &[u8], cancelled: &dyn Fn() -> bool) -> Result<Vec<u8>, String> {
    encode_with_progress(raw, &mut |_, _| !cancelled())
}

pub fn encode_with_progress(
    raw: &[u8],
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Vec<u8>, String> {
    encode_impl(raw, progress, None)
}

pub fn encode_shared_with_progress(
    raw: &[u8],
    state: &mut SharedContext,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Vec<u8>, String> {
    encode_impl(raw, progress, Some(state))
}

fn encode_impl(
    raw: &[u8],
    progress: &mut dyn FnMut(u64, u64) -> bool,
    shared: Option<&mut SharedContext>,
) -> Result<Vec<u8>, String> {
    if raw.len() > LIMIT {
        return Err("MOV model input exceeds memory limit".into());
    }
    report(progress, 0, 1)?;
    let mut segments = find_segments_with_cancel(raw, &mut || !progress(0, 1))?;
    if segments.iter().map(|s| s.blocks * 64).sum::<usize>() > MAX_COEFFICIENTS {
        return Err("MOV model coefficient work limit".into());
    }
    let phase = segments.len();
    let total = phase * 3;
    let mut verified = Vec::new();
    for (index, segment) in segments.drain(..).enumerate() {
        report(progress, index, total)?;
        let bytes = &raw[segment.offset..segment.offset + segment.len];
        if unpack_plane(bytes, segment.blocks)
            .and_then(|c| pack_plane(&c, segment.blocks, segment.len))
            .is_ok_and(|b| b == bytes)
        {
            verified.push(segment);
        }
    }
    segments = verified;
    if segments.is_empty() || segments.len() > MAX_SEGMENTS {
        return Err("MOV has no model-compatible ProRes planes".into());
    }
    segments.sort_unstable_by_key(|s| (s.key, s.frame));
    let use_shared = shared.is_some();
    let header = HEADER + if use_shared { 8 } else { 0 };
    let mut state = shared.as_ref().map(|s| (**s).clone()).unwrap_or_default();
    let mut coder = RangeEncode::new();
    let mut probabilities = std::mem::take(&mut state.probabilities);
    let mut previous_key = None;
    let mut previous = Vec::new();
    let mut descriptors = Vec::with_capacity(segments.len() * DESCRIPTOR);
    for (index, segment) in segments.iter().enumerate() {
        report(progress, phase + index, total)?;
        let coefficients = unpack_plane(
            &raw[segment.offset..segment.offset + segment.len],
            segment.blocks,
        )?;
        let exists = previous_key == Some(segment.key) && previous.len() == coefficients.len();
        if !exists {
            previous = vec![0; coefficients.len()];
        }
        descriptors.extend_from_slice(&(segment.offset as u64).to_le_bytes());
        descriptors.extend_from_slice(&(segment.len as u16).to_le_bytes());
        descriptors.push(segment.blocks as u8);
        descriptors.push((segment.key.2 << 1) | u8::from(!exists));
        let blocks = segment.blocks;
        let channel = segment.key.2 as usize;
        let mut copied = vec![false; blocks];
        let mut temporal = vec![false; blocks];
        let mut residual = vec![0i32; coefficients.len()];
        for block in 0..blocks {
            copied[block] = exists
                && (0..64)
                    .all(|f| coefficients[f * blocks + block] == previous[f * blocks + block]);
            let context = channel * 4
                + usize::from(block > 0 && copied[block - 1]) * 2
                + usize::from(previous[block] == 0);
            coder.bit(copied[block], &mut probabilities[context]);
            if copied[block] {
                continue;
            }
            if use_shared {
                let mut values = [0i16; 64];
                for frequency in 0..64 {
                    values[frequency] = coefficients[frequency * blocks + block];
                }
                let reference = state.dictionary.find(&values);
                coder.bit(reference.is_some(), &mut probabilities[90000 + channel]);
                if let Some(index) = reference {
                    for bit in (0..19).rev() {
                        coder.bit(index & (1 << bit) != 0, &mut probabilities[90016 + bit]);
                    }
                    copied[block] = true;
                    continue;
                }
            }
            let mut direct_cost = 0;
            let mut temporal_cost = 0;
            for frequency in 0..64 {
                let i = frequency * blocks + block;
                direct_cost +=
                    magnitude_bits(coefficients[i] as i32) * 2 + usize::from(coefficients[i] != 0);
                temporal_cost += magnitude_bits(coefficients[i] as i32 - previous[i] as i32) * 2
                    + usize::from(coefficients[i] != previous[i]);
            }
            temporal[block] = temporal_cost < direct_cost;
            let context = 32
                + channel * 4
                + usize::from(block > 0 && temporal[block - 1]) * 2
                + usize::from(previous[block] == 0);
            coder.bit(temporal[block], &mut probabilities[context]);
            for frequency in 0..64 {
                let i = frequency * blocks + block;
                residual[i] = coefficients[i] as i32
                    - if temporal[block] {
                        previous[i] as i32
                    } else {
                        0
                    };
            }
        }
        for frequency in 0..64 {
            for block in 0..blocks {
                if copied[block] {
                    continue;
                }
                let i = frequency * blocks + block;
                let left = if block > 0 { residual[i - 1] } else { 0 };
                let above = if frequency > 0 {
                    residual[i - blocks]
                } else {
                    0
                };
                let p = previous[i] as i32;
                let zero = zero_context(channel, frequency, p, left, above, temporal[block]);
                emit_value(
                    &mut coder,
                    &mut probabilities,
                    residual[i],
                    channel * 64 + frequency,
                    zero,
                    left,
                    p,
                );
            }
        }
        if use_shared {
            state.dictionary.add_plane(&coefficients, blocks);
        }
        previous = coefficients;
        previous_key = Some(segment.key);
        if coder.bytes.len() > LIMIT {
            return Err("MOV model output exceeds memory limit".into());
        }
    }
    let range = coder.finish();
    segments.sort_unstable_by_key(|s| s.offset);
    let occupied: usize = segments.iter().map(|s| s.len).sum();
    let length = header + descriptors.len() + range.len() + raw.len() - occupied;
    if length > LIMIT {
        return Err("MOV model output exceeds memory limit".into());
    }
    let mut out = Vec::new();
    out.try_reserve_exact(length)
        .map_err(|_| "MOV model allocation failed")?;
    out.extend_from_slice(if use_shared { SHARED_MAGIC } else { MAGIC });
    out.extend_from_slice(&(raw.len() as u64).to_le_bytes());
    out.extend_from_slice(&(segments.len() as u32).to_le_bytes());
    out.extend_from_slice(&(range.len() as u64).to_le_bytes());
    if use_shared {
        out.extend_from_slice(&state.files.to_le_bytes());
    }
    out.extend_from_slice(&descriptors);
    out.extend_from_slice(&range);
    let mut p = 0;
    for segment in segments {
        out.extend_from_slice(&raw[p..segment.offset]);
        p = segment.offset + segment.len;
    }
    out.extend_from_slice(&raw[p..]);
    let mut verification_progress = |done: u64, count: u64| {
        progress(
            (phase * 2) as u64 + done * phase as u64 / count.max(1),
            total as u64,
        )
    };
    let mut verification_state = shared.as_ref().map(|s| (**s).clone());
    if decode_impl(
        &out,
        &mut verification_progress,
        verification_state.as_mut(),
    )? != raw
    {
        return Err("MOV model byte-exact verification failed".into());
    }
    report(progress, total, total)?;
    if let Some(shared) = shared {
        state.probabilities = probabilities;
        state.files = state
            .files
            .checked_add(1)
            .ok_or("MOV model sequence overflow")?;
        *shared = state;
    }
    Ok(out)
}

pub fn decode(encoded: &[u8]) -> Result<Vec<u8>, String> {
    decode_with_cancel(encoded, &|| false)
}

pub fn decode_with_cancel(encoded: &[u8], cancelled: &dyn Fn() -> bool) -> Result<Vec<u8>, String> {
    decode_with_progress(encoded, &mut |_, _| !cancelled())
}

pub fn decode_with_progress(
    encoded: &[u8],
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Vec<u8>, String> {
    decode_impl(encoded, progress, None)
}

pub fn decode_shared_with_progress(
    encoded: &[u8],
    state: &mut SharedContext,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Vec<u8>, String> {
    decode_impl(encoded, progress, Some(state))
}

fn decode_impl(
    encoded: &[u8],
    progress: &mut dyn FnMut(u64, u64) -> bool,
    shared: Option<&mut SharedContext>,
) -> Result<Vec<u8>, String> {
    report(progress, 0, 1)?;
    let use_shared = shared.is_some();
    let header = HEADER + if use_shared { 8 } else { 0 };
    let expected_magic = if use_shared { SHARED_MAGIC } else { MAGIC };
    if encoded.len() < header || encoded.len() > LIMIT || &encoded[..8] != expected_magic {
        return Err("invalid ProRes model header".into());
    }
    if let Some(state) = shared.as_ref() {
        let sequence = u64::from_le_bytes(encoded[28..36].try_into().unwrap());
        if sequence != state.files {
            return Err("ProRes shared model record order mismatch".into());
        }
    }
    let raw_len = usize::try_from(u64::from_le_bytes(encoded[8..16].try_into().unwrap()))
        .map_err(|_| "MOV model size overflow")?;
    let count = u32::from_le_bytes(encoded[16..20].try_into().unwrap()) as usize;
    let range_len = usize::try_from(u64::from_le_bytes(encoded[20..28].try_into().unwrap()))
        .map_err(|_| "MOV model range size overflow")?;
    if raw_len > LIMIT
        || count == 0
        || count > MAX_SEGMENTS
        || count > (encoded.len() - header) / DESCRIPTOR
    {
        return Err("invalid ProRes model limits".into());
    }
    let range_start = header + count * DESCRIPTOR;
    if range_len < 5 || range_len > encoded.len() - range_start {
        return Err("truncated ProRes model range stream".into());
    }
    let mut coder = RangeDecode::new(&encoded[range_start..range_start + range_len])?;
    let mut descriptors = Vec::with_capacity(count);
    let mut occupied = 0usize;
    let mut coefficients_total = 0usize;
    for i in 0..count {
        let descriptor = &encoded[header + i * DESCRIPTOR..header + (i + 1) * DESCRIPTOR];
        let offset = usize::try_from(u64::from_le_bytes(descriptor[..8].try_into().unwrap()))
            .map_err(|_| "MOV model offset overflow")?;
        let len = u16::from_le_bytes(descriptor[8..10].try_into().unwrap()) as usize;
        let blocks = descriptor[10] as usize;
        let flags = descriptor[11];
        if blocks == 0
            || blocks > 32
            || !blocks.is_power_of_two()
            || len == 0
            || flags > 5
            || (i == 0 && flags & 1 == 0)
            || offset > raw_len
            || len > raw_len - offset
        {
            return Err("invalid ProRes model descriptor".into());
        }
        occupied = occupied
            .checked_add(len)
            .ok_or("MOV model segment overflow")?;
        coefficients_total = coefficients_total
            .checked_add(blocks * 64)
            .ok_or("MOV model coefficient count overflow")?;
        if coefficients_total > MAX_COEFFICIENTS {
            return Err("MOV model coefficient work limit".into());
        }
        descriptors.push((offset, len, blocks, flags));
    }
    if occupied > raw_len || raw_len - occupied != encoded.len() - range_start - range_len {
        return Err("ProRes model payload length mismatch".into());
    }
    let mut ranges: Vec<_> = descriptors.iter().map(|&(o, l, _, _)| (o, l)).collect();
    ranges.sort_unstable();
    let mut end = 0;
    for &(offset, len) in &ranges {
        if offset < end {
            return Err("overlapping ProRes model segments".into());
        }
        end = offset + len;
    }
    let mut out = Vec::new();
    out.try_reserve_exact(raw_len)
        .map_err(|_| "MOV model allocation failed")?;
    out.resize(raw_len, 0);
    let mut state = shared.as_ref().map(|s| (**s).clone()).unwrap_or_default();
    let mut probabilities = std::mem::take(&mut state.probabilities);
    let mut previous: Vec<i16> = Vec::new();
    for (index, (offset, len, blocks, flags)) in descriptors.into_iter().enumerate() {
        report(progress, index, count)?;
        let reset = flags & 1 != 0;
        let channel = (flags >> 1) as usize;
        if reset {
            previous = vec![0; blocks * 64];
        }
        if previous.len() != blocks * 64 {
            return Err("invalid ProRes model prediction group".into());
        }
        let mut coefficients = vec![0i16; blocks * 64];
        let mut residual = vec![0i32; blocks * 64];
        let mut copied = vec![false; blocks];
        let mut temporal = vec![false; blocks];
        for block in 0..blocks {
            let context = channel * 4
                + usize::from(block > 0 && copied[block - 1]) * 2
                + usize::from(previous[block] == 0);
            copied[block] = coder.bit(&mut probabilities[context])?;
            if copied[block] {
                if reset {
                    return Err("invalid initial ProRes model reference".into());
                }
                for frequency in 0..64 {
                    coefficients[frequency * blocks + block] = previous[frequency * blocks + block];
                }
            } else {
                if use_shared && coder.bit(&mut probabilities[90000 + channel])? {
                    let mut reference = 0usize;
                    for bit in (0..19).rev() {
                        if coder.bit(&mut probabilities[90016 + bit])? {
                            reference |= 1 << bit;
                        }
                    }
                    let values = state
                        .dictionary
                        .values
                        .get(reference)
                        .ok_or("invalid ProRes shared block reference")?;
                    for frequency in 0..64 {
                        coefficients[frequency * blocks + block] = values[frequency];
                    }
                    copied[block] = true;
                    continue;
                }
                let context = 32
                    + channel * 4
                    + usize::from(block > 0 && temporal[block - 1]) * 2
                    + usize::from(previous[block] == 0);
                temporal[block] = coder.bit(&mut probabilities[context])?;
            }
        }
        for frequency in 0..64 {
            for block in 0..blocks {
                if copied[block] {
                    continue;
                }
                let i = frequency * blocks + block;
                let left = if block > 0 { residual[i - 1] } else { 0 };
                let above = if frequency > 0 {
                    residual[i - blocks]
                } else {
                    0
                };
                let p = previous[i] as i32;
                let zero = zero_context(channel, frequency, p, left, above, temporal[block]);
                residual[i] = read_value(
                    &mut coder,
                    &mut probabilities,
                    channel * 64 + frequency,
                    zero,
                    left,
                    p,
                )?;
                coefficients[i] = i16::try_from(residual[i] + if temporal[block] { p } else { 0 })
                    .map_err(|_| "ProRes model coefficient overflow")?;
            }
        }
        let plane = pack_plane(&coefficients, blocks, len)?;
        out[offset..offset + len].copy_from_slice(&plane);
        if use_shared {
            state.dictionary.add_plane(&coefficients, blocks);
        }
        previous = coefficients;
    }
    let mut cursor = range_start + range_len;
    end = 0;
    for (offset, len) in ranges {
        let n = offset - end;
        out[end..offset].copy_from_slice(&encoded[cursor..cursor + n]);
        cursor += n;
        end = offset + len;
    }
    out[end..].copy_from_slice(&encoded[cursor..]);
    report(progress, count, count)?;
    if let Some(shared) = shared {
        state.probabilities = probabilities;
        state.files = state
            .files
            .checked_add(1)
            .ok_or("MOV model sequence overflow")?;
        *shared = state;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn movie_fixture() -> Vec<u8> {
        let mut mdat = Vec::new();
        for frame_number in 0..20 {
            let mut coefficients = vec![0i16; 256];
            for (i, value) in coefficients.iter_mut().enumerate() {
                *value = if i < 4 {
                    (frame_number * 97 % 2000) as i16 - 1000
                } else {
                    ((i * 19) % 43) as i16 - 21
                };
            }
            coefficients[255] = -3;
            let mut plane = pack_plane(&coefficients, 4, 4096).unwrap();
            plane.truncate(plane.iter().rposition(|&x| x != 0).unwrap() + 1);
            let slice_len = 8 + 3 * plane.len() + 16;
            let picture_len = 10 + slice_len;
            let frame_len = 28 + picture_len + 7;
            let mut frame = vec![0; 28];
            frame[..4].copy_from_slice(&(frame_len as u32).to_be_bytes());
            frame[4..8].copy_from_slice(b"icpf");
            frame[8..10].copy_from_slice(&20u16.to_be_bytes());
            frame[16..18].copy_from_slice(&16u16.to_be_bytes());
            frame[18..20].copy_from_slice(&16u16.to_be_bytes());
            frame[20] = 0xc0;
            frame.push(0x40);
            frame.extend_from_slice(&(picture_len as u32).to_be_bytes());
            frame.extend_from_slice(&[0, 1, 0]);
            frame.extend_from_slice(&(slice_len as u16).to_be_bytes());
            frame.extend_from_slice(&[0x40, 1]);
            for _ in 0..3 {
                frame.extend_from_slice(&(plane.len() as u16).to_be_bytes());
            }
            for _ in 0..3 {
                frame.extend_from_slice(&plane);
            }
            frame.extend_from_slice(&[0xaa; 16]);
            frame.extend_from_slice(&[0xf1; 7]);
            mdat.extend_from_slice(&frame);
            mdat.extend_from_slice(&[0; 13]);
        }
        let mut raw = Vec::new();
        for (kind, data) in [
            (b"ftyp", b"qt  ".as_slice()),
            (b"moov", b"unknown metadata".as_slice()),
            (b"mdat", mdat.as_slice()),
            (b"free", b"tail padding".as_slice()),
        ] {
            raw.extend_from_slice(&((data.len() + 8) as u32).to_be_bytes());
            raw.extend_from_slice(kind);
            raw.extend_from_slice(data);
        }
        raw
    }

    #[test]
    fn complete_movie_round_trip_and_progress() {
        let raw = movie_fixture();
        let mut progress = Vec::new();
        let encoded = encode_with_progress(&raw, &mut |d, t| {
            progress.push((d, t));
            true
        })
        .unwrap();
        assert_eq!(decode(&encoded).unwrap(), raw);
        assert!(encoded.len() < raw.len());
        let &(done, total) = progress.last().unwrap();
        assert_eq!(done, total);
        assert!(total > 1);
    }

    #[test]
    fn shared_records_reconstruct_in_order_with_cross_file_references() {
        let first = movie_fixture();
        let mut second = first.clone();
        second[20] ^= 1;
        let mut encoder = SharedContext::default();
        let a = encode_shared_with_progress(&first, &mut encoder, &mut |_, _| true).unwrap();
        let b = encode_shared_with_progress(&second, &mut encoder, &mut |_, _| true).unwrap();
        assert_eq!(encoder.files, 2);
        assert!(!encoder.dictionary.values.is_empty());
        let mut decoder = SharedContext::default();
        assert!(decode_shared_with_progress(&b, &mut decoder, &mut |_, _| true).is_err());
        assert_eq!(decoder.files, 0);
        assert_eq!(
            decode_shared_with_progress(&a, &mut decoder, &mut |_, _| true).unwrap(),
            first
        );
        assert_eq!(
            decode_shared_with_progress(&b, &mut decoder, &mut |_, _| true).unwrap(),
            second
        );
        assert_eq!(encoder.probabilities, decoder.probabilities);
        assert_eq!(encoder.dictionary.values, decoder.dictionary.values);
        assert_eq!(encoder.dictionary.hashes, decoder.dictionary.hashes);
        assert_eq!(encoder.dictionary.next, decoder.dictionary.next);
        assert!(b.len() < a.len());
    }

    #[test]
    fn cancelled_shared_encode_does_not_advance_state() {
        let mut state = SharedContext::default();
        let mut calls = 0;
        assert!(
            encode_shared_with_progress(&movie_fixture(), &mut state, &mut |_, _| {
                calls += 1;
                calls < 3
            })
            .is_err()
        );
        assert_eq!(state.files, 0);
        assert!(state.dictionary.values.is_empty());
        assert!(state.probabilities.iter().all(|&p| p == 2048));
    }

    #[test]
    fn random_shared_references_do_not_panic_or_change_state() {
        let mut seed = 89u32;
        for _ in 0..1000 {
            let mut encoded = Vec::from(*SHARED_MAGIC);
            encoded.extend_from_slice(&128u64.to_le_bytes());
            encoded.extend_from_slice(&1u32.to_le_bytes());
            encoded.extend_from_slice(&64u64.to_le_bytes());
            encoded.extend_from_slice(&0u64.to_le_bytes());
            encoded.extend_from_slice(&0u64.to_le_bytes());
            encoded.extend_from_slice(&32u16.to_le_bytes());
            encoded.extend_from_slice(&[1, 1]);
            for _ in 0..160 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                encoded.push(seed as u8);
            }
            let mut state = SharedContext::default();
            if decode_shared_with_progress(&encoded, &mut state, &mut |_, _| true).is_err() {
                assert_eq!(state.files, 0);
                assert!(state.dictionary.values.is_empty());
            }
        }
    }

    #[test]
    fn truncated_or_overlapping_model_is_rejected() {
        let encoded = encode(&movie_fixture()).unwrap();
        for len in [0, 27, 28, 39, encoded.len() / 2, encoded.len() - 1] {
            assert!(decode(&encoded[..len]).is_err());
        }
        let mut overlap = encoded.clone();
        let first_offset = overlap[28..36].to_vec();
        overlap[40..48].copy_from_slice(&first_offset);
        assert!(decode(&overlap).is_err());
        let mut too_many = encoded;
        too_many[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&too_many).is_err());
    }

    #[test]
    fn cancellation_inside_encode_and_decode() {
        let raw = movie_fixture();
        let mut calls = 0;
        assert!(encode_with_progress(&raw, &mut |_, _| {
            calls += 1;
            calls < 2
        })
        .unwrap_err()
        .contains("error.cancelled"));
        let encoded = encode(&raw).unwrap();
        calls = 0;
        assert!(decode_with_progress(&encoded, &mut |_, _| {
            calls += 1;
            calls < 2
        })
        .unwrap_err()
        .contains("error.cancelled"));
    }

    #[test]
    fn cancellation_during_mdat_scan_reaches_model_caller() {
        let mut raw = vec![0; 1024 * 1024];
        let len = raw.len() as u32;
        raw[..4].copy_from_slice(&len.to_be_bytes());
        raw[4..8].copy_from_slice(b"mdat");
        let mut calls = 0;
        let error = encode_with_progress(&raw, &mut |_, _| {
            calls += 1;
            calls != 5
        })
        .unwrap_err();
        assert_eq!(error, "error.cancelled");
        assert_eq!(calls, 5);
    }

    #[test]
    fn short_range_is_rejected_before_descriptors_or_output_allocation() {
        for shared in [false, true] {
            for range_len in 0u64..5 {
                let mut encoded = Vec::from(*if shared { SHARED_MAGIC } else { MAGIC });
                encoded.extend_from_slice(&(LIMIT as u64).to_le_bytes());
                encoded.extend_from_slice(&1u32.to_le_bytes());
                encoded.extend_from_slice(&range_len.to_le_bytes());
                if shared {
                    encoded.extend_from_slice(&0u64.to_le_bytes());
                }
                encoded.extend_from_slice(&[0; DESCRIPTOR]);
                encoded.resize(encoded.len() + range_len as usize, 0);
                let mut state = SharedContext::default();
                let error = if shared {
                    decode_shared_with_progress(&encoded, &mut state, &mut |_, _| true)
                } else {
                    decode(&encoded)
                }
                .unwrap_err();
                assert_eq!(error, "truncated ProRes model range stream");
                assert_eq!(state.files, 0);
                assert!(state.dictionary.values.is_empty());
            }
        }
    }

    #[test]
    fn random_range_payloads_do_not_panic() {
        let mut seed = 137u32;
        for _ in 0..2000 {
            let mut encoded = Vec::from(*MAGIC);
            encoded.extend_from_slice(&128u64.to_le_bytes());
            encoded.extend_from_slice(&1u32.to_le_bytes());
            encoded.extend_from_slice(&64u64.to_le_bytes());
            encoded.extend_from_slice(&0u64.to_le_bytes());
            encoded.extend_from_slice(&32u16.to_le_bytes());
            encoded.extend_from_slice(&[1, 1]);
            for _ in 0..160 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                encoded.push(seed as u8);
            }
            let _ = decode(&encoded);
        }
    }
    #[test]
    fn arithmetic_round_trip_including_extreme_probabilities() {
        let mut coder = RangeEncode::new();
        let mut probabilities = [2048; 37];
        let mut bits = Vec::new();
        let mut seed = 23u32;
        for i in 0..100_000 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let bit = match i / 25000 {
                0 => false,
                1 => true,
                2 => i % 101 == 0,
                _ => seed & 1 != 0,
            };
            bits.push(bit);
            coder.bit(bit, &mut probabilities[i % 37]);
        }
        let encoded = coder.finish();
        let mut decoder = RangeDecode::new(&encoded).unwrap();
        probabilities.fill(2048);
        for (i, bit) in bits.into_iter().enumerate() {
            assert_eq!(decoder.bit(&mut probabilities[i % 37]).unwrap(), bit);
        }
    }
    #[test]
    fn rejects_invalid_input_and_honors_cancel() {
        assert!(encode(b"bad MOV").is_err());
        assert!(decode(b"bad model").is_err());
        assert!(encode_with_cancel(&[], &|| true)
            .unwrap_err()
            .contains("error.cancelled"));
        let mut data = Vec::from(*MAGIC);
        data.extend_from_slice(&u64::MAX.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&5u64.to_le_bytes());
        assert!(decode(&data).is_err());
    }
}
