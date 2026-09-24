const MAGIC: &[u8; 8] = b"SPPR\0\0\0\x01";
const MAX_RAW: usize = 512 * 1024 * 1024;
const MAX_EXPANDED: usize = 512 * 1024 * 1024;
const MAX_SEGMENTS: usize = 1_000_000;
const DC_BOOKS: [u8; 7] = [0x04, 0x28, 0x28, 0x4d, 0x4d, 0x70, 0x70];
const RUN_BOOKS: [u8; 16] = [
    0x06, 0x06, 0x05, 0x05, 0x04, 0x29, 0x29, 0x29, 0x29, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x4c,
];
const LEVEL_BOOKS: [u8; 10] = [0x04, 0x0a, 0x05, 0x06, 0x04, 0x28, 0x28, 0x28, 0x28, 0x4c];

fn be16(b: &[u8], i: usize) -> Result<usize, String> {
    let a = b.get(i..i + 2).ok_or("truncated ProRes header")?;
    Ok(u16::from_be_bytes([a[0], a[1]]) as usize)
}
fn be32(b: &[u8], i: usize) -> Result<usize, String> {
    let a = b.get(i..i + 4).ok_or("truncated MOV header")?;
    Ok(u32::from_be_bytes([a[0], a[1], a[2], a[3]]) as usize)
}
fn allocate(n: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    out.try_reserve_exact(n)
        .map_err(|_| "MOV transform memory limit".to_string())?;
    Ok(out)
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}
impl Bits<'_> {
    fn get(&mut self, n: usize) -> Result<u32, String> {
        if n > 31 || self.pos + n > self.data.len() * 8 {
            return Err("truncated ProRes codeword".into());
        }
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | ((self.data[self.pos / 8] >> (7 - self.pos % 8)) & 1) as u32;
            self.pos += 1;
        }
        Ok(v)
    }
    fn zero_tail(&self) -> bool {
        let remaining = self.data.len() * 8 - self.pos;
        remaining < 32
            && (self.pos..self.data.len() * 8).all(|p| self.data[p / 8] & (1 << (7 - p % 8)) == 0)
    }
    fn word(&mut self, book: u8) -> Result<u32, String> {
        let rice = (book >> 5) as usize;
        let exp = ((book >> 2) & 7) as usize;
        let switch = (book & 3) as usize;
        let mut zeros = 0;
        while self.get(1)? == 0 {
            zeros += 1;
            if zeros > 15 {
                return Err("oversized ProRes codeword".into());
            }
        }
        if zeros <= switch {
            Ok(((zeros as u32) << rice) + self.get(rice)?)
        } else {
            let suffix = exp + zeros - switch - 1;
            if suffix > 30 {
                return Err("oversized ProRes exponent".into());
            }
            Ok((1u32 << suffix) + self.get(suffix)? - (1u32 << exp)
                + (((switch + 1) as u32) << rice))
        }
    }
}
struct Writer {
    data: Vec<u8>,
    pos: usize,
}
impl Writer {
    fn put(&mut self, value: u32, n: usize) -> Result<(), String> {
        if n > 31 || self.pos + n > self.data.len() * 8 {
            return Err("ProRes re-encoding overflow".into());
        }
        for shift in (0..n).rev() {
            self.data[self.pos / 8] |= (((value >> shift) & 1) as u8) << (7 - self.pos % 8);
            self.pos += 1;
        }
        Ok(())
    }
    fn word(&mut self, value: u32, book: u8) -> Result<(), String> {
        let rice = (book >> 5) as usize;
        let exp = ((book >> 2) & 7) as usize;
        let switch = (book & 3) as usize;
        let threshold = ((switch + 1) as u32) << rice;
        if value < threshold {
            self.put(0, (value >> rice) as usize)?;
            self.put(1, 1)?;
            self.put(value & ((1u32 << rice) - 1), rice)
        } else {
            let adjusted = value
                .checked_add(1u32 << exp)
                .and_then(|x| x.checked_sub(threshold))
                .ok_or("invalid ProRes symbol")?;
            let top = (31 - adjusted.leading_zeros()) as usize;
            let zeros = top
                .checked_add(switch + 1)
                .and_then(|x| x.checked_sub(exp))
                .ok_or("invalid ProRes exponent")?;
            self.put(0, zeros)?;
            self.put(adjusted, top + 1)
        }
    }
}

