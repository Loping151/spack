use std::io::Read;

const MAGIC: &[u8; 8] = b"SPGXM\x01\0\0";
const HEADER: usize = 184;
const LIMIT: usize = 128 << 20;
const SIDE_LIMIT: usize = 16 << 20;
const PIXEL_LIMIT: usize = 64 << 20;
const MAX_FRAMES: usize = 1024;
const TOTAL: u64 = 1_000_000;
const T: [[i64; 8]; 8] = [
    [5793, 8035, 7568, 6811, 5793, 4551, 3135, 1598],
    [5793, 6811, 3135, -1598, -5793, -8035, -7568, -4551],
    [5793, 4551, -3135, -8035, -5793, 1598, 7568, 6811],
    [5793, 1598, -7568, -4551, 5793, 6811, -3135, -8035],
    [5793, -1598, -7568, 4551, 5793, -6811, -3135, 8035],
    [5793, -4551, -3135, 8035, -5793, -1598, 7568, -6811],
    [5793, -6811, 3135, 1598, -5793, 8035, -7568, 4551],
    [5793, -8035, 7568, -6811, 5793, -4551, 3135, -1598],
];
const SCAN: [usize; 64] = [
    0, 1, 8, 9, 2, 3, 10, 11, 16, 17, 24, 25, 18, 19, 26, 27, 4, 5, 12, 20, 13, 6, 7, 14, 21, 28,
    29, 22, 15, 23, 30, 31, 32, 33, 40, 48, 41, 34, 35, 42, 49, 56, 57, 50, 43, 36, 37, 44, 51, 58,
    59, 52, 45, 38, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

fn tick(cb: &mut dyn FnMut(u64, u64) -> bool, done: u64) -> Result<(), String> {
    if cb(done.min(TOTAL), TOTAL) {
        Ok(())
    } else {
        Err("error.cancelled".into())
    }
}

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .p
            .checked_add(n)
            .ok_or("GIF prediction length overflow")?;
        let b = self
            .b
            .get(self.p..end)
            .ok_or("Truncated GIF prediction data")?;
        self.p = end;
        Ok(b)
    }
    fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn var(&mut self) -> Result<usize, String> {
        let mut n = 0usize;
        for s in (0..usize::BITS).step_by(7) {
            let b = self.byte()?;
            let part = (b & 127) as usize;
            if part > usize::MAX >> s {
                return Err("GIF prediction integer overflow".into());
            }
            n |= part << s;
            if b < 128 {
                return Ok(n);
            }
        }
        Err("GIF prediction integer overflow".into())
    }
    fn blob(&mut self) -> Result<&'a [u8], String> {
        let n = self.var()?;
        self.take(n)
    }
}
fn be16(b: &[u8], p: usize) -> Result<usize, String> {
    Ok(u16::from_be_bytes(
        b.get(p..p + 2)
            .ok_or("Truncated ProRes header")?
            .try_into()
            .unwrap(),
    ) as usize)
}
fn be32(b: &[u8], p: usize) -> Result<usize, String> {
    Ok(u32::from_be_bytes(
        b.get(p..p + 4)
            .ok_or("Truncated MOV header")?
            .try_into()
            .unwrap(),
    ) as usize)
}
fn le64(b: &[u8], p: usize) -> Result<usize, String> {
    usize::try_from(u64::from_le_bytes(
        b.get(p..p + 8)
            .ok_or("Truncated GIF prediction header")?
            .try_into()
            .unwrap(),
    ))
    .map_err(|_| "GIF prediction size overflow".into())
}

