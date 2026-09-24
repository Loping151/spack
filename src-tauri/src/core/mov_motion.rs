use super::{mov_cross, mov_exact, mov_motion_entropy};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const MAGIC: &[u8; 8] = b"SPMM\0\0\0\x01";
const HEADER: usize = 88;
const RAW_LIMIT: usize = 256 << 20;
const RECORD_LIMIT: usize = 512 << 20;
const COEFFICIENT_LIMIT: usize = 1 << 28;
const H: usize = 126;
const W: usize = 126;
const PIXELS: usize = 1008;
const PLANE: usize = H * W * 64;
const FRAME: usize = 3 * PLANE;
const MAX_FRAMES: usize = COEFFICIENT_LIMIT / FRAME;
const DESCRIPTOR: usize = 12;
const MAX_VECTOR: i16 = 18;
const TOTAL: u64 = 1_000_000;
type Quant = [[u8; 64]; 3];
struct MotionResidual {
    errors: Vec<i16>,
    modes: Vec<u8>,
    mx: Vec<i16>,
    my: Vec<i16>,
}
struct SearchFrame<'a> {
    coefficients: &'a [i16],
    previous: &'a [i16],
    pixels: &'a [i32],
    previous_pixels: &'a [i32],
    quant: &'a Quant,
    offsets: &'a [(i16, i16)],
}
struct SearchOutput<'a> {
    errors: [&'a mut [i16]; 3],
    modes: &'a mut [u8],
    mx: &'a mut [i16],
    my: &'a mut [i16],
}
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
fn allocate<T: Default + Clone>(n: usize) -> Result<Vec<T>, String> {
    let mut out = Vec::new();
    out.try_reserve_exact(n)
        .map_err(|_| "MOV motion allocation failed")?;
    out.resize(n, T::default());
    Ok(out)
}
fn be16(b: &[u8], at: usize) -> Result<usize, String> {
    Ok(u16::from_be_bytes(
        b.get(at..at + 2)
            .ok_or("Truncated ProRes header")?
            .try_into()
            .unwrap(),
    ) as usize)
}
fn be32(b: &[u8], at: usize) -> Result<usize, String> {
    Ok(u32::from_be_bytes(
        b.get(at..at + 4)
            .ok_or("Truncated MOV header")?
            .try_into()
            .unwrap(),
    ) as usize)
}
#[derive(Clone)]
struct Plane {
    offset: usize,
    len: usize,
    frame: usize,
    channel: usize,
    slice: usize,
}
fn geometry(slice: usize) -> (usize, usize, usize) {
    const X: [usize; 10] = [0, 8, 16, 24, 32, 40, 48, 56, 60, 62];
    const N: [usize; 10] = [8, 8, 8, 8, 8, 8, 8, 4, 2, 1];
    (X[slice % 10] * 2, (slice / 10) * 2, N[slice % 10] * 4)
}
fn coefficient_at(frame: usize, ch: usize, slice: usize, b: usize, k: usize) -> usize {
    let (mx, my, _) = geometry(slice);
    let within = b % 4;
    let (x, y) = if ch == 0 {
        (within % 2, within / 2)
    } else {
        (within / 2, within % 2)
    };
    frame * FRAME + ch * PLANE + ((my + y) * W + mx + (b / 4) * 2 + x) * 64 + k
}
fn parse_frame(
    raw: &[u8],
    start: usize,
    frame: usize,
    planes: &mut Vec<Plane>,
    cb: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<(usize, Quant), String> {
    let len = be32(raw, start)?;
    let end = start.checked_add(len).ok_or("ProRes length overflow")?;
    let f = raw.get(start..end).ok_or("Truncated ProRes frame")?;
    if len < 28
        || f.get(4..8) != Some(b"icpf")
        || be16(f, 16)? != 1000
        || be16(f, 18)? != 1000
        || f[20] >> 6 != 3
        || (f[20] >> 2) & 3 != 0
    {
        return Err("Unsupported MOV motion frame layout".into());
    }
    let picture = be16(f, 8)?.checked_add(8).ok_or("ProRes header overflow")?;
    if picture < 28 || picture >= len {
        return Err("Invalid ProRes frame header".into());
    }
    let mut q = [[4u8; 64]; 3];
    let mut at = 28;
    if f[27] & 2 != 0 {
        q[0].copy_from_slice(f.get(at..at + 64).ok_or("Truncated ProRes quantization")?);
        at += 64;
    }
    q[1] = q[0];
    if f[27] & 1 != 0 {
        q[1].copy_from_slice(f.get(at..at + 64).ok_or("Truncated ProRes quantization")?);
        at += 64;
    }
    q[2] = q[1];
    if at > picture || q.iter().flatten().any(|&x| x == 0) {
        return Err("Invalid ProRes quantization".into());
    }
    let ph = *f.get(picture).ok_or("Truncated ProRes picture")? as usize >> 3;
    let size = be32(f, picture + 1)?;
    let count = be16(f, picture + 5)?;
    if ph < 8
        || f.get(picture + 7) != Some(&0x30)
        || count != 630
        || size > len - picture
        || size < ph + count * 2
    {
        return Err("Unsupported ProRes motion slices".into());
    }
    let stop = picture + size;
    let mut pos = picture + ph + count * 2;
    for slice in 0..count {
        if slice % 64 == 0 {
            tick(cb, 0)?;
        }
        let n = be16(f, picture + ph + slice * 2)?;
        let sh = *f.get(pos).ok_or("Truncated ProRes slice")? as usize >> 3;
        if sh < 8 || n < sh || n > stop.saturating_sub(pos) || f.get(pos + 1) != Some(&1) {
            return Err("Unsupported ProRes motion qscale or slice".into());
        }
        let lengths = [be16(f, pos + 2)?, be16(f, pos + 4)?, be16(f, pos + 6)?];
        if sh + lengths.iter().sum::<usize>() > n {
            return Err("Invalid ProRes plane lengths".into());
        }
        let mut offset = start + pos + sh;
        for (channel, &length) in lengths.iter().enumerate() {
            if length > 0 {
                planes.push(Plane {
                    offset,
                    len: length,
                    frame,
                    channel,
                    slice,
                });
            }
            offset += length;
        }
        pos += n;
    }
    if pos != stop {
        return Err("Invalid ProRes picture suffix".into());
    }
    Ok((len, q))
}
fn scan(
    raw: &[u8],
    cb: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<(Vec<Quant>, Vec<Plane>), String> {
    if raw.len() > RAW_LIMIT {
        return Err("MOV motion source exceeds limit".into());
    }
    let mut quant = Vec::new();
    let mut planes = Vec::new();
    let mut atom = 0;
    while atom < raw.len() {
        tick(cb, 0)?;
        let mut size = be32(raw, atom)?;
        let kind = raw.get(atom + 4..atom + 8).ok_or("Truncated MOV atom")?;
        let mut header = 8;
        if size == 1 {
            size = usize::try_from(u64::from_be_bytes(
                raw.get(atom + 8..atom + 16)
                    .ok_or("Truncated MOV atom")?
                    .try_into()
                    .unwrap(),
            ))
            .map_err(|_| "MOV atom overflow")?;
            header = 16;
        }
        if size == 0 {
            size = raw.len() - atom;
        }
        if size < header || size > raw.len() - atom {
            return Err("Invalid MOV atom size".into());
        }
        let end = atom + size;
        if kind == b"mdat" {
            let mut p = atom + header;
            let mut next = p;
            while p + 8 <= end {
                if p >= next {
                    tick(cb, 0)?;
                    next = p + 16 * 1024;
                }
                if &raw[p + 4..p + 8] == b"icpf" {
                    if quant.len() >= MAX_FRAMES {
                        return Err("MOV motion coefficient limit".into());
                    }
                    let (n, q) = parse_frame(raw, p, quant.len(), &mut planes, cb)?;
                    if n > end - p {
                        return Err("ProRes frame crosses MOV atom".into());
                    }
                    quant.push(q);
                    p += n;
                } else {
                    p += 1;
                }
            }
        }
        atom = end;
    }
    if quant.is_empty() || planes.is_empty() {
        return Err("No supported MOV motion frames".into());
    }
    Ok((quant, planes))
}

fn idct(c: &[i16], q: &[u8; 64]) -> [i32; 64] {
    let mut natural = [0i64; 64];
    let mut ac = false;
    for k in 0..64 {
        natural[SCAN[k]] = c[k] as i64 * q[SCAN[k]] as i64;
        ac |= k > 0 && c[k] != 0;
    }
    if !ac {
        let dc = ((natural[0] * T[0][0] * T[0][0] + (1 << 22)) >> 23) as i32;
        return [dc; 64];
    }
    let mut temp = [0i64; 64];
    for y in 0..8 {
        for x in 0..8 {
            temp[y * 8 + x] = (0..8).map(|k| T[y][k] * natural[k * 8 + x]).sum();
        }
    }
    let mut out = [0i32; 64];
    for y in 0..8 {
        for x in 0..8 {
            let value: i64 = (0..8).map(|k| temp[y * 8 + k] * T[x][k]).sum();
            out[y * 8 + x] = ((value + (1 << 22)) >> 23) as i32;
        }
    }
    out
}
fn dct(p: &[i32; 64], q: &[u8; 64]) -> [i16; 64] {
    let mut temp = [0i64; 64];
    for u in 0..8 {
        for x in 0..8 {
            temp[u * 8 + x] = (0..8).map(|y| T[y][u] * p[y * 8 + x] as i64).sum();
        }
    }
    let mut natural = [0i16; 64];
    for u in 0..8 {
        for v in 0..8 {
            let value: i64 = (0..8).map(|x| temp[u * 8 + x] * T[x][v]).sum();
            let den = (q[u * 8 + v] as i64) << 33;
            natural[u * 8 + v] = (value + den / 2)
                .div_euclid(den)
                .clamp(i16::MIN as i64, i16::MAX as i64) as i16;
        }
    }
    let mut out = [0i16; 64];
    for k in 0..64 {
        out[k] = natural[SCAN[k]];
    }
    out
}
fn render(c: &[i16], q: &Quant) -> Result<Vec<i32>, String> {
    let mut pixels = allocate::<i32>(FRAME)?;
    for (ch, channel_quant) in q.iter().enumerate() {
        for by in 0..H {
            for bx in 0..W {
                let at = ch * PLANE + (by * W + bx) * 64;
                let b = idct(&c[at..at + 64], channel_quant);
                for y in 0..8 {
                    let p = ch * PLANE + (by * 8 + y) * PIXELS + bx * 8;
                    pixels[p..p + 8].copy_from_slice(&b[y * 8..y * 8 + 8]);
                }
            }
        }
    }
    Ok(pixels)
}
fn moved_block(p: &[i32], ch: usize, by: usize, bx: usize, dy: i16, dx: i16) -> [i32; 64] {
    let sy = by as i32 * 8 + (dy as i32).div_euclid(4);
    let sx = bx as i32 * 8 + (dx as i32).div_euclid(4);
    let fy = (dy as i32).rem_euclid(4) as i64;
    let fx = (dx as i32).rem_euclid(4) as i64;
    let mut out = [0i32; 64];
    for y in 0..8 {
        let y0 = (sy + y as i32).clamp(0, PIXELS as i32 - 1) as usize;
        let y1 = (sy + y as i32 + 1).clamp(0, PIXELS as i32 - 1) as usize;
        for x in 0..8 {
            let x0 = (sx + x as i32).clamp(0, PIXELS as i32 - 1) as usize;
            let x1 = (sx + x as i32 + 1).clamp(0, PIXELS as i32 - 1) as usize;
            let at = |y, x| p[ch * PLANE + y * PIXELS + x] as i64;
            out[y * 8 + x] = (((4 - fy) * ((4 - fx) * at(y0, x0) + fx * at(y0, x1))
                + fy * ((4 - fx) * at(y1, x0) + fx * at(y1, x1))
                + 8)
                >> 4) as i32;
        }
    }
    out
}
fn cost(value: i32) -> u64 {
    let n = 1 + value.unsigned_abs();
    let bits = 31 - n.leading_zeros();
    let base = 1u32 << bits;
    (bits as u64) * 256 + ((n - base) as u64) * 256 / base as u64
}
fn block_cost(
    cur: &[i16],
    previous: Option<&[i16]>,
    pred: Option<&[[i16; 64]; 3]>,
    by: usize,
    bx: usize,
) -> Option<u64> {
    let mut total = 0;
    for ch in 0..3 {
        let at = ch * PLANE + (by * W + bx) * 64;
        for k in 0..64 {
            let p = if let Some(p) = pred {
                p[ch][k] as i32
            } else if let Some(p) = previous {
                p[at + k] as i32
            } else {
                0
            };
            let d = cur[at + k] as i32 - p;
            if i16::try_from(d).is_err() {
                return None;
            }
            total += cost(d);
        }
    }
    Some(total)
}
fn sad(
    current: &[i32],
    previous: &[i32],
    by: usize,
    bx: usize,
    dy: i16,
    dx: i16,
    limit: u64,
) -> u64 {
    let mut score = 0u64;
    if dy % 4 == 0 && dx % 4 == 0 {
        let sy = by as i32 * 8 + dy as i32 / 4;
        let sx = bx as i32 * 8 + dx as i32 / 4;
        let interior = sy >= 0 && sx >= 0 && sy + 7 < PIXELS as i32 && sx + 7 < PIXELS as i32;
        for ch in 0..3 {
            for y in 0..8 {
                let at = ch * PLANE + (by * 8 + y) * PIXELS + bx * 8;
                if interior {
                    let prev = ch * PLANE + (sy as usize + y) * PIXELS + sx as usize;
                    for x in 0..8 {
                        score +=
                            (current[at + x] as i64 - previous[prev + x] as i64).unsigned_abs();
                    }
                } else {
                    let py = (sy + y as i32).clamp(0, PIXELS as i32 - 1) as usize;
                    for x in 0..8 {
                        let px = (sx + x as i32).clamp(0, PIXELS as i32 - 1) as usize;
                        score += (current[at + x] as i64
                            - previous[ch * PLANE + py * PIXELS + px] as i64)
                            .unsigned_abs();
                    }
                }
                if score >= limit {
                    return score;
                }
            }
        }
        return score;
    }
    for ch in 0..3 {
        let block = moved_block(previous, ch, by, bx, dy, dx);
        for y in 0..8 {
            let at = ch * PLANE + (by * 8 + y) * PIXELS + bx * 8;
            for x in 0..8 {
                score += (current[at + x] as i64 - block[y * 8 + x] as i64).unsigned_abs();
            }
            if score >= limit {
                return score;
            }
        }
    }
    score
}
fn search_rows(
    frame: &SearchFrame<'_>,
    first_row: usize,
    output: SearchOutput<'_>,
    cancelled: &AtomicBool,
    completed: &AtomicUsize,
) -> Result<(), String> {
    let SearchFrame {
        coefficients: cur,
        previous,
        pixels,
        previous_pixels,
        quant,
        offsets,
    } = *frame;
    for row in 0..output.modes.len() / W {
        let by = first_row + row;
        for bx in 0..W {
            let at = row * W + bx;
            if cancelled.load(Ordering::Relaxed) {
                return Err("error.cancelled".into());
            }
            let mut mode = 0;
            let mut score = block_cost(cur, None, None, by, bx).unwrap();
            if let Some(temporal) = block_cost(cur, Some(previous), None, by, bx) {
                if temporal < score {
                    mode = 1;
                    score = temporal;
                }
            }
            let mut chosen = [[0i16; 64]; 3];
            let (mut dy, mut dx) = (0i16, 0i16);
            if score > 0 {
                let mut vector = (0i16, 0i16);
                let mut best_sad = sad(pixels, previous_pixels, by, bx, 0, 0, u64::MAX);
                for &(y, x) in offsets {
                    if (y, x) == (0, 0) {
                        continue;
                    }
                    let s = sad(pixels, previous_pixels, by, bx, y, x, best_sad);
                    if s < best_sad {
                        best_sad = s;
                        vector = (y, x);
                    }
                }
                let integer_vector = vector;
                for y in -2i16..=2 {
                    for x in -2i16..=2 {
                        if (y, x) == (0, 0) {
                            continue;
                        }
                        let v = (integer_vector.0 + y, integer_vector.1 + x);
                        let s = sad(pixels, previous_pixels, by, bx, v.0, v.1, best_sad);
                        if s < best_sad {
                            best_sad = s;
                            vector = v;
                        }
                    }
                }
                for v in [integer_vector, vector] {
                    let mut pred = [[0i16; 64]; 3];
                    for ch in 0..3 {
                        pred[ch] = dct(
                            &moved_block(previous_pixels, ch, by, bx, v.0, v.1),
                            &quant[ch],
                        );
                    }
                    if let Some(c) = block_cost(cur, None, Some(&pred), by, bx) {
                        let c = c + 9 * 256;
                        if c < score {
                            score = c;
                            mode = 2;
                            dy = v.0;
                            dx = v.1;
                            chosen = pred;
                        }
                    }
                }
            }
            output.modes[at] = mode;
            if mode == 2 {
                output.mx[at] = dx;
                output.my[at] = dy;
            }
            for (ch, chosen_channel) in chosen.iter().enumerate() {
                let p = ch * PLANE + (by * W + bx) * 64;
                for k in 0..64 {
                    let pred = if mode == 1 {
                        previous[p + k]
                    } else if mode == 2 {
                        chosen_channel[k]
                    } else {
                        0
                    };
                    output.errors[ch][(row * W + bx) * 64 + k] =
                        i16::try_from(cur[p + k] as i32 - pred as i32)
                            .map_err(|_| "MOV motion residual overflow")?;
                }
            }
        }

        completed.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

fn search_frame(
    frame: &SearchFrame<'_>,
    output: SearchOutput<'_>,
    worker_count: usize,
    progress: &mut dyn FnMut(usize) -> bool,
) -> Result<(), String> {
    let cancelled = AtomicBool::new(false);
    let completed = AtomicUsize::new(0);
    let chunk_rows = H.div_ceil(worker_count.clamp(1, 4));
    let SearchOutput {
        errors: [y, u, v],
        modes,
        mx,
        my,
    } = output;
    let error_chunks = y
        .chunks_mut(chunk_rows * W * 64)
        .zip(u.chunks_mut(chunk_rows * W * 64))
        .zip(v.chunks_mut(chunk_rows * W * 64));
    let mode_chunks = modes
        .chunks_mut(chunk_rows * W)
        .zip(mx.chunks_mut(chunk_rows * W))
        .zip(my.chunks_mut(chunk_rows * W));
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for (index, (((y, u), v), ((modes, mx), my))) in error_chunks.zip(mode_chunks).enumerate() {
            let cancelled = &cancelled;
            let completed = &completed;
            workers.push(scope.spawn(move || {
                let result = search_rows(
                    frame,
                    index * chunk_rows,
                    SearchOutput {
                        errors: [y, u, v],
                        modes,
                        mx,
                        my,
                    },
                    cancelled,
                    completed,
                );
                if result.is_err() {
                    cancelled.store(true, Ordering::Relaxed);
                }
                result
            }));
        }
        let mut failure = None;
        while workers.iter().any(|worker| !worker.is_finished()) {
            if !cancelled.load(Ordering::Relaxed) && !progress(completed.load(Ordering::Relaxed)) {
                failure = Some("error.cancelled".to_string());
                cancelled.store(true, Ordering::Relaxed);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        for worker in workers {
            let result = worker
                .join()
                .unwrap_or_else(|_| Err("MOV motion search worker failed".into()));
            if let Err(error) = result {
                if failure.is_none() || error != "error.cancelled" {
                    failure = Some(error);
                }
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        if !progress(completed.load(Ordering::Relaxed)) {
            return Err("error.cancelled".into());
        }
        Ok(())
    })
}

fn encode_motion(
    coeff: &[i16],
    q: &[Quant],
    cb: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<MotionResidual, String> {
    let frames = q.len();
    let worker_count = std::thread::available_parallelism().map_or(1, |n| n.get().min(4));
    let mut errors = allocate::<i16>(coeff.len())?;
    let mut modes = allocate::<u8>(frames * H * W)?;
    let mut mx = allocate::<i16>(modes.len())?;
    let mut my = mx.clone();
    let mut previous_pixels = Vec::new();
    let mut offsets: Vec<_> = (-4i16..=4)
        .flat_map(|y| (-4i16..=4).map(move |x| (y * 4, x * 4)))
        .collect();
    offsets.sort_by_key(|&(y, x)| (y.abs() + x.abs(), y, x));
    for f in 0..frames {
        tick(cb, 200_000 + f as u64 * 300_000 / frames as u64)?;
        let cur = &coeff[f * FRAME..(f + 1) * FRAME];
        let pixels = render(cur, &q[f])?;
        if f == 0 {
            errors[..FRAME].copy_from_slice(cur);
            previous_pixels = pixels;
            continue;
        }
        let previous = &coeff[(f - 1) * FRAME..f * FRAME];
        let frame = SearchFrame {
            coefficients: cur,
            previous,
            pixels: &pixels,
            previous_pixels: &previous_pixels,
            quant: &q[f],
            offsets: &offsets,
        };
        let (y, uv) = errors[f * FRAME..(f + 1) * FRAME].split_at_mut(PLANE);
        let (u, v) = uv.split_at_mut(PLANE);
        let mode_range = f * H * W..(f + 1) * H * W;
        search_frame(
            &frame,
            SearchOutput {
                errors: [y, u, v],
                modes: &mut modes[mode_range.clone()],
                mx: &mut mx[mode_range.clone()],
                my: &mut my[mode_range],
            },
            worker_count,
            &mut |rows| {
                cb(
                    200_000 + ((f * H + rows) as u64) * 300_000 / (frames * H) as u64,
                    TOTAL,
                )
            },
        )?;
        previous_pixels = pixels;
    }
    Ok(MotionResidual {
        errors,
        modes,
        mx,
        my,
    })
}
fn decode_motion(
    errors: &[i16],
    q: &[Quant],
    modes: &[u8],
    mx: &[i16],
    my: &[i16],
    cb: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Vec<i16>, String> {
    let mut out = allocate::<i16>(errors.len())?;
    let mut previous_pixels = Vec::new();
    for (f, frame_quant) in q.iter().enumerate() {
        for by in 0..H {
            tick(
                cb,
                300_000 + ((f * H + by) as u64) * 450_000 / (q.len() * H) as u64,
            )?;
            for bx in 0..W {
                let mi = f * H * W + by * W + bx;
                let mode = modes[mi];
                for (ch, channel_quant) in frame_quant.iter().enumerate() {
                    let at = f * FRAME + ch * PLANE + (by * W + bx) * 64;
                    let pred = if mode == 2 {
                        dct(
                            &moved_block(&previous_pixels, ch, by, bx, my[mi], mx[mi]),
                            channel_quant,
                        )
                    } else {
                        [0i16; 64]
                    };
                    for k in 0..64 {
                        let p = if mode == 1 {
                            out[at - FRAME + k]
                        } else {
                            pred[k]
                        };
                        out[at + k] = i16::try_from(errors[at + k] as i32 + p as i32)
                            .map_err(|_| "MOV motion coefficient overflow")?;
                    }
                }
            }
        }
        if f + 1 < q.len() {
            previous_pixels = render(&out[f * FRAME..(f + 1) * FRAME], &q[f])?;
        }
    }
    Ok(out)
}

pub fn encode_with_progress(
    raw: &[u8],
    cb: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Vec<u8>, String> {
    tick(cb, 0)?;
    let (quant, planes) = scan(raw, cb)?;
    let mut coefficients = allocate::<i16>(quant.len() * FRAME)?;
    for (i, p) in planes.iter().enumerate() {
        if i % 128 == 0 {
            tick(cb, 20_000 + i as u64 * 180_000 / planes.len() as u64)?;
        }
        let (_, _, blocks) = geometry(p.slice);
        let bytes = &raw[p.offset..p.offset + p.len];
        let values = mov_exact::unpack_plane(bytes, blocks)?;
        if mov_exact::pack_plane(&values, blocks, p.len)? != bytes {
            return Err("Noncanonical ProRes motion plane".into());
        }
        for b in 0..blocks {
            for k in 0..64 {
                coefficients[coefficient_at(p.frame, p.channel, p.slice, b, k)] =
                    values[k * blocks + b];
            }
        }
    }
    let MotionResidual {
        errors,
        modes,
        mx,
        my,
    } = encode_motion(&coefficients, &quant, cb)?;
    drop(coefficients);
    let shape = [quant.len(), H, W];
    let (cross, cross_side) =
        mov_cross::encode(&errors, shape, &quant, &mut || !cb(500_000, TOTAL))?;
    drop(errors);
    let range = mov_motion_entropy::encode(&cross, shape, &modes, &mut || !cb(600_000, TOTAL))?;
    drop(cross);
    let occupied: usize = planes.iter().map(|p| p.len).sum();
    let side_len =
        quant.len() * 192 + planes.len() * DESCRIPTOR + modes.len() * 5 + raw.len() - occupied;
    if side_len > RECORD_LIMIT {
        return Err("MOV motion side exceeds limit".into());
    }
    let mut side = Vec::new();
    side.try_reserve_exact(side_len)
        .map_err(|_| "MOV motion side allocation failed")?;
    for frame in &quant {
        for ch in frame {
            side.extend_from_slice(ch);
        }
    }
    for p in &planes {
        side.extend_from_slice(&(p.offset as u32).to_le_bytes());
        side.extend_from_slice(&(p.len as u16).to_le_bytes());
        side.extend_from_slice(&(p.frame as u16).to_le_bytes());
        side.extend_from_slice(&(p.slice as u16).to_le_bytes());
        side.push(p.channel as u8);
        side.push(0);
    }
    side.extend_from_slice(&modes);
    for v in &mx {
        side.extend_from_slice(&v.to_le_bytes());
    }
    for v in &my {
        side.extend_from_slice(&v.to_le_bytes());
    }
    let mut end = 0;
    for p in &planes {
        side.extend_from_slice(&raw[end..p.offset]);
        end = p.offset + p.len;
    }
    side.extend_from_slice(&raw[end..]);
    tick(cb, 680_000)?;
    let compressed_side = zstd::bulk::compress(&side, 12).map_err(|e| e.to_string())?;
    drop(side);
    let len = HEADER + range.len() + cross_side.len() + compressed_side.len();
    if len > RECORD_LIMIT {
        return Err("MOV motion record exceeds limit".into());
    }
    let mut out = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|_| "MOV motion record allocation failed")?;
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(raw.len() as u64).to_le_bytes());
    out.extend_from_slice(&(quant.len() as u32).to_le_bytes());
    out.extend_from_slice(&(planes.len() as u32).to_le_bytes());
    for n in [
        range.len(),
        cross_side.len(),
        compressed_side.len(),
        side_len,
    ] {
        out.extend_from_slice(&(n as u64).to_le_bytes());
    }
    out.extend_from_slice(blake3::hash(raw).as_bytes());
    out.extend_from_slice(&range);
    out.extend_from_slice(&cross_side);
    out.extend_from_slice(&compressed_side);
    let restored = decode_with_progress(&out, &mut |d, t| {
        cb(700_000 + d * 300_000 / t.max(1), TOTAL)
    })?;
    if restored != raw {
        return Err("MOV motion byte-exact verification failed".into());
    }
    tick(cb, TOTAL)?;
    Ok(out)
}
pub fn decode_with_cancel(encoded: &[u8], cancelled: &dyn Fn() -> bool) -> Result<Vec<u8>, String> {
    decode_with_progress(encoded, &mut |_, _| !cancelled())
}
fn decode_side(
    encoded: &[u8],
    expected: usize,
    cb: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut decoder = zstd::stream::read::Decoder::with_buffer(encoded)
        .map_err(|e| e.to_string())?
        .single_frame();
    decoder.window_log_max(27).map_err(|e| e.to_string())?;
    let mut side = Vec::new();
    side.try_reserve_exact(expected)
        .map_err(|_| "MOV motion side allocation failed")?;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        tick(cb, 40_000)?;
        let n = decoder.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        if n > expected - side.len() {
            return Err("MOV motion side length mismatch".into());
        }
        side.extend_from_slice(&chunk[..n]);
    }
    if side.len() != expected {
        return Err("MOV motion side length mismatch".into());
    }
    if !decoder.finish().is_empty() {
        return Err("Trailing MOV motion side bytes".into());
    }
    Ok(side)
}
pub fn decode_with_progress(
    encoded: &[u8],
    cb: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Vec<u8>, String> {
    tick(cb, 0)?;
    if encoded.len() < HEADER || encoded.len() > RECORD_LIMIT || &encoded[..8] != MAGIC {
        return Err("Invalid MOV motion header".into());
    }
    let word = |at| {
        usize::try_from(u64::from_le_bytes(encoded[at..at + 8].try_into().unwrap()))
            .map_err(|_| "MOV motion length overflow")
    };
    let raw_len = word(8)?;
    let frames = u32::from_le_bytes(encoded[16..20].try_into().unwrap()) as usize;
    let count = u32::from_le_bytes(encoded[20..24].try_into().unwrap()) as usize;
    let range_len = word(24)?;
    let cross_len = word(32)?;
    let side_len = word(40)?;
    let decoded_side_len = word(48)?;
    if raw_len > RAW_LIMIT
        || frames == 0
        || frames > MAX_FRAMES
        || count == 0
        || count > frames * 630 * 3
        || range_len < 5
        || cross_len > RECORD_LIMIT
        || side_len > RECORD_LIMIT
        || decoded_side_len > RECORD_LIMIT
    {
        return Err("Invalid MOV motion limits".into());
    }
    let range_end = HEADER
        .checked_add(range_len)
        .ok_or("MOV motion length overflow")?;
    let cross_end = range_end
        .checked_add(cross_len)
        .ok_or("MOV motion length overflow")?;
    if cross_end.checked_add(side_len) != Some(encoded.len()) {
        return Err("MOV motion payload length mismatch".into());
    }
    let blocks = frames * H * W;
    let prefix = frames * 192 + count * DESCRIPTOR + blocks * 5;
    if decoded_side_len < prefix || decoded_side_len - prefix > raw_len {
        return Err("Invalid MOV motion side length".into());
    }
    let side = decode_side(&encoded[cross_end..], decoded_side_len, cb)?;
    tick(cb, 50_000)?;
    let mut quant = vec![[[0u8; 64]; 3]; frames];
    let mut at = 0;
    for frame in &mut quant {
        for q in frame {
            q.copy_from_slice(&side[at..at + 64]);
            at += 64;
            if q.contains(&0) {
                return Err("Invalid MOV motion quantization".into());
            }
        }
    }
    let mut planes = Vec::with_capacity(count);
    let mut end = 0;
    let mut occupied = 0usize;
    let mut seen = std::collections::HashSet::new();
    for i in 0..count {
        if i % 128 == 0 {
            tick(cb, 60_000)?;
        }
        let d = &side[at..at + DESCRIPTOR];
        at += DESCRIPTOR;
        let p = Plane {
            offset: u32::from_le_bytes(d[..4].try_into().unwrap()) as usize,
            len: u16::from_le_bytes(d[4..6].try_into().unwrap()) as usize,
            frame: u16::from_le_bytes(d[6..8].try_into().unwrap()) as usize,
            slice: u16::from_le_bytes(d[8..10].try_into().unwrap()) as usize,
            channel: d[10] as usize,
        };
        if p.frame >= frames
            || p.channel >= 3
            || p.slice >= 630
            || d[11] != 0
            || p.len == 0
            || p.offset < end
            || p.offset > raw_len
            || p.len > raw_len - p.offset
            || !seen.insert((p.frame, p.channel, p.slice))
        {
            return Err("Invalid MOV motion descriptor".into());
        }
        end = p.offset + p.len;
        occupied += p.len;
        planes.push(p);
    }
    if occupied > raw_len || raw_len - occupied != side.len() - prefix {
        return Err("MOV motion skeleton length mismatch".into());
    }
    let modes = &side[at..at + blocks];
    at += blocks;
    let mut mx = allocate::<i16>(blocks)?;
    let mut my = allocate::<i16>(blocks)?;
    for target in [&mut mx, &mut my] {
        for value in target.iter_mut() {
            *value = i16::from_le_bytes(side[at..at + 2].try_into().unwrap());
            at += 2;
        }
    }
    for i in 0..blocks {
        if modes[i] > 2
            || (i < H * W && modes[i] != 0)
            || mx[i].unsigned_abs() > MAX_VECTOR as u16
            || my[i].unsigned_abs() > MAX_VECTOR as u16
            || (modes[i] != 2 && (mx[i] != 0 || my[i] != 0))
        {
            return Err("Invalid MOV motion prediction mode or vector".into());
        }
    }
    let shape = [frames, H, W];
    let cross = mov_motion_entropy::decode(&encoded[HEADER..range_end], shape, modes, &mut || {
        !cb(100_000, TOTAL)
    })?;
    let errors = mov_cross::decode(
        cross,
        &encoded[range_end..cross_end],
        shape,
        &quant,
        &mut || !cb(200_000, TOTAL),
    )?;
    let coefficients = decode_motion(&errors, &quant, modes, &mx, &my, cb)?;
    drop(errors);
    let mut out = allocate::<u8>(raw_len)?;
    end = 0;
    for (i, p) in planes.iter().enumerate() {
        if i % 128 == 0 {
            tick(cb, 750_000 + i as u64 * 240_000 / count as u64)?;
        }
        let n = p.offset - end;
        out[end..p.offset].copy_from_slice(&side[at..at + n]);
        at += n;
        let (_, _, blocks) = geometry(p.slice);
        let mut values = vec![0i16; blocks * 64];
        for b in 0..blocks {
            for k in 0..64 {
                values[k * blocks + b] =
                    coefficients[coefficient_at(p.frame, p.channel, p.slice, b, k)];
            }
        }
        let plane = mov_exact::pack_plane(&values, blocks, p.len)?;
        out[p.offset..p.offset + p.len].copy_from_slice(&plane);
        end = p.offset + p.len;
    }
    out[end..].copy_from_slice(&side[at..]);
    if blake3::hash(&out).as_bytes() != &encoded[56..88] {
        return Err("MOV motion original BLAKE3 mismatch".into());
    }
    tick(cb, TOTAL)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_movie() -> Vec<u8> {
        let mut slices = Vec::new();
        for slice in 0..630 {
            let (_, _, blocks) = geometry(slice);
            let planes: Vec<Vec<u8>> = (0..3)
                .map(|ch| {
                    let mut values = vec![0i16; blocks * 64];
                    for (i, dc) in values[..blocks].iter_mut().enumerate() {
                        *dc = ((i * 19 + ch * 7) % 43) as i16 - 21;
                    }
                    values[blocks + ch] = 3 - ch as i16;
                    let mut bytes = mov_exact::pack_plane(&values, blocks, 4096).unwrap();
                    let used = bytes.iter().rposition(|&x| x != 0).unwrap() + 1;
                    bytes.truncate(used);
                    bytes
                })
                .collect();
            let mut bytes = vec![0x40, 1];
            for plane in &planes {
                bytes.extend_from_slice(&(plane.len() as u16).to_be_bytes());
            }
            for plane in planes {
                bytes.extend_from_slice(&plane);
            }
            slices.push(bytes);
        }
        let picture_len = 8 + 630 * 2 + slices.iter().map(Vec::len).sum::<usize>();
        let mut frame = vec![0u8; 92];
        frame[..4].copy_from_slice(&((92 + picture_len) as u32).to_be_bytes());
        frame[4..8].copy_from_slice(b"icpf");
        frame[8..10].copy_from_slice(&84u16.to_be_bytes());
        frame[16..18].copy_from_slice(&1000u16.to_be_bytes());
        frame[18..20].copy_from_slice(&1000u16.to_be_bytes());
        frame[20] = 0xc0;
        frame[27] = 2;
        frame[28..92].fill(8);
        frame.push(0x40);
        frame.extend_from_slice(&(picture_len as u32).to_be_bytes());
        frame.extend_from_slice(&630u16.to_be_bytes());
        frame.push(0x30);
        for slice in &slices {
            frame.extend_from_slice(&(slice.len() as u16).to_be_bytes());
        }
        for slice in slices {
            frame.extend_from_slice(&slice);
        }
        let mut raw = ((frame.len() * 2 + 8) as u32).to_be_bytes().to_vec();
        raw.extend_from_slice(b"mdat");
        raw.extend_from_slice(&frame);
        raw.extend_from_slice(&frame);
        raw
    }

    #[test]
    fn complete_movie_round_trip_and_scan_cancel() {
        let raw = synthetic_movie();
        let (quant, _) = scan(&raw, &mut |_, _| true).unwrap();
        assert_eq!(quant, vec![[[8u8; 64]; 3]; 2]);
        let mut calls = 0;
        assert_eq!(
            encode_with_progress(&raw, &mut |_, _| {
                calls += 1;
                calls != 5
            })
            .unwrap_err(),
            "error.cancelled"
        );
        assert_eq!(calls, 5);
        let encoded = encode_with_progress(&raw, &mut |_, _| true).unwrap();
        assert_eq!(decode_with_cancel(&encoded, &|| false).unwrap(), raw);
        assert_eq!(
            decode_with_cancel(&encoded, &|| true).unwrap_err(),
            "error.cancelled"
        );
    }

    #[test]
    fn side_requires_exact_length_and_single_zstd_frame() {
        let compressed = zstd::bulk::compress(b"side", 1).unwrap();
        assert_eq!(
            decode_side(&compressed, 4, &mut |_, _| true).unwrap(),
            b"side"
        );
        for expected in [3, 5] {
            assert!(decode_side(&compressed, expected, &mut |_, _| true).is_err());
        }
        let tails = [
            zstd::bulk::compress(b"", 1).unwrap(),
            vec![0x50, 0x2a, 0x4d, 0x18, 0, 0, 0, 0],
            vec![0],
        ];
        for tail in tails {
            let mut invalid = compressed.clone();
            invalid.extend_from_slice(&tail);
            assert_eq!(
                decode_side(&invalid, 4, &mut |_, _| true).unwrap_err(),
                "Trailing MOV motion side bytes"
            );
        }
    }

    #[test]
    fn integer_sad_matches_bilinear_path_at_edges_and_limits() {
        let current: Vec<i32> = (0..FRAME)
            .map(|i| ((i * 31 % 131071) as i32 - 65535) * 256)
            .collect();
        let previous: Vec<i32> = (0..FRAME)
            .map(|i| ((i * 13 % 65537) as i32 - 32768) * 256)
            .collect();
        for (by, bx) in [
            (0, 0),
            (0, W - 1),
            (H - 1, 0),
            (H - 1, W - 1),
            (H / 2, W / 2),
        ] {
            for dy in -4..=4 {
                for dx in -4..=4 {
                    for limit in [0, 1, 100_000_000, u64::MAX] {
                        let mut expected = 0;
                        'channels: for ch in 0..3 {
                            let block = moved_block(&previous, ch, by, bx, dy * 4, dx * 4);
                            for y in 0..8 {
                                let at = ch * PLANE + (by * 8 + y) * PIXELS + bx * 8;
                                for x in 0..8 {
                                    expected += (current[at + x] as i64 - block[y * 8 + x] as i64)
                                        .unsigned_abs();
                                }
                                if expected >= limit {
                                    break 'channels;
                                }
                            }
                        }
                        assert_eq!(
                            sad(&current, &previous, by, bx, dy * 4, dx * 4, limit),
                            expected
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn parallel_rows_are_deterministic_and_cancel_on_caller_thread() {
        let quant = [[8u8; 64]; 3];
        let mut previous = vec![0i16; FRAME];
        for ch in 0..3 {
            for by in 0..H {
                for bx in 0..W {
                    previous[ch * PLANE + (by * W + bx) * 64] =
                        (((by * 17 + bx * 23 + ch * 7) % 101) * 64) as i16;
                }
            }
        }
        let previous_pixels = render(&previous, &quant).unwrap();
        let mut coefficients = previous.clone();
        for by in [0, 31, 32, 63, 64, 95, 96, 125] {
            for bx in [0, 50, 125] {
                for (ch, q) in quant.iter().enumerate() {
                    let predicted = dct(&moved_block(&previous_pixels, ch, by, bx, 1, -1), q);
                    let at = ch * PLANE + (by * W + bx) * 64;
                    coefficients[at..at + 64].copy_from_slice(&predicted);
                }
            }
        }
        let pixels = render(&coefficients, &quant).unwrap();
        let mut offsets: Vec<_> = (-4i16..=4)
            .flat_map(|y| (-4i16..=4).map(move |x| (y * 4, x * 4)))
            .collect();
        offsets.sort_by_key(|&(y, x)| (y.abs() + x.abs(), y, x));
        let frame = SearchFrame {
            coefficients: &coefficients,
            previous: &previous,
            pixels: &pixels,
            previous_pixels: &previous_pixels,
            quant: &quant,
            offsets: &offsets,
        };
        let caller = std::thread::current().id();
        let run = |workers, cancel| {
            let mut output = MotionResidual {
                errors: vec![0; FRAME],
                modes: vec![0; H * W],
                mx: vec![0; H * W],
                my: vec![0; H * W],
            };
            let (y, uv) = output.errors.split_at_mut(PLANE);
            let (u, v) = uv.split_at_mut(PLANE);
            let mut callbacks = 0;
            let result = search_frame(
                &frame,
                SearchOutput {
                    errors: [y, u, v],
                    modes: &mut output.modes,
                    mx: &mut output.mx,
                    my: &mut output.my,
                },
                workers,
                &mut |_| {
                    assert_eq!(std::thread::current().id(), caller);
                    callbacks += 1;
                    !(cancel && callbacks == 1)
                },
            );
            (result, output, callbacks)
        };
        let (single_result, single, _) = run(1, false);
        let (parallel_result, parallel, _) = run(4, false);
        single_result.unwrap();
        parallel_result.unwrap();
        assert!(single.modes.contains(&2));
        assert_eq!(single.errors, parallel.errors);
        assert_eq!(single.modes, parallel.modes);
        assert_eq!(single.mx, parallel.mx);
        assert_eq!(single.my, parallel.my);
        for workers in [1, 4] {
            let (result, _, callbacks) = run(workers, true);
            assert_eq!(result.unwrap_err(), "error.cancelled");
            assert_eq!(callbacks, 1);
        }
    }
    #[test]
    fn fixed_transform_rounding_and_extremes() {
        for value in [i16::MIN, -3584, -1, 0, 1, 3426, i16::MAX] {
            let mut c = [0i16; 64];
            c[0] = value;
            let q = [255u8; 64];
            let pixels = idct(&c, &q);
            assert!(pixels.iter().all(|&v| v == pixels[0]));
            let recovered = dct(&pixels, &q);
            assert!((recovered[0] as i32 - value as i32).abs() <= 2);
        }
        for sign in [-1i16, 1] {
            let c = [sign * 32767; 64];
            let q = [255; 64];
            let pixels = idct(&c, &q);
            let _ = dct(&pixels, &q);
        }
    }
    #[test]
    fn negative_quarter_vectors_use_floor_coordinates() {
        let mut pixels = vec![0i32; FRAME];
        for y in 0..PIXELS {
            for x in 0..PIXELS {
                pixels[y * PIXELS + x] = (x * 256) as i32;
            }
        }
        let block = moved_block(&pixels, 0, 0, 1, 0, -1);
        assert_eq!(block[0], 7 * 256 + 192);
        let edge = moved_block(&pixels, 0, 0, 0, -1, -1);
        assert_eq!(edge[0], 0);
    }
    #[test]
    fn unsupported_and_cancelled_inputs_are_errors() {
        assert!(encode_with_progress(b"unsupported", &mut |_, _| true).is_err());
        assert_eq!(
            encode_with_progress(b"unsupported", &mut |_, _| false).unwrap_err(),
            "error.cancelled"
        );
        assert!(decode_with_progress(b"bad", &mut |_, _| true).is_err());
    }
    #[test]
    fn short_entropy_rejected_before_allocations() {
        for n in 0u64..5 {
            let mut b = vec![0u8; HEADER];
            b[..8].copy_from_slice(MAGIC);
            b[8..16].copy_from_slice(&(RAW_LIMIT as u64).to_le_bytes());
            b[16..20].copy_from_slice(&1u32.to_le_bytes());
            b[20..24].copy_from_slice(&1u32.to_le_bytes());
            b[24..32].copy_from_slice(&n.to_le_bytes());
            assert_eq!(
                decode_with_progress(&b, &mut |_, _| true).unwrap_err(),
                "Invalid MOV motion limits"
            );
        }
    }
}