pub(crate) fn unpack_plane(data: &[u8], blocks: usize) -> Result<Vec<i16>, String> {
    if blocks == 0 || blocks > 32 || !blocks.is_power_of_two() || data.is_empty() {
        return Err("unsupported ProRes plane".into());
    }
    let mut bits = Bits { data, pos: 0 };
    let mut coeffs = vec![0i16; blocks * 64];
    let first = bits.word(0xb8)?;
    let mut dc = (first >> 1) as i32 ^ -((first & 1) as i32);
    coeffs[0] = i16::try_from(dc).map_err(|_| "ProRes DC overflow")?;
    let mut code = 5u32;
    let mut negative = false;
    for cell in coeffs.iter_mut().take(blocks).skip(1) {
        code = bits.word(DC_BOOKS[(code as usize).min(6)])?;
        if code == 0 {
            negative = false;
        } else if code & 1 != 0 {
            negative = !negative;
        }
        let step = ((code + 1) >> 1) as i32;
        dc += if negative { -step } else { step };
        *cell = i16::try_from(dc).map_err(|_| "ProRes DC overflow")?;
    }
    let mut position = blocks - 1;
    let mut run = 4u32;
    let mut level = 2u32;
    while !bits.zero_tail() {
        run = bits.word(RUN_BOOKS[(run as usize).min(15)])?;
        position = position
            .checked_add(run as usize + 1)
            .ok_or("ProRes run overflow")?;
        if position >= coeffs.len() {
            return Err("invalid ProRes coefficient run".into());
        }
        level = bits.word(LEVEL_BOOKS[(level as usize).min(9)])? + 1;
        if level > 32767 {
            return Err("ProRes AC overflow".into());
        }
        coeffs[position] = if bits.get(1)? == 0 {
            level as i16
        } else {
            -(level as i16)
        };
    }
    Ok(coeffs)
}

pub(crate) fn pack_plane(coeffs: &[i16], blocks: usize, bytes: usize) -> Result<Vec<u8>, String> {
    if blocks == 0
        || blocks > 32
        || !blocks.is_power_of_two()
        || coeffs.len() != blocks * 64
        || bytes > 65535
    {
        return Err("invalid ProRes coefficients".into());
    }
    let mut writer = Writer {
        data: vec![0; bytes],
        pos: 0,
    };
    let dc = coeffs[0] as i32;
    writer.word(((dc << 1) ^ (dc >> 31)) as u32, 0xb8)?;
    let mut previous_code = 5u32;
    let mut negative = false;
    for i in 1..blocks {
        let delta = coeffs[i] as i32 - coeffs[i - 1] as i32;
        let code = if delta == 0 {
            negative = false;
            0
        } else {
            let changes_sign = (delta < 0) != negative;
            negative = delta < 0;
            delta.unsigned_abs() * 2 - u32::from(changes_sign)
        };
        writer.word(code, DC_BOOKS[(previous_code as usize).min(6)])?;
        previous_code = code;
    }
    let mut last = blocks - 1;
    let mut previous_run = 4usize;
    let mut previous_level = 2usize;
    for (position, &coefficient) in coeffs.iter().enumerate().skip(blocks) {
        if coefficient == 0 {
            continue;
        }
        let run = position - last - 1;
        let level = (coefficient as i32).unsigned_abs() as usize;
        writer.word(run as u32, RUN_BOOKS[previous_run.min(15)])?;
        writer.word(level as u32 - 1, LEVEL_BOOKS[previous_level.min(9)])?;
        writer.put(u32::from(coefficient < 0), 1)?;
        previous_run = run;
        previous_level = level;
        last = position;
    }
    Ok(writer.data)
}

#[derive(Clone, Debug)]
pub(crate) struct Segment {
    pub(crate) offset: usize,
    pub(crate) len: usize,
    pub(crate) blocks: usize,
    pub(crate) key: (usize, usize, u8, usize),
    pub(crate) frame: usize,
}