#[derive(Clone)]
struct GifFrame {
    w: usize,
    h: usize,
    x: usize,
    y: usize,
    interlace: bool,
    palette: Vec<[u8; 3]>,
    transparent: Option<u8>,
    pixels: Vec<u8>,
}
fn skip_blocks(r: &mut Reader<'_>) -> Result<(), String> {
    loop {
        let n = r.byte()? as usize;
        if n == 0 {
            return Ok(());
        }
        r.take(n)?;
    }
}
fn gif_metadata(skeleton: &[u8]) -> Result<Vec<GifFrame>, String> {
    let mut r = Reader { b: skeleton, p: 0 };
    let head = r.take(13)?;
    if (&head[..6] != b"GIF87a" && &head[..6] != b"GIF89a")
        || u16::from_le_bytes([head[6], head[7]]) != 300
        || u16::from_le_bytes([head[8], head[9]]) != 300
    {
        return Err("MOV prediction requires a 300 × 300 GIF canvas".into());
    }
    let palette = |bytes: &[u8]| {
        bytes
            .chunks_exact(3)
            .map(|p| [p[0], p[1], p[2]])
            .collect::<Vec<_>>()
    };
    let global = if head[10] & 128 != 0 {
        palette(r.take(3usize << ((head[10] & 7) + 1))?)
    } else {
        vec![]
    };
    let mut transparent = None;
    let mut frames = vec![];
    loop {
        match r.byte()? {
            0x3b => break,
            0x21 => {
                let label = r.byte()?;
                if label == 0xf9 {
                    let b = r.blob()?;
                    if b.len() != 4 || r.byte()? != 0 {
                        return Err("Invalid GIF control extension".into());
                    }
                    transparent = if b[0] & 1 != 0 { Some(b[3]) } else { None }
                } else {
                    skip_blocks(&mut r)?
                }
            }
            0x2c => {
                let d = r.take(9)?;
                let word = |i| u16::from_le_bytes([d[i], d[i + 1]]) as usize;
                let (x, y, w, h) = (word(0), word(2), word(4), word(6));
                if w == 0 || h == 0 || x + w > 300 || y + h > 300 || frames.len() >= MAX_FRAMES {
                    return Err("Unsupported GIF prediction frame dimensions".into());
                }
                let pal = if d[8] & 128 != 0 {
                    palette(r.take(3usize << ((d[8] & 7) + 1))?)
                } else {
                    global.clone()
                };
                if pal.is_empty() || transparent.is_some_and(|t| t as usize >= pal.len()) {
                    return Err("Invalid GIF prediction palette".into());
                }
                r.byte()?;
                skip_blocks(&mut r)?;
                frames.push(GifFrame {
                    w,
                    h,
                    x,
                    y,
                    interlace: d[8] & 64 != 0,
                    palette: pal,
                    transparent,
                    pixels: vec![],
                });
                transparent = None;
            }
            _ => return Err("Unsupported GIF prediction block".into()),
        }
    }
    if frames.is_empty() {
        return Err("No GIF prediction frames".into());
    }
    Ok(frames)
}
fn reorder(bytes: &[u8], f: &GifFrame, to_raster: bool) -> Vec<u8> {
    if !f.interlace {
        return bytes.to_vec();
    }
    let mut out = vec![0; bytes.len()];
    let mut at = 0;
    for (start, step) in [(0, 8), (4, 8), (2, 4), (1, 2)] {
        for y in (start..f.h).step_by(step) {
            if to_raster {
                out[y * f.w..(y + 1) * f.w].copy_from_slice(&bytes[at..at + f.w])
            } else {
                out[at..at + f.w].copy_from_slice(&bytes[y * f.w..(y + 1) * f.w])
            }
            at += f.w
        }
    }
    out
}
fn split(input: &[u8], contains_pixels: bool) -> Result<(Vec<u8>, Vec<GifFrame>), String> {
    if input.len() > LIMIT {
        return Err("GIF prediction input exceeds limit".into());
    }
    let mut r = Reader { b: input, p: 0 };
    if r.take(8)? != b"SGIF\x01\0\0\0" {
        return Err("Invalid SGIF prediction input".into());
    }
    if r.var()? > LIMIT {
        return Err("GIF prediction original exceeds limit".into());
    }
    let mut side = input[..r.p].to_vec();
    let mut skeleton = vec![];
    let mut pixels = vec![];
    let mut sum = 0usize;
    let mut records = 0;
    loop {
        let start = r.p;
        let tag = r.byte()?;
        records += 1;
        if records > MAX_FRAMES * 3 + 1 {
            return Err("Too many GIF prediction records".into());
        }
        match tag {
            0 => {
                skeleton.extend_from_slice(r.blob()?);
                side.extend_from_slice(&input[start..r.p]);
            }
            1 => {
                r.byte()?;
                r.blob()?;
                let n = r.var()?;
                if n == 0 || n > 90_000 || sum > PIXEL_LIMIT - n || pixels.len() >= MAX_FRAMES {
                    return Err("GIF prediction pixels exceed limit".into());
                }
                sum += n;
                side.extend_from_slice(&input[start..r.p]);
                let p = if contains_pixels {
                    r.take(n)?.to_vec()
                } else {
                    vec![0; n]
                };
                pixels.push(p);
                let rest = r.p;
                r.var()?;
                r.blob()?;
                r.blob()?;
                side.extend_from_slice(&input[rest..r.p]);
                skeleton.push(0);
            }
            255 => {
                side.push(255);
                break;
            }
            _ => return Err("Invalid SGIF prediction record".into()),
        }
        if side.len() > SIDE_LIMIT || skeleton.len() > SIDE_LIMIT {
            return Err("GIF prediction metadata exceeds limit".into());
        }
    }
    if r.p != input.len() {
        return Err("Trailing SGIF prediction bytes".into());
    }
    let mut frames = gif_metadata(&skeleton)?;
    if frames.len() != pixels.len() {
        return Err("GIF prediction frame count mismatch".into());
    }
    for (f, p) in frames.iter_mut().zip(pixels) {
        if p.len() != f.w * f.h {
            return Err("GIF prediction pixel dimensions mismatch".into());
        }
        f.pixels = reorder(&p, f, true);
        if contains_pixels && f.pixels.iter().any(|&x| x as usize >= f.palette.len()) {
            return Err("GIF index exceeds palette".into());
        }
    }
    Ok((side, frames))
}
fn join(side: &[u8], frames: &[GifFrame]) -> Result<Vec<u8>, String> {
    let mut r = Reader { b: side, p: 8 };
    r.var()?;
    let mut out = side[..r.p].to_vec();
    let mut frame = 0;
    loop {
        let start = r.p;
        match r.byte()? {
            0 => {
                r.blob()?;
                out.extend_from_slice(&side[start..r.p]);
            }
            1 => {
                r.byte()?;
                r.blob()?;
                let n = r.var()?;
                out.extend_from_slice(&side[start..r.p]);
                let f = frames.get(frame).ok_or("Missing GIF prediction frame")?;
                if n != f.pixels.len() {
                    return Err("GIF prediction frame length mismatch".into());
                }
                out.extend_from_slice(&reorder(&f.pixels, f, false));
                frame += 1;
                let rest = r.p;
                r.var()?;
                r.blob()?;
                r.blob()?;
                out.extend_from_slice(&side[rest..r.p]);
            }
            255 => {
                out.push(255);
                break;
            }
            _ => return Err("Invalid GIF prediction side record".into()),
        }
        if out.len() > LIMIT {
            return Err("GIF prediction reconstruction exceeds limit".into());
        }
    }
    if r.p != side.len() || frame != frames.len() {
        return Err("Incomplete GIF prediction reconstruction".into());
    }
    Ok(out)
}

