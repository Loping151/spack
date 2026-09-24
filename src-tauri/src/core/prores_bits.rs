pub(crate) const MAX_DIM: usize = 8192;
const DC_BOOKS: [u8; 7] = [0x04, 0x28, 0x28, 0x4d, 0x4d, 0x70, 0x70];
const RUN_BOOKS: [u8; 16] = [
    0x06, 0x06, 0x05, 0x05, 0x04, 0x29, 0x29, 0x29, 0x29, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x4c,
];
const LEVEL_BOOKS: [u8; 10] = [0x04, 0x0a, 0x05, 0x06, 0x04, 0x28, 0x28, 0x28, 0x28, 0x4c];
const SCAN: [usize; 64] = [
    0, 1, 8, 9, 2, 3, 10, 11, 16, 17, 24, 25, 18, 19, 26, 27, 4, 5, 12, 20, 13, 6, 7, 14, 21, 28,
    29, 22, 15, 23, 30, 31, 32, 33, 40, 48, 41, 34, 35, 42, 49, 56, 57, 50, 43, 36, 37, 44, 51, 58,
    59, 52, 45, 38, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

fn be16(b: &[u8], i: usize) -> Result<usize, String> {
    let a = b.get(i..i + 2).ok_or("truncated ProRes header")?;
    Ok(u16::from_be_bytes([a[0], a[1]]) as usize)
}
fn be32(b: &[u8], i: usize) -> Result<usize, String> {
    let a = b.get(i..i + 4).ok_or("truncated ProRes header")?;
    Ok(u32::from_be_bytes([a[0], a[1], a[2], a[3]]) as usize)
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}
impl Bits<'_> {
    fn left(&self) -> isize {
        self.data.len() as isize * 8 - self.pos as isize
    }
    fn get(&mut self, n: usize) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            let bit = self
                .data
                .get(self.pos / 8)
                .map_or(0, |b| (b >> (7 - self.pos % 8)) & 1);
            v = (v << 1) | bit as u32;
            self.pos += 1;
        }
        v
    }
    fn zero_tail(&self) -> bool {
        let total = self.data.len() * 8;
        total.saturating_sub(self.pos) < 32
            && (self.pos..total).all(|p| self.data[p / 8] & (1 << (7 - p % 8)) == 0)
    }
    fn word(&mut self, book: u8) -> Result<u32, String> {
        let rice = (book >> 5) as usize;
        let exp = ((book >> 2) & 7) as usize;
        let switch = (book & 3) as usize;
        let mut zeros = 0;
        while self.get(1) == 0 {
            zeros += 1;
            if zeros > 15 || self.left() < 0 {
                return Err("invalid ProRes codeword".into());
            }
        }
        if zeros <= switch {
            Ok(((zeros as u32) << rice) + self.get(rice))
        } else {
            let suffix = exp + zeros - switch - 1;
            if suffix > 30 {
                return Err("invalid ProRes codeword".into());
            }
            Ok((1u32 << suffix) + self.get(suffix) - (1u32 << exp)
                + (((switch + 1) as u32) << rice))
        }
    }
}

#[derive(Default)]
struct Writer {
    data: Vec<u8>,
    acc: u64,
    nbits: u32,
}
impl Writer {
    #[inline]
    fn put(&mut self, value: u32, n: usize) {
        if n == 0 {
            return;
        }
        let v = if n >= 32 {
            value as u64
        } else {
            (value as u64) & ((1u64 << n) - 1)
        };
        self.acc = (self.acc << n) | v;
        self.nbits += n as u32;
        while self.nbits >= 8 {
            self.nbits -= 8;
            self.data.push((self.acc >> self.nbits) as u8);
        }
    }
    fn zeros(&mut self, mut n: usize) {
        while n > 0 {
            let k = n.min(32);
            self.put(0, k);
            n -= k;
        }
    }
    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.data.push((self.acc << (8 - self.nbits)) as u8);
        }
        self.data
    }
    fn word(&mut self, value: u32, book: u8) {
        let rice = (book >> 5) as usize;
        let exp = ((book >> 2) & 7) as usize;
        let switch = (book & 3) as usize;
        let threshold = ((switch + 1) as u32) << rice;
        if value < threshold {
            self.zeros((value >> rice) as usize);
            self.put(1, 1);
            self.put(value & ((1u32 << rice) - 1), rice)
        } else {
            let adjusted = value + (1u32 << exp) - threshold;
            let top = (31 - adjusted.leading_zeros()) as usize;
            self.zeros(top + switch + 1 - exp);
            self.put(adjusted, top + 1)
        }
    }
}