fn frame_segments(
    raw: &[u8],
    start: usize,
    frame: usize,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<(usize, Vec<Segment>), String> {
    let length = be32(raw, start)?;
    if length < 28
        || start.checked_add(length).is_none_or(|x| x > raw.len())
        || raw.get(start + 4..start + 8) != Some(b"icpf")
    {
        return Err("invalid ProRes frame".into());
    }
    let f = &raw[start..start + length];
    let header = be16(f, 8)?;
    if header < 20 || header + 8 > length {
        return Err("invalid ProRes frame header".into());
    }
    let width = be16(f, 16)?;
    let height = be16(f, 18)?;
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        return Err("unsupported ProRes dimensions".into());
    }
    let frame_type = (f[20] >> 2) & 3;
    if frame_type != 0 {
        return Err("interlaced ProRes is stored verbatim".into());
    }
    let chroma = f[20] >> 6;
    if chroma != 2 && chroma != 3 {
        return Err("unsupported ProRes chroma".into());
    }
    let picture = header + 8;
    let ph = *f.get(picture).ok_or("truncated ProRes picture")? as usize >> 3;
    if ph < 8 {
        return Err("invalid ProRes picture header".into());
    }
    let picture_len = be32(f, picture + 1)?;
    let mbw = width.div_ceil(16);
    let mbh = height.div_ceil(16);
    let slice_flag = *f.get(picture + 7).ok_or("truncated ProRes slices")?;
    if slice_flag & 15 != 0 || slice_flag >> 4 > 3 {
        return Err("unsupported ProRes slice layout".into());
    }
    let slice_width = 1usize << (slice_flag >> 4);
    let count = mbh * (mbw / slice_width + (mbw % slice_width).count_ones() as usize);
    let end = picture
        .checked_add(picture_len)
        .ok_or("ProRes size overflow")?;
    if count == 0 || end > f.len() || picture_len < ph + 2 * count {
        return Err("invalid ProRes picture size".into());
    }
    let mut pos = picture + ph + 2 * count;
    let mut x = 0;
    let mut segments = Vec::with_capacity(count.min(128) * 3);
    for slice in 0..count {
        if slice % 128 == 0 && cancelled() {
            return Err("error.cancelled".into());
        }
        let len = be16(f, picture + ph + slice * 2)?;
        if len < 6 || pos + len > end {
            return Err("invalid ProRes slice length".into());
        }
        let sh = f[pos] as usize >> 3;
        if sh < 6 || sh > len {
            return Err("invalid ProRes slice header".into());
        }
        let y = be16(f, pos + 2)?;
        let u = be16(f, pos + 4)?;
        let v = if sh >= 8 {
            be16(f, pos + 6)?
        } else {
            len.checked_sub(sh + y + u).ok_or("invalid ProRes planes")?
        };
        if sh + y + u + v > len {
            return Err("invalid ProRes plane size".into());
        }
        let mut mbs = slice_width;
        while mbs > mbw - x {
            mbs >>= 1;
        }
        let mut at = pos + sh;
        for (plane, size) in [y, u, v].into_iter().enumerate() {
            let blocks = mbs * if plane == 0 || chroma == 3 { 4 } else { 2 };
            if size != 0 {
                segments.push(Segment {
                    offset: start + at,
                    len: size,
                    blocks,
                    key: (width, height, plane as u8, slice),
                    frame,
                });
            }
            at += size;
        }
        x += mbs;
        if x == mbw {
            x = 0;
        }
        pos += len;
    }
    if pos != end {
        return Err("unrecognized ProRes picture suffix".into());
    }
    Ok((length, segments))
}

pub(crate) fn find_segments(raw: &[u8]) -> Result<Vec<Segment>, String> {
    find_segments_with_cancel(raw, &mut || false)
}