struct Slice {
    planes: [(usize, usize); 3],
    x: usize,
    y: usize,
    blocks: usize,
}
struct MovFrame {
    weights: [[i64; 64]; 3],
    slices: Vec<Slice>,
}
fn mov_frame(raw: &[u8], start: usize) -> Result<(usize, MovFrame), String> {
    let len = be32(raw, start)?;
    let end = start.checked_add(len).ok_or("ProRes frame overflow")?;
    let f = raw.get(start..end).ok_or("Truncated ProRes frame")?;
    if len < 28
        || f.get(4..8) != Some(b"icpf")
        || be16(f, 16)? != 1000
        || be16(f, 18)? != 1000
        || f[20] >> 6 != 3
        || (f[20] >> 2) & 3 != 0
    {
        return Err("Unsupported MOV prediction frame".into());
    }
    let picture = be16(f, 8)?.checked_add(8).ok_or("ProRes header overflow")?;
    if picture < 28 || picture >= f.len() {
        return Err("Invalid ProRes prediction header".into());
    }
    let mut weights = [[4i64; 64]; 3];
    let mut q = 28;
    if f[27] & 2 != 0 {
        let bytes = f
            .get(q..q + 64)
            .ok_or("Truncated ProRes quantization matrix")?;
        for (i, &b) in bytes.iter().enumerate() {
            weights[0][i] = b as i64;
        }
        q += 64
    }
    weights[1] = weights[0];
    if f[27] & 1 != 0 {
        let bytes = f
            .get(q..q + 64)
            .ok_or("Truncated ProRes quantization matrix")?;
        for (i, &b) in bytes.iter().enumerate() {
            weights[1][i] = b as i64;
        }
        q += 64
    }
    weights[2] = weights[1];
    if q > picture || weights.iter().flatten().any(|&v| v == 0) {
        return Err("Invalid ProRes quantization matrix".into());
    }
    let ph = *f.get(picture).ok_or("Truncated ProRes picture")? as usize >> 3;
    let size = be32(f, picture + 1)?;
    let count = be16(f, picture + 5)?;
    let flags = *f.get(picture + 7).ok_or("Truncated ProRes picture")?;
    if ph < 8 || flags != 0x30 || count != 630 || picture.checked_add(size).is_none_or(|x| x > len)
    {
        return Err("Unsupported ProRes prediction slice layout".into());
    }
    let stop = picture + size;
    let mut at = picture + ph + count * 2;
    if at > stop {
        return Err("Invalid ProRes prediction slice table".into());
    }
    let mut slices = Vec::with_capacity(count);
    let (mut x, mut y) = (0, 0);
    for i in 0..count {
        let n = be16(f, picture + ph + i * 2)?;
        let sh = *f.get(at).ok_or("Truncated ProRes slice")? as usize >> 3;
        if sh < 8 || n < sh || at + n > stop || f.get(at + 1) != Some(&1) {
            return Err("Unsupported ProRes prediction slice".into());
        }
        let lengths = [be16(f, at + 2)?, be16(f, at + 4)?, be16(f, at + 6)?];
        if lengths.contains(&0) || sh + lengths.iter().sum::<usize>() > n {
            return Err("Invalid ProRes prediction plane sizes".into());
        }
        let mut mbs = 8;
        while mbs > 63 - x {
            mbs >>= 1
        }
        let mut offset = start + at + sh;
        let mut planes = [(0, 0); 3];
        for j in 0..3 {
            planes[j] = (offset, lengths[j]);
            offset += lengths[j]
        }
        slices.push(Slice {
            planes,
            x: x * 2,
            y: y * 2,
            blocks: mbs * 4,
        });
        x += mbs;
        if x == 63 {
            x = 0;
            y += 1
        }
        at += n;
    }
    if at != stop || x != 0 || y != 63 {
        return Err("Incomplete ProRes prediction picture".into());
    }
    Ok((len, MovFrame { weights, slices }))
}
fn mov_frames(raw: &[u8], cb: &mut dyn FnMut(u64, u64) -> bool) -> Result<Vec<MovFrame>, String> {
    if raw.len() > 512 << 20 {
        return Err("MOV prediction input exceeds limit".into());
    }
    let mut at = 0;
    let mut frames = vec![];
    while at < raw.len() {
        tick(cb, 1000)?;
        let mut len = be32(raw, at)?;
        let kind = raw.get(at + 4..at + 8).ok_or("Truncated MOV atom")?;
        let mut header = 8;
        if len == 1 {
            len = usize::try_from(u64::from_be_bytes(
                raw.get(at + 8..at + 16)
                    .ok_or("Truncated MOV atom")?
                    .try_into()
                    .unwrap(),
            ))
            .map_err(|_| "MOV atom overflow")?;
            header = 16
        }
        if len == 0 {
            len = raw.len() - at
        }
        if len < header || len > raw.len() - at {
            return Err("Invalid MOV atom length".into());
        }
        let end = at + len;
        if kind == b"mdat" {
            let mut p = at + header;
            while p + 8 <= end {
                let mut scan = p + 4;
                let mut found = None;
                while scan + 4 <= end {
                    tick(cb, 1000)?;
                    let chunk_end = (scan + (64 << 10)).min(end);
                    if let Some(offset) = raw[scan..chunk_end].windows(4).position(|b| b == b"icpf")
                    {
                        found = Some(scan + offset - 4);
                        break;
                    }
                    if chunk_end == end {
                        break;
                    }
                    scan = chunk_end - 3;
                }
                let Some(start) = found else { break };
                let (n, frame) = mov_frame(raw, start)?;
                if n > end - start {
                    return Err("ProRes frame outside MOV atom".into());
                }
                frames.push(frame);
                if frames.len() > MAX_FRAMES {
                    return Err("Too many MOV prediction frames".into());
                }
                p = start + n;
            }
        }
        at = end;
    }
    if frames.is_empty() {
        return Err("No supported MOV prediction frames".into());
    }
    Ok(frames)
}