fn unpack_plane(data: &[u8], blocks: usize) -> Result<Vec<i16>, String> {
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
        position += run as usize + 1;
        if position >= coeffs.len() {
            return Err("invalid ProRes coefficient run".into());
        }
        level = bits.word(LEVEL_BOOKS[(level as usize).min(9)])? + 1;
        if level > 32767 {
            return Err("ProRes AC overflow".into());
        }
        coeffs[position] = if bits.get(1) == 0 {
            level as i16
        } else {
            -(level as i16)
        };
    }
    Ok(coeffs)
}

fn pack_plane(coeffs: &[i16], blocks: usize) -> Vec<u8> {
    let mut w = Writer::default();
    let dc = coeffs[0] as i32;
    w.word(((dc << 1) ^ (dc >> 31)) as u32, 0xb8);
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
        w.word(code, DC_BOOKS[(previous_code as usize).min(6)]);
        previous_code = code;
    }
    let mut last = blocks - 1;
    let mut previous_run = 4usize;
    let mut previous_level = 2usize;
    for (position, &c) in coeffs.iter().enumerate().skip(blocks) {
        if c == 0 {
            continue;
        }
        let run = position - last - 1;
        let level = (c as i32).unsigned_abs() as usize;
        w.word(run as u32, RUN_BOOKS[previous_run.min(15)]);
        w.word(level as u32 - 1, LEVEL_BOOKS[previous_level.min(9)]);
        w.put(u32::from(c < 0), 1);
        previous_run = run;
        previous_level = level;
        last = position;
    }
    w.finish()
}

fn unpack_alpha(data: &[u8], n: usize) -> Vec<u16> {
    let mut g = Bits { data, pos: 0 };
    let mut out = Vec::with_capacity(n);
    let mut a: u32 = 0xffff;
    'outer: loop {
        loop {
            let v: i32 = if g.get(1) == 1 {
                g.get(16) as i32
            } else {
                let v = g.get(7) as i32;
                let m = (v + 2) >> 1;
                if v & 1 != 0 {
                    -m
                } else {
                    m
                }
            };
            a = (a as i32 + v) as u32 & 0xffff;
            out.push(a as u16);
            if out.len() >= n {
                break 'outer;
            }
            if !(g.left() > 0 && g.get(1) == 1) {
                break;
            }
        }
        let mut r = g.get(4) as usize;
        if r == 0 {
            r = g.get(11) as usize;
        }
        let r = r.min(n - out.len());
        out.extend(std::iter::repeat_n(a as u16, r));
        if out.len() >= n || g.left() < -64 {
            break;
        }
    }
    out.resize(n, a as u16);
    out
}

fn pack_alpha(vals: &[u16]) -> Vec<u8> {
    let mut w = Writer::default();
    let diff = |w: &mut Writer, cur: i32, prev: i32| {
        let d = cur - prev;
        if !(-64..=64).contains(&d) || d == 0 {
            w.put(1, 1);
            w.put((d & 0xffff) as u32, 16);
        } else {
            w.put(0, 1);
            w.put((d.abs() - 1) as u32, 6);
            w.put((d < 0) as u32, 1);
        }
    };
    let run = |w: &mut Writer, r: usize| {
        if r > 0 {
            w.put(0, 1);
            if r < 16 {
                w.put(r as u32, 4)
            } else {
                w.put(r as u32, 15)
            }
        } else {
            w.put(1, 1);
        }
    };
    diff(&mut w, vals[0] as i32, -1);
    let mut prev = vals[0];
    let mut r = 0;
    for &cur in &vals[1..] {
        if cur != prev {
            run(&mut w, r);
            diff(&mut w, cur as i32, prev as i32);
            prev = cur;
            r = 0;
        } else {
            r += 1;
        }
    }
    run(&mut w, r);
    w.finish()
}

#[derive(Clone)]
pub(crate) struct Slice {
    pub mb_x: usize,
    pub mb_y: usize,
    pub mbs: usize,
    pub qscale: u8,
    header: usize,
    planes: [(usize, usize); 4],
}