pub(crate) fn find_segments_with_cancel(
    raw: &[u8],
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<Vec<Segment>, String> {
    if cancelled() {
        return Err("error.cancelled".into());
    }
    let mut segments = Vec::new();
    let mut atom = 0;
    let mut frames = 0;
    while atom < raw.len() {
        if cancelled() {
            return Err("error.cancelled".into());
        }
        let mut size = be32(raw, atom)?;
        let kind = raw.get(atom + 4..atom + 8).ok_or("truncated MOV atom")?;
        let mut header = 8;
        if size == 1 {
            let n: [u8; 8] = raw
                .get(atom + 8..atom + 16)
                .ok_or("truncated MOV extended atom")?
                .try_into()
                .unwrap();
            size = usize::try_from(u64::from_be_bytes(n)).map_err(|_| "oversized MOV atom")?;
            header = 16;
        } else if size == 0 {
            size = raw.len() - atom;
        }
        if size < header || size > raw.len() - atom {
            return Err("invalid MOV atom size".into());
        }
        let end = atom + size;
        if kind == b"mdat" {
            let mut p = atom + header;
            let mut next_cancel_check = p;
            while p + 8 <= end {
                if p >= next_cancel_check {
                    if cancelled() {
                        return Err("error.cancelled".into());
                    }
                    next_cancel_check = p.saturating_add(16 * 1024);
                }
                if &raw[p + 4..p + 8] == b"icpf" {
                    let mut cancelled_in_frame = false;
                    let parsed = frame_segments(raw, p, frames, &mut || {
                        cancelled_in_frame |= cancelled();
                        cancelled_in_frame
                    });
                    if cancelled_in_frame {
                        return Err("error.cancelled".into());
                    }
                    if let Ok((length, mut found)) = parsed {
                        if length <= end - p {
                            if segments.len() + found.len() > MAX_SEGMENTS {
                                return Err("MOV slice limit".into());
                            }
                            segments.append(&mut found);
                            frames += 1;
                            p += length;
                            continue;
                        }
                    }
                }
                p += 1;
            }
        }
        atom = end;
    }
    if frames == 0 || segments.is_empty() {
        return Err("MOV has no supported ProRes frames".into());
    }
    Ok(segments)
}

pub fn encode(raw: &[u8]) -> Result<Vec<u8>, String> {
    if raw.len() > MAX_RAW {
        return Err("MOV exceeds transform memory limit".into());
    }
    let mut segments = find_segments(raw)?;
    segments.sort_unstable_by_key(|s| (s.key, s.frame));
    let mut chosen = Vec::new();
    let mut payload = Vec::new();
    let mut flags = Vec::new();
    let mut projected = raw.len() + 20;
    let mut begin = 0;
    while begin < segments.len() {
        let mut end = begin + 1;
        while end < segments.len() && segments[end].key == segments[begin].key {
            end += 1;
        }
        let mut verified = Vec::new();
        let mut verbatim = Vec::new();
        let mut direct = Vec::new();
        let mut temporal = Vec::new();
        let mut previous = vec![0i16; segments[begin].blocks * 64];
        let group_size: usize = segments[begin..end].iter().map(|s| s.blocks * 128).sum();
        if group_size > 32 * 1024 * 1024 {
            begin = end;
            continue;
        }
        for s in &segments[begin..end] {
            let bytes = &raw[s.offset..s.offset + s.len];
            let Ok(coefficients) = unpack_plane(bytes, s.blocks) else {
                continue;
            };
            if !pack_plane(&coefficients, s.blocks, s.len).is_ok_and(|b| b == bytes) {
                continue;
            }
            if previous.len() != coefficients.len() {
                continue;
            }
            let base = direct.len();
            let n = coefficients.len();
            direct.resize(base + n * 2, 0);
            temporal.resize(base + n * 2, 0);
            for (i, (&c, &p)) in coefficients.iter().zip(&previous).enumerate() {
                let delta = (c as u16).wrapping_sub(p as u16) as i16;
                for (value, dest) in [(c, &mut direct), (delta, &mut temporal)] {
                    let signed = value as i32;
                    let zz = ((signed << 1) ^ (signed >> 15)) as u16;
                    dest[base + i] = zz as u8;
                    dest[base + n + i] = (zz >> 8) as u8;
                }
            }
            previous = coefficients;
            verbatim.extend_from_slice(bytes);
            verified.push(s.clone());
        }
        begin = end;
        if verified.is_empty() {
            continue;
        }
        let score = |b: &[u8]| {
            zstd::bulk::compress(b, 6)
                .map(|c| c.len())
                .map_err(|e| e.to_string())
        };
        let raw_score = score(&verbatim)?;
        let direct_score = score(&direct)?;
        let temporal_score = score(&temporal)?;
        let use_temporal = temporal_score < direct_score;
        let best_score = direct_score.min(temporal_score);
        if best_score + 32 + verified.len() * 3 >= raw_score {
            continue;
        }
        let next_size = projected
            .checked_add(direct.len() + verified.len() * 16)
            .and_then(|x| x.checked_sub(verbatim.len()))
            .ok_or("MOV transform size overflow")?;
        if next_size > MAX_EXPANDED {
            continue;
        }
        projected = next_size;
        for (i, s) in verified.into_iter().enumerate() {
            chosen.push(s);
            flags.push(u8::from(i == 0) | if use_temporal { 0 } else { 2 });
        }
        payload.extend_from_slice(if use_temporal { &temporal } else { &direct });
    }
    if chosen.is_empty() {
        return Err("ProRes coefficient transform has no measured gain".into());
    }
    let mut segments = chosen;
    let mut out = allocate(projected)?;
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(raw.len() as u64).to_le_bytes());
    out.extend_from_slice(&(segments.len() as u32).to_le_bytes());
    for (s, flag) in segments.iter().zip(flags) {
        out.extend_from_slice(&(s.offset as u64).to_le_bytes());
        out.extend_from_slice(&(s.len as u32).to_le_bytes());
        out.push(s.blocks as u8);
        out.push(flag);
        out.extend_from_slice(&[0, 0]);
    }
    out.extend_from_slice(&payload);
    drop(payload);
    segments.sort_unstable_by_key(|s| s.offset);
    let mut p = 0;
    for s in &segments {
        out.extend_from_slice(&raw[p..s.offset]);
        p = s.offset + s.len;
    }
    out.extend_from_slice(&raw[p..]);
    Ok(out)
}