fn idct(coeff: &[i16], block: usize, blocks: usize, weight: &[i64; 64]) -> [i32; 64] {
    let mut c = [0i64; 64];
    let mut ac = false;
    for i in 0..64 {
        let v = coeff[i * blocks + block] as i64;
        c[SCAN[i]] = v * weight[SCAN[i]];
        ac |= i != 0 && v != 0;
    }
    if !ac {
        let v = ((c[0] * T[0][0] * T[0][0] + (1 << 22)) >> 23) as i32;
        return [v; 64];
    }
    let rows: Vec<usize> = (0..8)
        .filter(|&k| c[k * 8..k * 8 + 8].iter().any(|&v| v != 0))
        .collect();
    let mut temp = [0i64; 64];
    for y in 0..8 {
        for x in 0..8 {
            temp[y * 8 + x] = rows.iter().map(|&k| T[y][k] * c[k * 8 + x]).sum();
        }
    }
    let mut out = [0i32; 64];
    for y in 0..8 {
        for x in 0..8 {
            let v: i64 = (0..8).map(|k| temp[y * 8 + k] * T[x][k]).sum();
            out[y * 8 + x] = ((v + (1 << 22)) >> 23) as i32;
        }
    }
    out
}
fn predict(raw: &[u8], mov: &MovFrame, gif: &GifFrame) -> Result<Vec<u8>, String> {
    let mut planes = vec![0i32; 3 * 1008 * 1008];
    for slice in &mov.slices {
        for ch in 0..3 {
            let (at, n) = slice.planes[ch];
            let coeff = super::mov_exact::unpack_plane(&raw[at..at + n], slice.blocks)?;
            for block in 0..slice.blocks {
                let within = block % 4;
                let (bx, by) = if ch == 0 {
                    (within % 2, within / 2)
                } else {
                    (within / 2, within % 2)
                };
                let x = (slice.x + (block / 4) * 2 + bx) * 8;
                let y = (slice.y + by) * 8;
                let values = idct(&coeff, block, slice.blocks, &mov.weights[ch]);
                for row in 0..8 {
                    let offset = ch * 1008 * 1008 + (y + row) * 1008 + x;
                    planes[offset..offset + 8].copy_from_slice(&values[row * 8..row * 8 + 8]);
                }
            }
        }
    }
    let mut rgb = vec![[0u8; 3]; 1_000_000];
    for y in 0..1000 {
        for x in 0..1000 {
            let p = y * 1008 + x;
            let yy = (planes[p] as i64 + 224 * 256) * 38155;
            let u = planes[1008 * 1008 + p] as i64;
            let v = planes[2 * 1008 * 1008 + p] as i64;
            let values = [yy + v * 58744, yy - u * 6988 - v * 17463, yy + u * 69219];
            rgb[y * 1000 + x] = values.map(|n| ((n + (1 << 23)) >> 24).clamp(0, 255) as u8);
        }
    }
    let mut cache: std::collections::HashMap<u32, u8> =
        std::collections::HashMap::with_capacity(4096);
    let mut out = Vec::with_capacity(gif.w * gif.h);
    for y in gif.y..gif.y + gif.h {
        let sy = y * 10 / 3;
        for x in gif.x..gif.x + gif.w {
            let sx = x * 10 / 3;
            let mut sum = [0u32; 3];
            for j in 0..4 {
                let wy = ((y * 10 + 10).min((sy + j + 1) * 3))
                    .saturating_sub((y * 10).max((sy + j) * 3));
                for i in 0..4 {
                    let wx = ((x * 10 + 10).min((sx + i + 1) * 3))
                        .saturating_sub((x * 10).max((sx + i) * 3));
                    if wx * wy == 0 {
                        continue;
                    }
                    let p = rgb[(sy + j) * 1000 + sx + i];
                    for c in 0..3 {
                        sum[c] += p[c] as u32 * (wx * wy) as u32;
                    }
                }
            }
            let color = sum.map(|v| ((v + 50) / 100) as u8);
            if color.iter().all(|&v| v < 4) {
                if let Some(t) = gif.transparent {
                    out.push(t);
                    continue;
                }
            }
            let key = ((color[0] as u32) << 16) | ((color[1] as u32) << 8) | color[2] as u32;
            let index = *cache.entry(key).or_insert_with(|| {
                let mut best = u32::MAX;
                let mut index = 0u8;
                for (i, p) in gif.palette.iter().enumerate() {
                    let distance = (0..3)
                        .map(|c| {
                            let d = i32::from(color[c]) - i32::from(p[c]);
                            (d * d) as u32
                        })
                        .sum();
                    if distance < best {
                        best = distance;
                        index = i as u8;
                    }
                }
                index
            });
            out.push(index);
        }
    }
    Ok(out)
}