#[derive(Clone)]
pub(crate) struct Frame {
    pub offset: usize,
    pub len: usize,
    pub width: usize,
    pub height: usize,
    pub alpha: bool,
    pub qmat: [[u8; 64]; 2],
    pub head_len: usize,
    pub slices: Vec<Slice>,
}
impl Frame {
    pub fn mbw(&self) -> usize {
        self.width.div_ceil(16)
    }
    pub fn mbh(&self) -> usize {
        self.height.div_ceil(16)
    }
    fn alpha_rows(&self, s: &Slice) -> usize {
        16.min(self.height - s.mb_y * 16)
    }
}

fn parse(f: &[u8], start: usize, with_slices: bool) -> Result<Frame, String> {
    let length = be32(f, 0)?;
    if length < 28 || f.get(4..8) != Some(b"icpf") {
        return Err("not a ProRes frame".into());
    }
    let header = be16(f, 8)?;
    let width = be16(f, 16)?;
    let height = be16(f, 18)?;
    let flags = *f.get(20).ok_or("truncated ProRes header")?;
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return Err("unsupported ProRes dimensions".into());
    }
    if flags >> 6 != 3 || (flags >> 2) & 3 != 0 {
        return Err("only progressive 4:4:4 ProRes is supported".into());
    }
    let alpha = f.get(25).ok_or("truncated ProRes header")? & 15 != 0;
    let qflags = *f.get(27).ok_or("truncated ProRes header")?;
    let mut qmat = [[4u8; 64]; 2];
    let mut at = 28;
    if qflags & 2 != 0 {
        qmat[0].copy_from_slice(f.get(at..at + 64).ok_or("truncated ProRes matrix")?);
        at += 64;
    }
    if qflags & 1 != 0 {
        qmat[1].copy_from_slice(f.get(at..at + 64).ok_or("truncated ProRes matrix")?);
    } else {
        qmat[1] = qmat[0];
    }
    if qmat.iter().flatten().any(|&q| q == 0) {
        return Err("invalid ProRes matrix".into());
    }
    let picture = header + 8;
    let ph = *f.get(picture).ok_or("truncated ProRes picture")? as usize >> 3;
    let slice_flag = *f.get(picture + 7).ok_or("truncated ProRes picture")?;
    if ph < 8 || slice_flag & 15 != 0 || slice_flag >> 4 > 3 {
        return Err("unsupported ProRes slice layout".into());
    }
    let (mbw, mbh) = (width.div_ceil(16), height.div_ceil(16));
    let slice_width = 1usize << (slice_flag >> 4);
    let mut slices = Vec::new();
    let mut pos = picture + ph;
    let count = mbh * (mbw / slice_width + (mbw % slice_width).count_ones() as usize);
    let picture_len = if with_slices {
        be32(f, picture + 1)?
    } else {
        0
    };
    let table = pos;
    if with_slices {
        pos += 2 * count;
    }
    for y in 0..mbh {
        let mut x = 0;
        while x < mbw {
            let mut mbs = slice_width;
            while mbs > mbw - x {
                mbs >>= 1;
            }
            let mut slice = Slice {
                mb_x: x,
                mb_y: y,
                mbs,
                qscale: 1,
                header: 8,
                planes: [(0, 0); 4],
            };
            if with_slices {
                let len = be16(f, table + 2 * slices.len())?;
                let sh = *f.get(pos).ok_or("truncated ProRes slice")? as usize >> 3;
                if sh < 8 || sh > len {
                    return Err("unsupported ProRes slice header".into());
                }
                slice.header = sh;
                slice.qscale = f[pos + 1];
                let (ys, us, vs) = (be16(f, pos + 2)?, be16(f, pos + 4)?, be16(f, pos + 6)?);
                let rest = len
                    .checked_sub(sh + ys + us + vs)
                    .ok_or("invalid ProRes plane sizes")?;
                let a0 = start + pos + sh;
                slice.planes = [
                    (a0, ys),
                    (a0 + ys, us),
                    (a0 + ys + us, vs),
                    (a0 + ys + us + vs, rest),
                ];
                pos += len;
            }
            slices.push(slice);
            x += mbs;
        }
    }
    if with_slices && (pos != picture + picture_len || pos > length || length > f.len()) {
        return Err("unrecognized ProRes picture layout".into());
    }
    Ok(Frame {
        offset: start,
        len: length,
        width,
        height,
        alpha,
        qmat,
        head_len: picture + ph,
        slices,
    })
}

