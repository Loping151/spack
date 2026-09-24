use super::prores_bits::{self as pr, Decoded, Frame};
use super::source_codec::{self as sc, band_rows, par_each, Planes, BANDS};
use super::source_rc::*;

const MAGIC: &[u8; 8] = b"SPMS\0\0\0\x02";
const MAX_FRAMES: usize = 1 << 20;
const MAX_OUTPUT: usize = 512 << 20;
const EXTRA_REFS: usize = 2;

thread_local! {
    static THREAD_BUDGET: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
pub(crate) fn with_thread_budget<R>(threads: usize, f: impl FnOnce() -> R) -> R {
    THREAD_BUDGET.with(|b| b.set(threads));
    let r = f();
    THREAD_BUDGET.with(|b| b.set(0));
    r
}
pub(crate) fn thread_budget() -> usize {
    let budget = THREAD_BUDGET.with(|b| b.get());
    if budget > 0 {
        budget
    } else {
        std::thread::available_parallelism()
            .map_or(4, |x| x.get())
            .clamp(1, 32)
    }
}

pub(crate) fn par_map<T: Send, F: Fn(usize) -> T + Sync>(n: usize, f: F) -> Vec<T> {
    let threads = thread_budget().min(n.max(1));
    if threads <= 1 {
        return (0..n).map(f).collect();
    }
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let f = &f;
                s.spawn(move || {
                    (t..n)
                        .step_by(threads)
                        .map(|i| (i, f(i)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut all: Vec<(usize, T)> = handles
            .into_iter()
            .flat_map(|h| h.join().expect("worker panicked"))
            .collect();
        all.sort_by_key(|x| x.0);
        all.into_iter().map(|x| x.1).collect()
    })
}

const KR: f64 = 0.2126;
const KB: f64 = 0.0722;
const KG: f64 = 1.0 - KR - KB;
const CONV: [[f64; 4]; 3] = [
    [2.921954, 9.82974, 0.992308, 256.499207],
    [-1.610063, -5.416347, 7.026407, 2048.04826],
    [7.026417, -6.382119, -0.644283, 2048.04492],
];
const AC_OFFSET: f64 = 0.25;
const BASIS: [[f64; 8]; 8] = [
    [
        0.3535533905932738,
        0.3535533905932738,
        0.3535533905932738,
        0.3535533905932738,
        0.3535533905932738,
        0.3535533905932738,
        0.3535533905932738,
        0.3535533905932738,
    ],
    [
        0.4903926402016152,
        0.4157348061512726,
        0.27778511650980114,
        0.09754516100806417,
        -0.0975451610080641,
        -0.277785116509801,
        -0.4157348061512727,
        -0.4903926402016152,
    ],
    [
        0.46193976625564337,
        0.19134171618254492,
        -0.19134171618254486,
        -0.46193976625564337,
        -0.4619397662556434,
        -0.19134171618254517,
        0.191341716182545,
        0.46193976625564326,
    ],
    [
        0.4157348061512726,
        -0.0975451610080641,
        -0.4903926402016152,
        -0.2777851165098011,
        0.2777851165098009,
        0.4903926402016152,
        0.09754516100806439,
        -0.41573480615127256,
    ],
    [
        0.3535533905932738,
        -0.35355339059327373,
        -0.35355339059327384,
        0.3535533905932737,
        0.35355339059327384,
        -0.35355339059327334,
        -0.35355339059327356,
        0.3535533905932733,
    ],
    [
        0.27778511650980114,
        -0.4903926402016152,
        0.09754516100806415,
        0.41573480615127273,
        -0.41573480615127256,
        -0.09754516100806401,
        0.4903926402016153,
        -0.27778511650980076,
    ],
    [
        0.19134171618254492,
        -0.4619397662556434,
        0.46193976625564326,
        -0.19134171618254495,
        -0.19134171618254528,
        0.46193976625564337,
        -0.4619397662556432,
        0.19134171618254478,
    ],
    [
        0.09754516100806417,
        -0.2777851165098011,
        0.41573480615127273,
        -0.4903926402016153,
        0.4903926402016152,
        -0.4157348061512725,
        0.27778511650980076,
        -0.09754516100806429,
    ],
];

fn qscale_value(q: u8) -> f64 {
    if q > 128 {
        (((q as i32) - 96) << 2) as f64
    } else {
        q.max(1) as f64
    }
}

fn source_planes(d: &Decoded, fr: &Frame) -> Planes {
    let (wp, hp) = (d.wp, d.hp);
    let mut ycc = [
        vec![0f64; wp * hp],
        vec![0f64; wp * hp],
        vec![0f64; wp * hp],
    ];
    let mbw = wp / 16;
    for (p, plane) in ycc.iter_mut().enumerate() {
        let qm = &fr.qmat[usize::from(p > 0)];
        for by in 0..hp / 8 {
            for bx in 0..wp / 8 {
                let qs = qscale_value(d.qmb[(by / 2) * mbw + bx / 2]);
                let mut x = [[0f64; 8]; 8];
                for u in 0..8 {
                    for v in 0..8 {
                        let mut c = d.coef[p][(by * 8 + u) * wp + bx * 8 + v] as f64;
                        if u == 0 && v == 0 {
                            c += 4096.0;
                        }
                        x[u][v] = c * qm[u * 8 + v] as f64 * qs;
                    }
                }
                let rows: Vec<usize> = (0..8).filter(|&u| x[u].iter().any(|&v| v != 0.0)).collect();
                let mut t = [[0f64; 8]; 8];
                for &u in &rows {
                    for j in 0..8 {
                        let mut acc = 0.0;
                        for v in 0..8 {
                            acc += x[u][v] * BASIS[v][j];
                        }
                        t[u][j] = acc;
                    }
                }
                for i in 0..8 {
                    for j in 0..8 {
                        let mut acc = 0.0;
                        for &u in &rows {
                            acc += BASIS[u][i] * t[u][j];
                        }
                        plane[(by * 8 + i) * wp + bx * 8 + j] = acc;
                    }
                }
            }
        }
    }
    let mut s = Planes::new(wp, hp);
    for i in 0..wp * hp {
        let y = (ycc[0][i] - 256.0) / 3504.0;
        let cb = (ycc[1][i] - 2048.0) / 3584.0;
        let cr = (ycc[2][i] - 2048.0) / 3584.0;
        let r = y + 2.0 * (1.0 - KR) * cr;
        let b = y + 2.0 * (1.0 - KB) * cb;
        let g = (y - KR * r - KB * b) / KG;
        let q = |c: f64| (c * 255.0).round().clamp(0.0, 255.0) as u8;
        s.p[0][i] = (d.alpha[i] / 257) as u8;
        s.p[1][i] = q(g);
        s.p[2][i] = q(r);
        s.p[3][i] = q(b);
    }
    s
}

fn predict_block(
    s: &Planes,
    by: usize,
    bx: usize,
    p: usize,
    qm: &[u8; 64],
    qs: f64,
    q: &mut [i16],
    z: &mut [f32],
    stride: usize,
) {
    let wp = s.w;
    let mut px = [[0f64; 8]; 8];
    for (i, row) in px.iter_mut().enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            let o = (by * 8 + i) * wp + bx * 8 + j;
            let (r, g, b) = (s.p[2][o] as f64, s.p[1][o] as f64, s.p[3][o] as f64);
            *cell = (CONV[p][0] * r + CONV[p][1] * g + CONV[p][2] * b + CONV[p][3]).floor();
        }
    }
    let mut t = [[0f64; 8]; 8];
    for u in 0..8 {
        for j in 0..8 {
            let mut acc = 0.0;
            for i in 0..8 {
                acc += BASIS[u][i] * px[i][j];
            }
            t[u][j] = acc;
        }
    }
    for u in 0..8 {
        for v in 0..8 {
            let mut acc = 0.0;
            for j in 0..8 {
                acc += t[u][j] * BASIS[v][j];
            }
            let centred = if u == 0 && v == 0 { acc - 16384.0 } else { acc };
            let zv = centred / (qm[u * 8 + v] as f64 * qs);
            let level = (zv.abs() + AC_OFFSET).floor() as i32 * if zv < 0.0 { -1 } else { 1 };
            q[u * stride + bx * 8 + v] = level.clamp(-32768, 32767) as i16;
            z[u * stride + bx * 8 + v] = zv as f32;
        }
    }
}

struct Pred {
    q: [Vec<i16>; 3],
    z: [Vec<f32>; 3],
}
fn predict_row(s: &Planes, fr: &Frame, qmb: &[u8], by: usize) -> Pred {
    let wp = s.w;
    let mut out = Pred {
        q: std::array::from_fn(|_| vec![0; wp * 8]),
        z: std::array::from_fn(|_| vec![0.0; wp * 8]),
    };
    for bx in 0..wp / 8 {
        let qs = qscale_value(qmb[(by / 2) * (wp / 16) + bx / 2]);
        for p in 0..3 {
            predict_block(
                s,
                by,
                bx,
                p,
                &fr.qmat[usize::from(p > 0)],
                qs,
                &mut out.q[p],
                &mut out.z[p],
                wp,
            );
        }
    }
    out
}
fn predict_frame(s: &Planes, fr: &Frame, qmb: &[u8], parallel: bool) -> Pred {
    let rows = s.h / 8;
    let parts = if parallel {
        par_map(rows, |by| predict_row(s, fr, qmb, by))
    } else {
        (0..rows).map(|by| predict_row(s, fr, qmb, by)).collect()
    };
    let mut out = Pred {
        q: std::array::from_fn(|_| Vec::with_capacity(s.w * s.h)),
        z: std::array::from_fn(|_| Vec::with_capacity(s.w * s.h)),
    };
    for part in parts {
        for p in 0..3 {
            out.q[p].extend_from_slice(&part.q[p]);
            out.z[p].extend_from_slice(&part.z[p]);
        }
    }
    out
}

fn zctx(p: usize, u: usize, v: usize, z: f32) -> usize {
    let band = (u + v).min(7);
    let bucket = if u == 0 && v == 0 {
        0
    } else {
        ((z.abs() * 10.0) as usize).min(31)
    };
    (p * 8 + band) * 32 + bucket
}

fn thumb(p: &Planes) -> Vec<i32> {
    let mut t = vec![0i32; 32 * 32 * 4];
    for y in 0..p.h {
        for x in 0..p.w {
            let b = ((y * 32 / p.h) * 32 + x * 32 / p.w) * 4;
            for c in 0..4 {
                t[b + c] += p.p[c][y * p.w + x] as i32;
            }
        }
    }
    t
}

#[derive(Default)]
pub struct Pool {
    frames: Vec<PoolFrame>,
}
struct PoolFrame {
    wp: usize,
    hp: usize,
    planes: Vec<u8>,
    resid: Vec<u8>,
    qmb: Vec<u8>,
    thumb: Vec<i32>,
}
impl PoolFrame {
    fn new(s: &Planes, resid: &[Vec<i16>; 3], qmb: &[u8], thumb: Vec<i32>) -> Self {
        let mut raw = Vec::with_capacity(s.w * s.h * 4);
        for c in &s.p {
            raw.extend_from_slice(c);
        }
        let mut r = Vec::with_capacity(s.w * s.h * 6);
        for c in resid {
            for v in c {
                r.extend_from_slice(&v.to_le_bytes());
            }
        }
        PoolFrame {
            wp: s.w,
            hp: s.h,
            planes: zstd::bulk::compress(&raw, 1).unwrap_or_default(),
            resid: zstd::bulk::compress(&r, 1).unwrap_or_default(),
            qmb: qmb.to_vec(),
            thumb,
        }
    }
    fn load(&self) -> Result<(Planes, [Vec<i16>; 3]), String> {
        let n = self.wp * self.hp;
        let raw = zstd::bulk::decompress(&self.planes, n * 4).map_err(|e| e.to_string())?;
        let r = zstd::bulk::decompress(&self.resid, n * 6).map_err(|e| e.to_string())?;
        if raw.len() != n * 4 || r.len() != n * 6 {
            return Err("invalid reference frame".into());
        }
        let mut s = Planes::new(self.wp, self.hp);
        for (c, plane) in s.p.iter_mut().enumerate() {
            plane.copy_from_slice(&raw[c * n..(c + 1) * n]);
        }
        let resid = std::array::from_fn(|c| {
            r[c * n * 2..(c + 1) * n * 2]
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect()
        });
        Ok((s, resid))
    }
}
fn flush_pool(pending: &mut Vec<(Planes, [Vec<i16>; 3], Vec<u8>)>, pool: &mut Pool) {
    let frames = par_map(pending.len(), |i| {
        let (s, r, q) = &pending[i];
        PoolFrame::new(s, r, q, thumb(s))
    });
    pool.frames.extend(frames);
    pending.clear();
}

fn nearest_refs(t: &[i32], wp: usize, hp: usize, pool: &Pool) -> Vec<u16> {
    let mut best: Vec<(i64, usize)> = pool
        .frames
        .iter()
        .enumerate()
        .filter(|(_, f)| f.wp == wp && f.hp == hp)
        .map(|(i, f)| {
            (
                f.thumb
                    .iter()
                    .zip(t)
                    .map(|(a, b)| (a - b).abs() as i64)
                    .sum(),
                i,
            )
        })
        .collect();
    best.sort_unstable();
    best.iter()
        .take(EXTRA_REFS)
        .map(|&(_, i)| i as u16)
        .collect()
}

fn novel_matches(cur: &Planes, reference: &Planes, prev: Option<&Planes>) -> usize {
    let block_eq = |a: &Planes, by: usize, bx: usize| {
        (0..8).all(|i| {
            let o = (by * 8 + i) * cur.w + bx * 8;
            (0..4).all(|c| a.p[c][o..o + 8] == cur.p[c][o..o + 8])
        })
    };
    let mut n = 0;
    for by in 0..cur.h / 8 {
        for bx in 0..cur.w / 8 {
            if block_eq(reference, by, bx) && !prev.is_some_and(|p| block_eq(p, by, bx)) {
                n += 1;
            }
        }
    }
    n
}

fn block_same(a: &Planes, b: &Planes, by: usize, bx: usize) -> bool {
    let w = a.w;
    (0..8).all(|i| {
        let o = (by * 8 + i) * w + bx * 8;
        (1..4).all(|c| a.p[c][o..o + 8] == b.p[c][o..o + 8])
    })
}
fn reuse<'a>(
    cands: &[(&'a Planes, &'a [Vec<i16>; 3], &'a [u8])],
    s: &Planes,
    qmb: &[u8],
    by: usize,
    bx: usize,
) -> Option<&'a [Vec<i16>; 3]> {
    let m = (by / 2) * (s.w / 16) + bx / 2;
    cands
        .iter()
        .find(|(cs, _, cq)| {
            cs.w == s.w && cs.h == s.h && cq[m] == qmb[m] && block_same(cs, s, by, bx)
        })
        .map(|c| c.1)
}

struct ResidualModels {
    spatial: ResidualModel,
    temporal: ResidualModel,
}
impl ResidualModels {
    fn new() -> Self {
        ResidualModels {
            spatial: ResidualModel::new(3 * 8 * 32),
            temporal: ResidualModel::new(6),
        }
    }
}

fn put_section(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u64).to_le_bytes());
    out.extend_from_slice(data);
}
fn get_section<'a>(data: &'a [u8], at: &mut usize) -> Result<&'a [u8], String> {
    let len = data.get(*at..*at + 8).ok_or("truncated source record")?;
    let len = u64::from_le_bytes(len.try_into().unwrap()) as usize;
    let s = data
        .get(*at + 8..(*at + 8).checked_add(len).ok_or("invalid source record")?)
        .ok_or("truncated source record")?;
    *at += 8 + len;
    Ok(s)
}
fn unzstd(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut d = zstd::stream::read::Decoder::new(data).map_err(|e| e.to_string())?;
    std::io::Read::read_to_end(&mut std::io::Read::take(&mut d, limit as u64 + 1), &mut out)
        .map_err(|e| e.to_string())?;
    if out.len() > limit {
        return Err("source record section too large".into());
    }
    Ok(out)
}