pub fn decode(encoded: &[u8]) -> Result<Vec<u8>, String> {
    if encoded.len() < 20 || encoded.len() > MAX_EXPANDED || &encoded[..8] != MAGIC {
        return Err("invalid ProRes transform header".into());
    }
    let raw_len = usize::try_from(u64::from_le_bytes(encoded[8..16].try_into().unwrap()))
        .map_err(|_| "MOV size overflow")?;
    let count = u32::from_le_bytes(encoded[16..20].try_into().unwrap()) as usize;
    if raw_len > MAX_RAW || count == 0 || count > MAX_SEGMENTS || count > (encoded.len() - 20) / 16
    {
        return Err("invalid ProRes transform limits".into());
    }
    let mut descriptors = Vec::with_capacity(count);
    let mut cursor = 20 + count * 16;
    let mut occupied = 0usize;
    for i in 0..count {
        let h = &encoded[20 + i * 16..36 + i * 16];
        let offset = usize::try_from(u64::from_le_bytes(h[..8].try_into().unwrap()))
            .map_err(|_| "MOV offset overflow")?;
        let len = u32::from_le_bytes(h[8..12].try_into().unwrap()) as usize;
        let blocks = h[12] as usize;
        let reset = h[13];
        if blocks == 0
            || blocks > 32
            || !blocks.is_power_of_two()
            || len == 0
            || len > 65535
            || reset > 3
            || h[14..] != [0, 0]
            || (i == 0 && reset & 1 == 0)
            || offset > raw_len
            || len > raw_len - offset
        {
            return Err("invalid ProRes segment descriptor".into());
        }
        cursor = cursor
            .checked_add(blocks * 128)
            .ok_or("ProRes transform size overflow")?;
        occupied = occupied
            .checked_add(len)
            .ok_or("MOV segment size overflow")?;
        descriptors.push((offset, len, blocks, reset));
    }
    if occupied > raw_len || cursor.checked_add(raw_len - occupied) != Some(encoded.len()) {
        return Err("truncated or trailing ProRes transform data".into());
    }
    let mut ranges: Vec<(usize, usize)> = descriptors.iter().map(|&(o, l, _, _)| (o, l)).collect();
    ranges.sort_unstable();
    let mut end = 0;
    for &(offset, len) in &ranges {
        if offset < end {
            return Err("overlapping ProRes segments".into());
        }
        end = offset + len;
    }
    let mut out = allocate(raw_len)?;
    out.resize(raw_len, 0);
    cursor = 20 + count * 16;
    let mut previous: Vec<i16> = Vec::new();
    for (offset, len, blocks, reset) in descriptors {
        let n = blocks * 64;
        if reset & 1 != 0 {
            previous = vec![0; n];
        }
        if previous.len() != n {
            return Err("incompatible ProRes prediction group".into());
        }
        let mut coefficients = vec![0i16; n];
        for i in 0..n {
            let zz = u16::from_le_bytes([encoded[cursor + i], encoded[cursor + n + i]]);
            let value = ((zz >> 1) as i16) ^ -((zz & 1) as i16);
            coefficients[i] = if reset & 2 != 0 {
                value
            } else {
                (value as u16).wrapping_add(previous[i] as u16) as i16
            };
        }
        cursor += n * 2;
        let packed = pack_plane(&coefficients, blocks, len)?;
        out[offset..offset + len].copy_from_slice(&packed);
        previous = coefficients;
    }
    end = 0;
    for (offset, len) in ranges {
        let n = offset - end;
        out[end..offset].copy_from_slice(&encoded[cursor..cursor + n]);
        cursor += n;
        end = offset + len;
    }
    out[end..].copy_from_slice(&encoded[cursor..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_during_unrecognized_mdat_scan() {
        let mut raw = vec![0; 1024 * 1024];
        let len = raw.len() as u32;
        raw[..4].copy_from_slice(&len.to_be_bytes());
        raw[4..8].copy_from_slice(b"mdat");
        let mut calls = 0;
        let result = find_segments_with_cancel(&raw, &mut || {
            calls += 1;
            calls == 4
        });
        assert_eq!(result.err().as_deref(), Some("error.cancelled"));
        assert_eq!(calls, 4);
    }

    #[test]
    fn one_shot_cancellation_inside_slice_table_is_not_swallowed() {
        let count = 256usize;
        let picture_len = 8 + count * 12;
        let frame_len = 28 + picture_len;
        let mut frame = vec![0; 28];
        frame[..4].copy_from_slice(&(frame_len as u32).to_be_bytes());
        frame[4..8].copy_from_slice(b"icpf");
        frame[8..10].copy_from_slice(&20u16.to_be_bytes());
        frame[16..18].copy_from_slice(&128u16.to_be_bytes());
        frame[18..20].copy_from_slice(&4096u16.to_be_bytes());
        frame[20] = 0xc0;
        frame.push(0x40);
        frame.extend_from_slice(&(picture_len as u32).to_be_bytes());
        frame.extend_from_slice(&(count as u16).to_be_bytes());
        frame.push(0x30);
        for _ in 0..count {
            frame.extend_from_slice(&10u16.to_be_bytes());
        }
        for _ in 0..count {
            frame.extend_from_slice(&[0x40, 1, 0, 2, 0, 0, 0, 0, 0, 0]);
        }
        let mut raw = Vec::new();
        raw.extend_from_slice(&((frame.len() + 8) as u32).to_be_bytes());
        raw.extend_from_slice(b"mdat");
        raw.extend_from_slice(&frame);
        assert_eq!(find_segments(&raw).unwrap().len(), count);
        for cancel_at in [4, 5] {
            let mut calls = 0;
            let result = find_segments_with_cancel(&raw, &mut || {
                calls += 1;
                calls == cancel_at
            });
            assert_eq!(result.err().as_deref(), Some("error.cancelled"));
            assert_eq!(calls, cancel_at);
        }
    }

    #[test]
    fn entropy_coefficients_round_trip() {
        for blocks in [1, 2, 4, 8, 16, 32] {
            let mut c = vec![0i16; blocks * 64];
            for (i, cell) in c.iter_mut().enumerate() {
                if i < blocks || i % 5 == 1 {
                    *cell = ((i * 19) % 43) as i16 - 21;
                }
            }
            let b = pack_plane(&c, blocks, 6000).unwrap();
            let used = b.iter().rposition(|&x| x != 0).unwrap() + 1;
            let restored = unpack_plane(&b[..used], blocks).unwrap();
            assert_eq!(c, restored);
            assert_eq!(&b[..used], &pack_plane(&restored, blocks, used).unwrap());
        }
    }
    #[test]
    fn rejects_invalid_input() {
        assert!(encode(b"not a movie").is_err());
        assert!(decode(b"not a transform").is_err());
        let mut b = Vec::from(*MAGIC);
        b.extend_from_slice(&u64::MAX.to_le_bytes());
        b.extend_from_slice(&1u32.to_le_bytes());
        assert!(decode(&b).is_err());
    }
    #[test]
    fn codewords_round_trip() {
        for book in [0xb8, 0x04, 0x28, 0x4d, 0x70, 0x06, 0x05, 0x29, 0x4c, 0x0a] {
            for value in 0..10000u32 {
                let mut writer = Writer {
                    data: vec![0; 8],
                    pos: 0,
                };
                writer.word(value, book).unwrap();
                let mut bits = Bits {
                    data: &writer.data,
                    pos: 0,
                };
                assert_eq!(value, bits.word(book).unwrap());
                assert_eq!(writer.pos, bits.pos);
            }
        }
    }
    #[test]
    fn scattered_planes_preserve_container_and_reject_overlap() {
        let c = vec![0i16; 4 * 64];
        let plane = pack_plane(&c, 4, 32).unwrap();
        let mut expected = vec![0x77; 130];
        expected[5..37].copy_from_slice(&plane);
        expected[80..112].copy_from_slice(&plane);
        let mut transformed = Vec::from(*MAGIC);
        transformed.extend_from_slice(&130u64.to_le_bytes());
        transformed.extend_from_slice(&2u32.to_le_bytes());
        for offset in [5u64, 80] {
            transformed.extend_from_slice(&offset.to_le_bytes());
            transformed.extend_from_slice(&32u32.to_le_bytes());
            transformed.extend_from_slice(&[4, 3, 0, 0]);
        }
        transformed.resize(transformed.len() + 1024, 0);
        transformed.extend_from_slice(&[0x77; 66]);
        assert_eq!(decode(&transformed).unwrap(), expected);
        assert!(decode(&transformed[..transformed.len() - 1]).is_err());
        transformed[36..44].copy_from_slice(&10u64.to_le_bytes());
        assert!(decode(&transformed).is_err());
    }

    #[test]
    fn malformed_bytes_do_not_panic() {
        let mut state = 19u32;
        for len in 0..512 {
            let mut bytes = vec![0; len];
            for b in &mut bytes {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                *b = state as u8;
            }
            assert!(encode(&bytes).is_err());
            assert!(decode(&bytes).is_err());
            let _ = unpack_plane(&bytes, 32);
        }
    }

    #[test]
    fn movie_round_trip_preserves_unknown_atoms_alpha_and_padding() {
        let mut mdat = Vec::new();
        for frame_number in 0..64 {
            let mut coefficients = vec![0i16; 256];
            for (i, c) in coefficients.iter_mut().enumerate() {
                *c = if i < 4 {
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
            frame.extend_from_slice(&[0x40]);
            frame.extend_from_slice(&(picture_len as u32).to_be_bytes());
            frame.extend_from_slice(&[0, 1, 0]);
            frame.extend_from_slice(&(slice_len as u16).to_be_bytes());
            frame.extend_from_slice(&[0x40, 4]);
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
        let encoded = encode(&raw).expect("synthetic correlated ProRes must select a transform");
        assert_eq!(decode(&encoded).unwrap(), raw);
        for end in [0, 19, 20, 63, encoded.len() / 2, encoded.len() - 1] {
            assert!(decode(&encoded[..end]).is_err());
        }
    }
}