pub(crate) fn parse_head(head: &[u8], frame_len: usize) -> Result<Frame, String> {
    let mut f = parse(head, 0, false)?;
    if f.head_len != head.len() || f.len != frame_len {
        return Err("inconsistent ProRes frame head".into());
    }
    f.len = frame_len;
    Ok(f)
}

pub(crate) fn frames(raw: &[u8]) -> Vec<Frame> {
    let mut out = Vec::new();
    let mut atom = 0usize;
    while atom + 8 <= raw.len() {
        let Ok(mut size) = be32(raw, atom) else { break };
        let kind = &raw[atom + 4..atom + 8];
        let mut header = 8;
        if size == 1 {
            let Some(n) = raw.get(atom + 8..atom + 16) else {
                break;
            };
            size = u64::from_be_bytes(n.try_into().unwrap()) as usize;
            header = 16;
        } else if size == 0 {
            size = raw.len() - atom;
        }
        if size < header || size > raw.len() - atom {
            break;
        }
        if kind == b"mdat" {
            let end = atom + size;
            let mut p = atom + header;
            while p + 8 <= end {
                if &raw[p + 4..p + 8] == b"icpf" {
                    if let Ok(fr) = be32(raw, p).and_then(|len| {
                        let f = raw.get(p..p + len).ok_or("truncated frame")?;
                        parse(f, p, true)
                    }) {
                        if fr.len <= end - p {
                            p += fr.len;
                            out.push(fr);
                            continue;
                        }
                    }
                }
                p += 1;
            }
        }
        atom += size;
    }
    out
}

pub(crate) struct Decoded {
    pub wp: usize,
    pub hp: usize,
    pub coef: [Vec<i16>; 3],
    pub alpha: Vec<u16>,
    pub qmb: Vec<u8>,
}

fn block_pos(s: &Slice, p: usize, blk: usize) -> (usize, usize) {
    let (mb, sub) = (blk / 4, blk % 4);
    let (dy, dx) = if p == 0 {
        (sub / 2, sub % 2)
    } else {
        (sub % 2, sub / 2)
    };
    (s.mb_y * 2 + dy, (s.mb_x + mb) * 2 + dx)
}

pub(crate) fn decode(raw: &[u8], fr: &Frame) -> Result<Decoded, String> {
    let (wp, hp) = (fr.mbw() * 16, fr.mbh() * 16);
    let mut coef = [
        vec![0i16; wp * hp],
        vec![0i16; wp * hp],
        vec![0i16; wp * hp],
    ];
    let mut alpha = vec![0u16; wp * hp];
    let mut qmb = vec![0u8; fr.mbw() * fr.mbh()];
    for s in &fr.slices {
        for m in 0..s.mbs {
            qmb[s.mb_y * fr.mbw() + s.mb_x + m] = s.qscale;
        }
        let blocks = s.mbs * 4;
        for p in 0..3 {
            let (o, l) = s.planes[p];
            let c = unpack_plane(raw.get(o..o + l).ok_or("truncated ProRes plane")?, blocks)?;
            for (pos, &v) in c.iter().enumerate() {
                if v != 0 {
                    let (by, bx) = block_pos(s, p, pos % blocks);
                    let nat = SCAN[pos / blocks];
                    coef[p][(by * 8 + nat / 8) * wp + bx * 8 + nat % 8] = v;
                }
            }
        }
        let (o, l) = s.planes[3];
        if fr.alpha && l > 0 {
            let rows = fr.alpha_rows(s);
            let w = s.mbs * 16;
            let a = unpack_alpha(raw.get(o..o + l).ok_or("truncated ProRes alpha")?, w * rows);
            for r in 0..rows {
                let o = (s.mb_y * 16 + r) * wp + s.mb_x * 16;
                alpha[o..o + w].copy_from_slice(&a[r * w..r * w + w]);
            }
        }
    }
    Ok(Decoded {
        wp,
        hp,
        coef,
        alpha,
        qmb,
    })
}