struct Analysis {
    d: Decoded,
    s: Planes,
    pred_z: [Vec<f32>; 3],
    resid: [Vec<i16>; 3],
    thumb: Vec<i32>,
}
fn analyse(raw: &[u8], fr: &Frame) -> Result<Analysis, String> {
    let d = pr::decode(raw, fr)?;
    let s = source_planes(&d, fr);
    let pred = predict_frame(&s, fr, &d.qmb, false);
    let resid: [Vec<i16>; 3] = std::array::from_fn(|p| {
        d.coef[p]
            .iter()
            .zip(&pred.q[p])
            .map(|(a, b)| a.wrapping_sub(*b))
            .collect()
    });
    let thumb = thumb(&s);
    Ok(Analysis {
        d,
        s,
        pred_z: pred.z,
        resid,
        thumb,
    })
}

fn analysis_chunk(fr: &Frame) -> usize {
    let frame_bytes = fr.mbw() * fr.mbh() * 256 * 48;
    thread_budget()
        .min((256usize << 20) / frame_bytes.max(1))
        .max(1)
}

pub fn shares_content(a: &[u8], b: &[u8]) -> bool {
    let (fa, fb) = (pr::frames(a), pr::frames(b));
    if fa.is_empty() || fb.is_empty() || fa[0].width != fb[0].width || fa[0].height != fb[0].height
    {
        return false;
    }
    let picks: Vec<(usize, usize)> = [0usize, 1, 2]
        .iter()
        .map(|&k| (k * (fa.len() - 1) / 2, k * (fb.len() - 1) / 2))
        .collect();
    par_map(picks.len(), |k| {
        let (i, j) = picks[k];
        let (Ok(da), Ok(db)) = (pr::decode(a, &fa[i]), pr::decode(b, &fb[j])) else {
            return false;
        };
        let (sa, sb) = (source_planes(&da, &fa[i]), source_planes(&db, &fb[j]));
        let (w, bh, bw) = (sa.w, sa.h / 8, sa.w / 8);
        let mut equal = 0;
        for by in 0..bh {
            for bx in 0..bw {
                let mut flat = true;
                let mut same = true;
                for c in 0..4 {
                    for r in 0..8 {
                        let o = (by * 8 + r) * w + bx * 8;
                        let (x, y) = (&sa.p[c][o..o + 8], &sb.p[c][o..o + 8]);
                        flat &= x.iter().all(|&v| v == sa.p[c][by * 8 * w + bx * 8]);
                        same &= x == y;
                    }
                }
                equal += usize::from(!flat && same);
            }
        }
        equal * 100 >= bh * bw
    })
    .into_iter()
    .any(|x| x)
}