struct Encoder {
    low: u64,
    range: u32,
    cache: u8,
    pending: usize,
    bytes: Vec<u8>,
}
impl Encoder {
    fn new() -> Self {
        Self {
            low: 0,
            range: u32::MAX,
            cache: 0,
            pending: 1,
            bytes: vec![],
        }
    }
    fn shift(&mut self) {
        if (self.low as u32) < 0xff000000 || self.low >> 32 != 0 {
            let carry = (self.low >> 32) as u8;
            let mut b = self.cache;
            loop {
                self.bytes.push(b.wrapping_add(carry));
                b = 255;
                self.pending -= 1;
                if self.pending == 0 {
                    break;
                }
            }
            self.cache = (self.low as u32 >> 24) as u8;
        }
        self.pending += 1;
        self.low = ((self.low as u32) << 8) as u64;
    }
    fn bit(&mut self, b: bool, p: u16) {
        let bound = (self.range >> 12) * p as u32;
        if b {
            self.low += bound as u64;
            self.range -= bound
        } else {
            self.range = bound
        }
        while self.range < 0x01000000 {
            self.range <<= 8;
            self.shift();
        }
    }
    fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift()
        }
        self.bytes
    }
}
struct Decoder<'a> {
    bytes: &'a [u8],
    at: usize,
    range: u32,
    code: u32,
}
impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Result<Self, String> {
        let mut d = Self {
            bytes,
            at: 0,
            range: u32::MAX,
            code: 0,
        };
        for _ in 0..5 {
            d.code = (d.code << 8) | d.next()? as u32;
        }
        if bytes.first() != Some(&0) {
            return Err("Invalid GIF arithmetic prefix".into());
        }
        Ok(d)
    }
    fn next(&mut self) -> Result<u8, String> {
        let b = *self
            .bytes
            .get(self.at)
            .ok_or("Truncated GIF arithmetic stream")?;
        self.at += 1;
        Ok(b)
    }
    fn bit(&mut self, p: u16) -> Result<bool, String> {
        let bound = (self.range >> 12) * p as u32;
        let b = self.code >= bound;
        if b {
            self.code -= bound;
            self.range -= bound
        } else {
            self.range = bound
        }
        while self.range < 0x01000000 {
            self.range <<= 8;
            self.code = (self.code << 8) | self.next()? as u32;
        }
        Ok(b)
    }
}
fn update(p: &mut u16, b: bool) {
    if b {
        *p -= (*p >> 2).max(1)
    } else {
        *p += ((4096 - *p) >> 2).max(1)
    }
    *p = (*p).clamp(1, 4095);
}
struct Table {
    p: Vec<u16>,
    n: Vec<u8>,
}
impl Table {
    fn new() -> Self {
        Self {
            p: vec![2048; 1 << 24],
            n: vec![0; 1 << 24],
        }
    }
    fn probability(&self, c: usize, low: u16) -> u16 {
        let n = self.n[c] as u32;
        ((self.p[c] as u32 * n + low as u32 * 32) / (n + 32)) as u16
    }
    fn update(&mut self, c: usize, b: bool) {
        update(&mut self.p[c], b);
        self.n[c] = self.n[c].saturating_add(1);
    }
}
struct Model {
    tables: [Table; 3],
    lower: [Vec<u16>; 3],
}
impl Model {
    fn new() -> Self {
        Self {
            tables: [Table::new(), Table::new(), Table::new()],
            lower: [vec![2048; 65536], vec![2048; 65536], vec![2048; 65536]],
        }
    }
    fn contexts(l: usize, u: usize, p: usize, pre: usize) -> [usize; 3] {
        [
            ((l << 8 | u) << 8) | pre,
            ((l << 8 | p) << 8) | pre,
            ((u << 8 | p) << 8) | pre,
        ]
    }
    fn probability(&self, l: usize, u: usize, p: usize, pre: usize) -> u16 {
        let c = Self::contexts(l, u, p, pre);
        let low = [
            self.lower[0][l * 256 + pre],
            self.lower[1][u * 256 + pre],
            self.lower[2][p * 256 + pre],
        ];
        let pair = |a: usize, b: usize| ((low[a] as u32 + low[b] as u32) / 2) as u16;
        ((self.tables[0].probability(c[0], pair(0, 1)) as u32
            + self.tables[1].probability(c[1], pair(0, 2)) as u32
            + self.tables[2].probability(c[2], pair(1, 2)) as u32)
            / 3)
        .clamp(1, 4095) as u16
    }
    fn update(&mut self, l: usize, u: usize, p: usize, pre: usize, b: bool) {
        for (i, c) in Self::contexts(l, u, p, pre).into_iter().enumerate() {
            self.tables[i].update(c, b)
        }
        for (i, v) in [l, u, p].into_iter().enumerate() {
            update(&mut self.lower[i][v * 256 + pre], b)
        }
    }
}
fn predictions(frames: &[GifFrame], mov: &[MovFrame], raw: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    super::mov_source::par_map(frames.len(), |fi| predict(raw, &mov[fi], &frames[fi]))
        .into_iter()
        .collect()
}