fn slice_coeffs(coef: &[i16], wp: usize, s: &Slice, p: usize) -> Vec<i16> {
    let blocks = s.mbs * 4;
    let mut out = vec![0i16; blocks * 64];
    for blk in 0..blocks {
        let (by, bx) = block_pos(s, p, blk);
        for (scan, &nat) in SCAN.iter().enumerate() {
            out[scan * blocks + blk] = coef[(by * 8 + nat / 8) * wp + bx * 8 + nat % 8];
        }
    }
    out
}

fn slice_alpha(alpha: &[u16], wp: usize, fr: &Frame, s: &Slice) -> Vec<u16> {
    let rows = fr.alpha_rows(s);
    let w = s.mbs * 16;
    let mut v = Vec::with_capacity(rows * w);
    for r in 0..rows {
        let o = (s.mb_y * 16 + r) * wp + s.mb_x * 16;
        v.extend_from_slice(&alpha[o..o + w]);
    }
    v
}

fn build_slice(
    fr: &Frame,
    s: &Slice,
    qscale: u8,
    coef: &[Vec<i16>; 3],
    alpha: &[u16],
    wp: usize,
) -> Vec<u8> {
    let blocks = s.mbs * 4;
    let planes: Vec<Vec<u8>> = (0..3)
        .map(|p| pack_plane(&slice_coeffs(&coef[p], wp, s, p), blocks))
        .collect();
    let mut sl = vec![8 << 3, qscale];
    for p in &planes {
        sl.extend_from_slice(&(p.len() as u16).to_be_bytes());
    }
    for p in &planes {
        sl.extend_from_slice(p);
    }
    if fr.alpha {
        sl.extend(pack_alpha(&slice_alpha(alpha, wp, fr, s)));
    }
    sl
}

pub(crate) fn rebuild(
    fr: &Frame,
    head: &[u8],
    qscale: &[u8],
    coef: &[Vec<i16>; 3],
    alpha: &[u16],
    parallel: bool,
) -> Result<Vec<u8>, String> {
    let wp = fr.mbw() * 16;
    let build = |i: usize| build_slice(fr, &fr.slices[i], qscale[i], coef, alpha, wp);
    let slices: Vec<Vec<u8>> = if parallel {
        super::mov_source::par_map(fr.slices.len(), build)
    } else {
        (0..fr.slices.len()).map(build).collect()
    };
    let mut out = head.to_vec();
    for s in &slices {
        let len = u16::try_from(s.len()).map_err(|_| "ProRes slice too large")?;
        out.extend_from_slice(&len.to_be_bytes());
    }
    for s in &slices {
        out.extend_from_slice(s);
    }
    if out.len() > fr.len {
        return Err("rebuilt ProRes frame exceeds its size".into());
    }
    out.resize(fr.len, 0);
    Ok(out)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *seed >> 33
    }

    #[test]
    fn coefficient_planes_round_trip() {
        let mut seed = 7;
        for blocks in [4usize, 8, 16, 32] {
            let mut c = vec![0i16; blocks * 64];
            for v in c.iter_mut() {
                if lcg(&mut seed) % 5 == 0 {
                    *v = (lcg(&mut seed) % 81) as i16 - 40;
                }
            }
            let packed = pack_plane(&c, blocks);
            assert_eq!(unpack_plane(&packed, blocks).unwrap(), c);
        }
    }

    #[test]
    fn alpha_round_trips_with_runs_and_jumps() {
        let mut seed = 11;
        let mut v = Vec::new();
        for _ in 0..3000 {
            let a = match lcg(&mut seed) % 4 {
                0 => 0,
                1 => 65535,
                _ => (lcg(&mut seed) % 256) as u16 * 257,
            };
            for _ in 0..(lcg(&mut seed) % 40) {
                v.push(a);
            }
        }
        v.truncate(2048);
        assert_eq!(unpack_alpha(&pack_alpha(&v), v.len()), v);
    }

    #[test]
    fn malformed_frames_are_errors() {
        let mut seed = 3;
        for len in [0usize, 7, 28, 200, 4096] {
            let mut raw: Vec<u8> = (0..len).map(|_| lcg(&mut seed) as u8).collect();
            if len >= 8 {
                raw[4..8].copy_from_slice(b"icpf");
            }
            assert!(
                frames(&raw).is_empty()
                    || frames(&raw)
                        .iter()
                        .all(|f| decode(&raw, f).is_ok() || decode(&raw, f).is_err())
            );
            assert!(parse_head(&raw, raw.len()).is_err() || len >= 28);
        }
    }
}