pub fn pool_of(raw: &[u8]) -> Result<Pool, String> {
    let frames = pr::frames(raw);
    if frames.is_empty() || frames.len() > MAX_FRAMES || raw.len() > MAX_OUTPUT {
        return Err("no supported ProRes frames".into());
    }
    let mut pool = Pool::default();
    for group in frames.chunks(analysis_chunk(&frames[0])) {
        let parts = par_map(group.len(), |i| {
            let fr = &group[i];
            let d = pr::decode(raw, fr)?;
            let s = source_planes(&d, fr);
            let pred = predict_frame(&s, fr, &d.qmb, false);
            let resid: [Vec<i16>; 3] = std::array::from_fn(|p| {
                d.coef[p]
                    .iter()
                    .zip(&pred.q[p])
                    .map(|(a, b)| a.wrapping_sub(*b))
                    .collect()
            });
            Ok::<_, String>(PoolFrame::new(&s, &resid, &d.qmb, thumb(&s)))
        });
        for p in parts {
            pool.frames.push(p?);
        }
    }
    Ok(pool)
}

pub fn encode(
    raw: &[u8],
    pool: &Pool,
    light: bool,
    cancelled: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Vec<u8>, String> {
    let frames = pr::frames(raw);
    if frames.is_empty() || frames.len() > MAX_FRAMES || raw.len() > MAX_OUTPUT {
        return Err("no supported ProRes frames".into());
    }
    let mut skeleton = Vec::new();
    let mut table = Vec::new();
    let mut heads = Vec::new();
    let mut qscales = Vec::new();
    let mut at = 0;
    for fr in &frames {
        if fr.offset < at {
            return Err("overlapping ProRes frames".into());
        }
        skeleton.extend_from_slice(&raw[at..fr.offset]);
        table.extend_from_slice(&((fr.offset - at) as u64).to_le_bytes());
        table.extend_from_slice(&(fr.len as u32).to_le_bytes());
        heads.extend_from_slice(&(fr.head_len as u16).to_le_bytes());
        heads.extend_from_slice(&raw[fr.offset..fr.offset + fr.head_len]);
        qscales.extend(fr.slices.iter().map(|s| s.qscale));
        at = fr.offset + fr.len;
    }
    skeleton.extend_from_slice(&raw[at..]);
    let mut refs_side = Vec::new();
    let mut senc = sc::Encoder::new(light);
    let mut rbands: Vec<(ResidualModels, Enc)> = (0..BANDS)
        .map(|_| (ResidualModels::new(), Enc::new()))
        .collect();
    let mut prev: Option<(Planes, [Vec<i16>; 3], Vec<u8>)> = None;
    let chunk = analysis_chunk(&frames[0]);
    let mut done = 0;
    for group in frames.chunks(chunk) {
        let analysed = par_map(group.len(), |i| analyse(raw, &group[i]));
        for (a, fr) in analysed.into_iter().zip(group) {
            let a = a?;
            let (wp, hp) = (a.d.wp, a.d.hp);
            let nearest = nearest_refs(&a.thumb, wp, hp, pool);
            let mut loaded = Vec::with_capacity(nearest.len());
            let mut chosen = Vec::new();
            if let Some(&first) = nearest.first() {
                let pf = &pool.frames[first as usize];
                let (s, r) = pf.load()?;
                let blocks = (wp / 8) * (hp / 8);
                if novel_matches(&a.s, &s, prev.as_ref().map(|p| &p.0)) * 1000 >= blocks {
                    chosen = nearest.clone();
                    loaded.push((s, r, pf.qmb.clone()));
                    for &i in &nearest[1..] {
                        let pf = &pool.frames[i as usize];
                        let (s, r) = pf.load()?;
                        loaded.push((s, r, pf.qmb.clone()));
                    }
                }
            }
            refs_side.push(chosen.len() as u8);
            for &i in &chosen {
                refs_side.extend_from_slice(&i.to_le_bytes());
            }
            let extra: Vec<&Planes> = loaded.iter().map(|l| &l.0).collect();
            senc.frame_extra(&a.s, &extra);
            let mut cands: Vec<(&Planes, &[Vec<i16>; 3], &[u8])> = Vec::new();
            if let Some((ps, pr_, pq)) = &prev {
                cands.push((ps, pr_, pq));
            }
            for (s, r, q) in &loaded {
                cands.push((s, r, q));
            }
            let (cands_r, a_r) = (&cands, &a);
            par_each(&mut rbands, |b, (models, renc)| {
                let (cands, a) = (cands_r, a_r);
                let (b0, b1) = band_rows(hp / 8, b);
                for by in b0..b1 {
                    for bx in 0..wp / 8 {
                        let hit = reuse(cands, &a.s, &a.d.qmb, by, bx);
                        for p in 0..3 {
                            for u in 0..8 {
                                for v in 0..8 {
                                    let o = (by * 8 + u) * wp + bx * 8 + v;
                                    let r = a.resid[p][o] as i32;
                                    if let Some(cr) = hit {
                                        models.temporal.encode(
                                            renc,
                                            p * 2 + usize::from(u == 0 && v == 0),
                                            r - cr[p][o] as i32,
                                        );
                                    } else {
                                        let z = a.pred_z[p][o];
                                        models.spatial.encode(
                                            renc,
                                            zctx(p, u, v, z),
                                            if z < 0.0 { -r } else { r },
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            });
            prev = Some((a.s, a.resid, a.d.qmb));
            done += fr.len as u64;
            if !cancelled(done, raw.len() as u64) {
                return Err("error.cancelled".into());
            }
        }
    }
    let z = |b: &[u8]| zstd::bulk::compress(b, 19).map_err(|e| e.to_string());
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&(frames.len() as u32).to_le_bytes());
    put_section(&mut out, &z(&skeleton)?);
    put_section(&mut out, &z(&table)?);
    put_section(&mut out, &z(&heads)?);
    put_section(&mut out, &z(&qscales)?);
    put_section(&mut out, &z(&refs_side)?);
    put_section(&mut out, &senc.finish());
    let streams: Vec<Vec<u8>> = rbands.into_iter().map(|(_, e)| e.finish()).collect();
    let mut r_all = Vec::new();
    for st in &streams {
        r_all.extend_from_slice(&(st.len() as u32).to_le_bytes());
    }
    for st in streams {
        r_all.extend(st);
    }
    put_section(&mut out, &r_all);
    Ok(out)
}

pub fn uses_pool(data: &[u8]) -> Result<bool, String> {
    if data.get(..8) != Some(MAGIC) {
        return Err("invalid source record".into());
    }
    let nfr = u32::from_le_bytes(
        data.get(8..12)
            .ok_or("truncated source record")?
            .try_into()
            .unwrap(),
    ) as usize;
    if nfr == 0 || nfr > MAX_FRAMES {
        return Err("invalid source record".into());
    }
    let mut at = 12;
    for _ in 0..4 {
        get_section(data, &mut at)?;
    }
    let refs_side = unzstd(get_section(data, &mut at)?, nfr * (1 + 2 * EXTRA_REFS))?;
    let mut i = 0;
    while i < refs_side.len() {
        let n = refs_side[i] as usize;
        if n > 0 {
            return Ok(true);
        }
        i += 1 + 2 * n;
    }
    Ok(false)
}

pub fn decode(
    data: &[u8],
    pool: &Pool,
    keep_pool: bool,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<(Vec<u8>, Pool), String> {
    if data.get(..8) != Some(MAGIC) {
        return Err("invalid source record".into());
    }
    let nfr = u32::from_le_bytes(
        data.get(8..12)
            .ok_or("truncated source record")?
            .try_into()
            .unwrap(),
    ) as usize;
    if nfr == 0 || nfr > MAX_FRAMES {
        return Err("invalid source record".into());
    }
    let mut at = 12;
    let skeleton = unzstd(get_section(data, &mut at)?, MAX_OUTPUT)?;
    let table = unzstd(get_section(data, &mut at)?, nfr * 12)?;
    let heads = unzstd(get_section(data, &mut at)?, nfr * 4096)?;
    let qscales = unzstd(get_section(data, &mut at)?, MAX_OUTPUT)?;
    let refs_side = unzstd(get_section(data, &mut at)?, nfr * (1 + 2 * EXTRA_REFS))?;
    let s_stream = get_section(data, &mut at)?;
    let r_stream = get_section(data, &mut at)?;
    if at != data.len() || table.len() != nfr * 12 {
        return Err("invalid source record".into());
    }
    let total: usize = skeleton.len()
        + (0..nfr)
            .map(|i| {
                u32::from_le_bytes(table[i * 12 + 8..i * 12 + 12].try_into().unwrap()) as usize
            })
            .sum::<usize>();
    if total > MAX_OUTPUT {
        return Err("source record output too large".into());
    }
    let mut sdec = sc::Decoder::new(s_stream)?;
    let mut rbands: Vec<(ResidualModels, Dec)> = Vec::with_capacity(BANDS);
    let mut rat_ = 4 * BANDS;
    for b in 0..BANDS {
        let len = r_stream
            .get(b * 4..b * 4 + 4)
            .ok_or("truncated residual stream")?;
        let len = u32::from_le_bytes(len.try_into().unwrap()) as usize;
        let st = r_stream
            .get(rat_..rat_.checked_add(len).ok_or("invalid residual stream")?)
            .ok_or("truncated residual stream")?;
        rbands.push((ResidualModels::new(), Dec::new(st)));
        rat_ += len;
    }
    if rat_ != r_stream.len() {
        return Err("invalid residual stream".into());
    }
    let mut out = Vec::with_capacity(total);
    let mut next = Pool::default();
    let mut prev: Option<(Planes, [Vec<i16>; 3], Vec<u8>)> = None;
    let mut pending: Vec<(Planes, [Vec<i16>; 3], Vec<u8>)> = Vec::new();
    let (mut sk, mut hat, mut qat, mut rat) = (0usize, 0usize, 0usize, 0usize);
    for fi in 0..nfr {
        let gap = u64::from_le_bytes(table[fi * 12..fi * 12 + 8].try_into().unwrap()) as usize;
        let flen =
            u32::from_le_bytes(table[fi * 12 + 8..fi * 12 + 12].try_into().unwrap()) as usize;
        out.extend_from_slice(
            skeleton
                .get(sk..sk.checked_add(gap).ok_or("invalid layout")?)
                .ok_or("invalid layout")?,
        );
        sk += gap;
        let hl = u16::from_le_bytes(
            heads
                .get(hat..hat + 2)
                .ok_or("invalid heads")?
                .try_into()
                .unwrap(),
        ) as usize;
        let head = heads.get(hat + 2..hat + 2 + hl).ok_or("invalid heads")?;
        hat += 2 + hl;
        let fr = pr::parse_head(head, flen)?;
        let (wp, hp) = (fr.mbw() * 16, fr.mbh() * 16);
        let qs = qscales
            .get(qat..qat + fr.slices.len())
            .ok_or("invalid quantizers")?;
        qat += fr.slices.len();
        let mut qmb = vec![0u8; fr.mbw() * fr.mbh()];
        for (s, &q) in fr.slices.iter().zip(qs) {
            for m in 0..s.mbs {
                qmb[s.mb_y * fr.mbw() + s.mb_x + m] = q;
            }
        }
        let nref = *refs_side.get(rat).ok_or("invalid references")? as usize;
        if nref > EXTRA_REFS {
            return Err("invalid references".into());
        }
        let mut loaded = Vec::with_capacity(nref);
        for k in 0..nref {
            let b = refs_side
                .get(rat + 1 + 2 * k..rat + 3 + 2 * k)
                .ok_or("invalid references")?;
            let pf = pool
                .frames
                .get(u16::from_le_bytes([b[0], b[1]]) as usize)
                .ok_or("missing reference frame")?;
            let (s, r) = pf.load()?;
            loaded.push((s, r, pf.qmb.clone()));
        }
        rat += 1 + 2 * nref;
        let extra: Vec<&Planes> = loaded.iter().map(|l| &l.0).collect();
        let s = sdec.frame_extra(wp, hp, &extra);
        let pred = predict_frame(&s, &fr, &qmb, true);
        let mut resid: [Vec<i16>; 3] = std::array::from_fn(|_| Vec::with_capacity(wp * hp));
        {
            let mut cands: Vec<(&Planes, &[Vec<i16>; 3], &[u8])> = Vec::new();
            if let Some((ps, pr_, pq)) = &prev {
                cands.push((ps, pr_, pq));
            }
            for (ls, lr, lq) in &loaded {
                cands.push((ls, lr, lq));
            }
            let (cands, s_, qmb_, pz) = (&cands, &s, &qmb, &pred.z);
            let parts: Vec<[Vec<i16>; 3]> = par_each(&mut rbands, |b, (models, rdec)| {
                let (b0, b1) = band_rows(hp / 8, b);
                let base = b0 * 8 * wp;
                let mut part: [Vec<i16>; 3] =
                    std::array::from_fn(|_| vec![0i16; (b1 - b0) * 8 * wp]);
                for by in b0..b1 {
                    for bx in 0..wp / 8 {
                        let hit = reuse(cands, s_, qmb_, by, bx);
                        for (p, plane) in part.iter_mut().enumerate() {
                            for u in 0..8 {
                                for v in 0..8 {
                                    let o = (by * 8 + u) * wp + bx * 8 + v;
                                    plane[o - base] = if let Some(cr) = hit {
                                        (cr[p][o] as i32
                                            + models.temporal.decode(
                                                rdec,
                                                p * 2 + usize::from(u == 0 && v == 0),
                                            )) as i16
                                    } else {
                                        let z = pz[p][o];
                                        let r = models.spatial.decode(rdec, zctx(p, u, v, z));
                                        (if z < 0.0 { -r } else { r }) as i16
                                    };
                                }
                            }
                        }
                    }
                }
                part
            });
            for part in parts {
                for p in 0..3 {
                    resid[p].extend_from_slice(&part[p]);
                }
            }
        }
        let coef: [Vec<i16>; 3] = std::array::from_fn(|p| {
            pred.q[p]
                .iter()
                .zip(&resid[p])
                .map(|(a, b)| a.wrapping_add(*b))
                .collect()
        });
        let alpha: Vec<u16> = s.p[0].iter().map(|&a| a as u16 * 257).collect();
        out.extend_from_slice(&pr::rebuild(&fr, head, qs, &coef, &alpha, true)?);
        if let Some((ps, pr_, pq)) = prev.take() {
            if keep_pool {
                pending.push((ps, pr_, pq));
            }
        }
        if pending.len() >= 16 {
            flush_pool(&mut pending, &mut next);
        }
        prev = Some((s, resid, qmb));
        if !progress(out.len() as u64, total as u64) {
            return Err("error.cancelled".into());
        }
    }
    if let Some(last) = prev.take() {
        if keep_pool {
            pending.push(last);
        }
    }
    flush_pool(&mut pending, &mut next);
    out.extend_from_slice(skeleton.get(sk..).ok_or("invalid layout")?);
    if qat != qscales.len() || hat != heads.len() || rat != refs_side.len() {
        return Err("invalid source record".into());
    }
    Ok((out, next))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn par_map_keeps_order() {
        assert_eq!(
            par_map(1000, |i| i * 3),
            (0..1000).map(|i| i * 3).collect::<Vec<_>>()
        );
    }

    #[test]
    fn non_prores_input_is_rejected() {
        let raw = vec![0u8; 4096];
        assert!(encode(&raw, &Pool::default(), false, &mut |_, _| true).is_err());
    }

    #[test]
    fn malformed_records_are_errors_not_panics() {
        let mut seed = 5u64;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 33) as u8
        };
        for len in [0usize, 8, 12, 64, 1000] {
            let mut data: Vec<u8> = (0..len).map(|_| next()).collect();
            if len >= 12 {
                data[..8].copy_from_slice(MAGIC);
                data[8..12].copy_from_slice(&3u32.to_le_bytes());
            }
            assert!(decode(&data, &Pool::default(), true, &mut |_, _| true).is_err());
        }
        let mut data = MAGIC.to_vec();
        data.extend_from_slice(&1u32.to_le_bytes());
        for _ in 0..7 {
            data.extend_from_slice(&0u64.to_le_bytes());
        }
        assert!(decode(&data, &Pool::default(), true, &mut |_, _| true).is_err());
    }
}