fn encode_pixels(
    frames: &[GifFrame],
    predictions: &[Vec<u8>],
    cb: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Vec<u8>, String> {
    let mut model = Model::new();
    let mut encoder = Encoder::new();
    for (fi, (f, prediction)) in frames.iter().zip(predictions).enumerate() {
        let base = 2000 + fi as u64 * 997000 / frames.len() as u64;
        let span = 997000 / frames.len() as u64;
        for (i, &value) in f.pixels.iter().enumerate() {
            if i % (f.w * 16) == 0 {
                tick(
                    cb,
                    base + span / 2 + span / 2 * i as u64 / f.pixels.len() as u64,
                )?;
                if encoder.bytes.len() > LIMIT {
                    return Err("GIF arithmetic output exceeds limit".into());
                }
            }
            let l = if i % f.w != 0 { f.pixels[i - 1] } else { 0 } as usize;
            let u = if i >= f.w { f.pixels[i - f.w] } else { 0 } as usize;
            let p = prediction[i] as usize;
            let mut pre = 1;
            for shift in (0..8).rev() {
                let b = value & (1 << shift) != 0;
                encoder.bit(b, model.probability(l, u, p, pre));
                model.update(l, u, p, pre, b);
                pre = pre * 2 + usize::from(b)
            }
        }
    }
    let out = encoder.finish();
    if out.len() > LIMIT {
        return Err("GIF arithmetic output exceeds limit".into());
    }
    Ok(out)
}
fn decode_pixels(
    frames: &mut [GifFrame],
    predictions: &[Vec<u8>],
    coded: &[u8],
    cb: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<(), String> {
    let mut model = Model::new();
    let mut decoder = Decoder::new(coded)?;
    let count = frames.len();
    for (fi, (f, prediction)) in frames.iter_mut().zip(predictions).enumerate() {
        let base = 2000 + fi as u64 * 997000 / count as u64;
        let span = 997000 / count as u64;
        for (i, &p) in prediction.iter().enumerate() {
            if i % (f.w * 16) == 0 {
                tick(
                    cb,
                    base + span / 2 + span / 2 * i as u64 / f.pixels.len() as u64,
                )?
            }
            let l = if i % f.w != 0 { f.pixels[i - 1] } else { 0 } as usize;
            let u = if i >= f.w { f.pixels[i - f.w] } else { 0 } as usize;
            let p = p as usize;
            let mut pre = 1;
            for _ in 0..8 {
                let b = decoder.bit(model.probability(l, u, p, pre))?;
                model.update(l, u, p, pre, b);
                pre = pre * 2 + usize::from(b)
            }
            let value = (pre - 256) as u8;
            if value as usize >= f.palette.len() {
                return Err("Decoded GIF index exceeds palette".into());
            }
            f.pixels[i] = value;
        }
    }
    if decoder.at != coded.len() {
        return Err("Trailing GIF arithmetic bytes".into());
    }
    Ok(())
}

pub fn encode(
    raw_gif: &[u8],
    raw_mov: &[u8],
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Vec<u8>, String> {
    tick(progress, 0)?;
    if raw_gif.len() > LIMIT {
        return Err("GIF prediction input exceeds limit".into());
    }
    let sgif = super::gif_exact::encode(raw_gif)?;
    let (side, frames) = split(&sgif, true)?;
    let mov = mov_frames(raw_mov, &mut |d, t| progress(d / 2, t))?;
    if mov.len() != frames.len() {
        return Err("GIF and MOV frame counts differ".into());
    }
    tick(progress, 1000)?;
    let packed_side = zstd::bulk::compress(&side, 19).map_err(|e| e.to_string())?;
    let predicted = predictions(&frames, &mov, raw_mov)?;
    let coded = encode_pixels(&frames, &predicted, &mut |d, t| progress(d / 2, t))?;
    if HEADER + packed_side.len() + coded.len() > LIMIT {
        return Err("GIF prediction record exceeds limit".into());
    }
    let mut out = Vec::with_capacity(HEADER + packed_side.len() + coded.len());
    out.extend_from_slice(MAGIC);
    for n in [
        raw_gif.len(),
        sgif.len(),
        side.len(),
        packed_side.len(),
        coded.len(),
    ] {
        out.extend_from_slice(&(n as u64).to_le_bytes())
    }
    out.extend_from_slice(&(frames.len() as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(blake3::hash(raw_mov).as_bytes());
    out.extend_from_slice(blake3::hash(raw_gif).as_bytes());
    out.extend_from_slice(blake3::hash(&sgif).as_bytes());
    let mut hasher = blake3::Hasher::new();
    hasher.update(&packed_side);
    hasher.update(&coded);
    out.extend_from_slice(hasher.finalize().as_bytes());
    out.extend_from_slice(&packed_side);
    out.extend_from_slice(&coded);
    let (_, mut check) = split(&side, false)?;
    decode_pixels(&mut check, &predicted, &coded, &mut |d, t| {
        progress(TOTAL / 2 + d / 2, t)
    })?;
    let rebuilt = join(&side, &check)?;
    if rebuilt != sgif || super::gif_exact::decode(&rebuilt)? != raw_gif {
        return Err("GIF/MOV prediction self-check failed".into());
    }
    tick(progress, TOTAL)?;
    Ok(out)
}

pub fn decode(
    record: &[u8],
    raw_mov: &[u8],
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Vec<u8>, String> {
    tick(progress, 0)?;
    if record.len() < HEADER
        || record.len() > LIMIT
        || record.get(..8) != Some(MAGIC)
        || raw_mov.len() > 512 << 20
    {
        return Err("Invalid GIF prediction header".into());
    }
    let original_len = le64(record, 8)?;
    let sgif_len = le64(record, 16)?;
    let side_len = le64(record, 24)?;
    let side_coded_len = le64(record, 32)?;
    let pixel_len = le64(record, 40)?;
    let count = u32::from_le_bytes(record[48..52].try_into().unwrap()) as usize;
    if original_len > LIMIT
        || sgif_len > LIMIT
        || side_len > SIDE_LIMIT
        || side_coded_len > LIMIT - HEADER
        || pixel_len > LIMIT - HEADER - side_coded_len
        || HEADER + side_coded_len + pixel_len != record.len()
        || count == 0
        || count > MAX_FRAMES
        || record[52..56] != [0; 4]
    {
        return Err("GIF prediction parameters exceed limits".into());
    }
    if blake3::hash(raw_mov).as_bytes() != &record[56..88]
        || blake3::hash(&record[HEADER..]).as_bytes() != &record[152..184]
    {
        return Err("GIF prediction MOV or payload checksum mismatch".into());
    }
    let packed = &record[HEADER..HEADER + side_coded_len];
    let mut decoder = zstd::stream::read::Decoder::new(packed)
        .map_err(|e| e.to_string())?
        .single_frame();
    let mut side = vec![];
    decoder
        .by_ref()
        .take(side_len as u64 + 1)
        .read_to_end(&mut side)
        .map_err(|e| e.to_string())?;
    if side.len() != side_len {
        return Err("GIF prediction metadata length mismatch".into());
    }
    let mut tail = [0];
    if decoder
        .finish()
        .read(&mut tail)
        .map_err(|e| e.to_string())?
        != 0
    {
        return Err("Trailing GIF prediction metadata bytes".into());
    }
    let (_, mut frames) = split(&side, false)?;
    if frames.len() != count {
        return Err("GIF prediction frame count mismatch".into());
    }
    let mov = mov_frames(raw_mov, progress)?;
    if mov.len() != count {
        return Err("GIF and MOV frame counts differ".into());
    }
    let predicted = predictions(&frames, &mov, raw_mov)?;
    decode_pixels(
        &mut frames,
        &predicted,
        &record[HEADER + side_coded_len..],
        progress,
    )?;
    let sgif = join(&side, &frames)?;
    if sgif.len() != sgif_len || blake3::hash(&sgif).as_bytes() != &record[120..152] {
        return Err("GIF prediction reconstruction checksum mismatch".into());
    }
    let raw = super::gif_exact::decode(&sgif)?;
    if raw.len() != original_len || blake3::hash(&raw).as_bytes() != &record[88..120] {
        return Err("GIF prediction original checksum mismatch".into());
    }
    tick(progress, TOTAL)?;
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gif_fixture() -> Vec<u8> {
        let mut raw=b"GIF89a\x2c\x01\x2c\x01\x80\0\0\0\0\0\xff\xff\xff\x21\xf9\x04\x01\0\0\0\0\x2c\0\0\0\0\x2c\x01\x2c\x01\0\x02".to_vec();
        let mut bits = vec![0u8; (90_000 * 6 + 3_usize).div_ceil(8)];
        let mut at = 0;
        for code in (0..90_000).flat_map(|_| [4u8, 0]).chain([5]) {
            for bit in 0..3 {
                bits[at / 8] |= ((code >> bit) & 1) << (at % 8);
                at += 1;
            }
        }
        for block in bits.chunks(255) {
            raw.push(block.len() as u8);
            raw.extend_from_slice(block)
        }
        raw.extend_from_slice(&[0, 0x3b, 0xaa]);
        raw
    }
    fn mov_fixture() -> Vec<u8> {
        let mut picture = vec![0u8; 8 + 630 * 2];
        picture[0] = 8 << 3;
        picture[5..7].copy_from_slice(&630u16.to_be_bytes());
        picture[7] = 0x30;
        let mut table = 0;
        for _ in 0..63 {
            for mbs in [8, 8, 8, 8, 8, 8, 8, 4, 2, 1] {
                let blocks = mbs * 4;
                let mut encoded = Vec::new();
                for ch in 0..3 {
                    let mut c = vec![0i16; blocks * 64];
                    if ch == 0 {
                        c[..blocks].fill(-3584)
                    }
                    let b = (1..256)
                        .find_map(|n| super::super::mov_exact::pack_plane(&c, blocks, n).ok())
                        .unwrap();
                    encoded.push(b)
                }
                let mut slice = vec![8 << 3, 1];
                for b in &encoded {
                    slice.extend_from_slice(&(b.len() as u16).to_be_bytes())
                }
                for b in encoded {
                    slice.extend(b)
                }
                picture[8 + table * 2..10 + table * 2]
                    .copy_from_slice(&(slice.len() as u16).to_be_bytes());
                picture.extend(slice);
                table += 1;
            }
        }
        let n = picture.len() as u32;
        picture[1..5].copy_from_slice(&n.to_be_bytes());
        let mut frame = vec![0u8; 28];
        frame[4..8].copy_from_slice(b"icpf");
        frame[8..10].copy_from_slice(&20u16.to_be_bytes());
        frame[16..18].copy_from_slice(&1000u16.to_be_bytes());
        frame[18..20].copy_from_slice(&1000u16.to_be_bytes());
        frame[20] = 3 << 6;
        frame.extend(picture);
        let n = frame.len() as u32;
        frame[..4].copy_from_slice(&n.to_be_bytes());
        let mut mov = vec![];
        mov.extend_from_slice(&((frame.len() + 8) as u32).to_be_bytes());
        mov.extend_from_slice(b"mdat");
        mov.extend(frame);
        mov
    }

    #[test]
    fn companion_round_trip_binding_bounds_and_cancellation() {
        let gif = gif_fixture();
        let mov = mov_fixture();
        let mut last = 0;
        let record = encode(&gif, &mov, &mut |done, total| {
            assert!(done >= last && done <= total);
            last = done;
            true
        })
        .unwrap();
        assert_eq!(last, TOTAL);
        assert!(record.len() < gif.len() / 4);
        assert_eq!(decode(&record, &mov, &mut |_, _| true).unwrap(), gif);
        let mut wrong = mov.clone();
        wrong[20] ^= 1;
        assert!(decode(&record, &wrong, &mut |_, _| true).is_err());
        for end in [0, 7, HEADER - 1, HEADER, record.len() - 1] {
            assert!(decode(&record[..end], &mov, &mut |_, _| true).is_err())
        }
        for pos in [8, 16, 24, 32, 40, 48, 52, 56, 88, 120, 152, HEADER] {
            let mut bad = record.clone();
            bad[pos] ^= 255;
            assert!(decode(&bad, &mov, &mut |_, _| true).is_err())
        }
        let mut extra = record.clone();
        extra.push(0);
        assert!(decode(&extra, &mov, &mut |_, _| true).is_err());
        let n = le64(&extra, 40).unwrap() + 1;
        extra[40..48].copy_from_slice(&(n as u64).to_le_bytes());
        let hash = blake3::hash(&extra[HEADER..]);
        extra[152..184].copy_from_slice(hash.as_bytes());
        assert!(decode(&extra, &mov, &mut |_, _| true).is_err());
        assert!(encode(&gif, &mov, &mut |_, _| false).is_err());
        assert!(decode(&record, &mov, &mut |_, _| false).is_err());
        let mut cancelled = false;
        assert!(encode(&gif, &mov, &mut |done, _| {
            if done > 2000 && !cancelled {
                cancelled = true;
                false
            } else {
                true
            }
        })
        .is_err());
        assert!(cancelled);
    }

    #[test]
    fn malformed_side_inputs_are_bounded() {
        let sgif = super::super::gif_exact::encode(&gif_fixture()).unwrap();
        let (side, _) = split(&sgif, true).unwrap();
        for n in 0..side.len().min(1024) {
            assert!(split(&side[..n], false).is_err())
        }
        let mut extra = side.clone();
        extra.push(0);
        assert!(split(&extra, false).is_err());
        for i in (0..side.len()).step_by(11) {
            let mut changed = side.clone();
            changed[i] ^= 255;
            let _ = split(&changed, false);
        }
    }

    #[test]
    fn long_mdat_scan_observes_one_shot_cancellation() {
        let mut mov = vec![0u8; 2 << 20];
        let n = mov.len() as u32;
        mov[..4].copy_from_slice(&n.to_be_bytes());
        mov[4..8].copy_from_slice(b"mdat");
        let mut calls = 0;
        let result = mov_frames(&mov, &mut |_, _| {
            calls += 1;
            calls != 5
        });
        assert_eq!(result.err().as_deref(), Some("error.cancelled"));
        assert_eq!(calls, 5);

        let frame = mov_fixture();
        let mut padded = vec![0u8; 8 + (64 << 10) - 5];
        padded.extend_from_slice(&frame[8..]);
        let n = padded.len() as u32;
        padded[..4].copy_from_slice(&n.to_be_bytes());
        padded[4..8].copy_from_slice(b"mdat");
        assert_eq!(mov_frames(&padded, &mut |_, _| true).unwrap().len(), 1);
    }
}
