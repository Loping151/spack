use super::source_rc::*;

const SAME: usize = 0;
const SPATIAL: usize = 1;
const TCOPY: usize = 2;
const TGRAD: usize = 3;
const NREF: usize = 2;
const MAXREF: usize = 5;
pub(crate) const BANDS: usize = 8;
const ACT: usize = 12;
const GB: usize = 4;

#[derive(Clone)]
pub(crate) struct Planes {
    pub w: usize,
    pub h: usize,
    pub p: [Vec<u8>; 4],
}
impl Planes {
    pub fn new(w: usize, h: usize) -> Self {
        Planes {
            w,
            h,
            p: std::array::from_fn(|_| vec![0; w * h]),
        }
    }
    #[inline]
    fn at(&self, c: usize, y: isize, x: isize) -> i32 {
        let y = y.clamp(0, self.h as isize - 1) as usize;
        let x = x.clamp(0, self.w as isize - 1) as usize;
        self.p[c][y * self.w + x] as i32
    }
    #[inline]
    fn half(&self, c: usize, y2: isize, x2: isize) -> i32 {
        let (y, x) = (y2 >> 1, x2 >> 1);
        match (y2 & 1, x2 & 1) {
            (0, 0) => self.at(c, y, x),
            (0, _) => (self.at(c, y, x) + self.at(c, y, x + 1) + 1) >> 1,
            (_, 0) => (self.at(c, y, x) + self.at(c, y + 1, x) + 1) >> 1,
            _ => {
                (self.at(c, y, x)
                    + self.at(c, y, x + 1)
                    + self.at(c, y + 1, x)
                    + self.at(c, y + 1, x + 1)
                    + 2)
                    >> 2
            }
        }
    }
}

pub(crate) fn band_rows(bh: usize, b: usize) -> (usize, usize) {
    (b * bh / BANDS, (b + 1) * bh / BANDS)
}

pub(crate) fn par_each<T: Send, R: Send>(
    items: &mut [T],
    f: impl Fn(usize, &mut T) -> R + Sync,
) -> Vec<R> {
    std::thread::scope(|s| {
        let handles: Vec<_> = items
            .iter_mut()
            .enumerate()
            .map(|(i, t)| {
                let f = &f;
                s.spawn(move || f(i, t))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("worker panicked"))
            .collect()
    })
}

#[inline]
fn med(a: i32, b: i32, c: i32) -> i32 {
    let (mn, mx) = if a < b { (a, b) } else { (b, a) };
    if c >= mx {
        mn
    } else if c <= mn {
        mx
    } else {
        a + b - c
    }
}
#[inline]
fn wrap(e: i32) -> i32 {
    ((e + 128) & 255) - 128
}
#[inline]
fn act_bucket(a: i32) -> usize {
    match a {
        0..=2 => a as usize,
        3..=4 => 3,
        5..=6 => 4,
        7..=9 => 5,
        10..=14 => 6,
        15..=22 => 7,
        23..=35 => 8,
        36..=60 => 9,
        61..=110 => 10,
        _ => 11,
    }
}
#[inline]
fn gbucket(c: usize, eg: i32) -> usize {
    if c < 2 {
        return 0;
    }
    match eg.unsigned_abs() {
        0 => 0,
        1..=2 => 1,
        3..=8 => 2,
        _ => 3,
    }
}

struct Models {
    mode: Vec<u16>,
    refi: Vec<u16>,
    mv: ResidualModel,
    res: ResidualModel,
}
impl Models {
    fn new() -> Self {
        Models {
            mode: vec![PROB_INIT; 16 * 4],
            refi: vec![PROB_INIT; 8],
            mv: ResidualModel::new(2),
            res: ResidualModel::new(4 * 2 * ACT * GB),
        }
    }
}

struct Cur<'a> {
    buf: &'a Planes,
    y0: usize,
    top: usize,
}
impl Cur<'_> {
    #[inline]
    fn at(&self, c: usize, y: isize, x: isize) -> i32 {
        self.buf.at(c, y - self.y0 as isize, x)
    }
}

#[inline]
fn spatial_nb(top: usize, y: isize, x: isize, f: impl Fn(isize, isize) -> i32) -> (i32, i32, i32) {
    match (y > top as isize, x > 0) {
        (true, true) => (f(y, x - 1), f(y - 1, x), f(y - 1, x - 1)),
        (true, false) => {
            let n = f(y - 1, x);
            (n, n, n)
        }
        (false, true) => {
            let w = f(y, x - 1);
            (w, w, w)
        }
        (false, false) => (0, 0, 0),
    }
}

#[inline]
fn predict(
    cur: &Cur,
    rf: &Planes,
    mode: usize,
    mv: (isize, isize),
    c: usize,
    y: usize,
    x: usize,
) -> i32 {
    let (yi, xi) = (y as isize, x as isize);
    match mode {
        SPATIAL => {
            if c >= 2 {
                let (w, n, nw) = spatial_nb(cur.top, yi, xi, |yy, xx| {
                    cur.at(c, yy, xx) - cur.at(1, yy, xx)
                });
                cur.at(1, yi, xi) + med(w, n, nw)
            } else {
                let (w, n, nw) = spatial_nb(cur.top, yi, xi, |yy, xx| cur.at(c, yy, xx));
                med(w, n, nw)
            }
        }
        TCOPY => rf.half(c, 2 * yi + mv.0, 2 * xi + mv.1),
        _ => {
            let r = |yy: isize, xx: isize| rf.half(c, 2 * yy + mv.0, 2 * xx + mv.1);
            let (w, n, nw) = spatial_nb(cur.top, yi, xi, |yy, xx| cur.at(c, yy, xx) - r(yy, xx));
            r(yi, xi) + med(w, n, nw)
        }
    }
}

#[inline]
fn neighbour_act(rm: &[u8], w: usize, y: usize, x: usize) -> i32 {
    let g = |yy: usize, xx: usize| rm[yy * w + xx] as i32;
    let mut a = 0;
    if x > 0 {
        a += 2 * g(y, x - 1);
    }
    if y > 0 {
        a += 2 * g(y - 1, x);
        if x > 0 {
            a += g(y - 1, x - 1);
        }
        if x + 1 < w {
            a += g(y - 1, x + 1);
        }
    }
    a / 2
}

fn fast_cost(
    cur: &Planes,
    rf: &Planes,
    mode: usize,
    mv: (isize, isize),
    by: usize,
    bx: usize,
) -> Option<u32> {
    let (y0, x0) = (by * 8, bx * 8);
    if y0 == 0 || x0 == 0 {
        return None;
    }
    let w = cur.w;
    let mut c9 = [[[0i32; 9]; 9]; 4];
    for (c, plane) in c9.iter_mut().enumerate() {
        for (i, row) in plane.iter_mut().enumerate() {
            let o = (y0 - 1 + i) * w + x0 - 1;
            for (j, v) in row.iter_mut().enumerate() {
                *v = cur.p[c][o + j] as i32;
            }
        }
    }
    let mut cost = 0u32;
    if mode == SPATIAL {
        for i in 1..9 {
            for j in 1..9 {
                for c in 0..4 {
                    let p = if c >= 2 {
                        let d = |a: usize, b: usize| c9[c][a][b] - c9[1][a][b];
                        c9[1][i][j] + med(d(i, j - 1), d(i - 1, j), d(i - 1, j - 1))
                    } else {
                        med(c9[c][i][j - 1], c9[c][i - 1][j], c9[c][i - 1][j - 1])
                    };
                    cost += 32 - wrap(c9[c][i][j] - p).unsigned_abs().leading_zeros();
                }
            }
        }
        return Some(cost);
    }
    let (ry, rx) = (
        (y0 as isize - 1) + (mv.0 >> 1),
        (x0 as isize - 1) + (mv.1 >> 1),
    );
    let (hy, hx) = ((mv.0 & 1) as usize, (mv.1 & 1) as usize);
    if ry < 0 || rx < 0 || ry as usize + 9 + hy > rf.h || rx as usize + 9 + hx > rf.w {
        return None;
    }
    let (ry, rx) = (ry as usize, rx as usize);
    let mut d9 = [[[0i32; 9]; 9]; 4];
    let mut r9 = [[[0i32; 9]; 9]; 4];
    for c in 0..4 {
        let rp = &rf.p[c];
        for i in 0..9 {
            let o = (ry + i) * w + rx;
            for j in 0..9 {
                let r = match (hy, hx) {
                    (0, 0) => rp[o + j] as i32,
                    (0, _) => (rp[o + j] as i32 + rp[o + j + 1] as i32 + 1) >> 1,
                    (_, 0) => (rp[o + j] as i32 + rp[o + j + w] as i32 + 1) >> 1,
                    _ => {
                        (rp[o + j] as i32
                            + rp[o + j + 1] as i32
                            + rp[o + j + w] as i32
                            + rp[o + j + w + 1] as i32
                            + 2)
                            >> 2
                    }
                };
                r9[c][i][j] = r;
                d9[c][i][j] = c9[c][i][j] - r;
            }
        }
    }
    for i in 1..9 {
        for j in 1..9 {
            for c in 0..4 {
                let p = r9[c][i][j] + med(d9[c][i][j - 1], d9[c][i - 1][j], d9[c][i - 1][j - 1]);
                cost += 32 - wrap(c9[c][i][j] - p).unsigned_abs().leading_zeros();
            }
        }
    }
    Some(cost)
}

fn block_cost(
    cur: &Cur,
    rf: &Planes,
    mode: usize,
    mv: (isize, isize),
    by: usize,
    bx: usize,
) -> u32 {
    if cur.y0 == 0 && cur.top == 0 {
        if let Some(c) = fast_cost(cur.buf, rf, mode, mv, by, bx) {
            return c;
        }
    }
    let mut cost = 0u32;
    for y in by * 8..by * 8 + 8 {
        for x in bx * 8..bx * 8 + 8 {
            for c in 0..4 {
                let e =
                    wrap(cur.at(c, y as isize, x as isize) - predict(cur, rf, mode, mv, c, y, x));
                cost += 32 - e.unsigned_abs().leading_zeros();
            }
        }
    }
    cost
}
#[inline]
fn block_diff(
    cur: &Planes,
    rf: &Planes,
    mv: (isize, isize),
    by: usize,
    bx: usize,
    bound: u32,
    f: impl Fn(i32) -> u32,
) -> u32 {
    let w = cur.w as isize;
    let (y0, x0) = ((by * 8) as isize, (bx * 8) as isize);
    let (dy, dx) = (mv.0 >> 1, mv.1 >> 1);
    let (hy, hx) = ((mv.0 & 1) as usize, (mv.1 & 1) as usize);
    let (ry, rx) = (y0 + dy, x0 + dx);
    let mut s = 0u32;
    if ry >= 0
        && rx >= 0
        && ry + 8 + hy as isize <= rf.h as isize
        && rx + 8 + hx as isize <= rf.w as isize
    {
        for c in 0..4 {
            let (cp, rp) = (&cur.p[c], &rf.p[c]);
            for i in 0..8 {
                let co = ((y0 + i) * w + x0) as usize;
                let ro = ((ry + i) * w + rx) as usize;
                let rw = w as usize;
                for j in 0..8 {
                    let r = match (hy, hx) {
                        (0, 0) => rp[ro + j] as i32,
                        (0, _) => (rp[ro + j] as i32 + rp[ro + j + 1] as i32 + 1) >> 1,
                        (_, 0) => (rp[ro + j] as i32 + rp[ro + j + rw] as i32 + 1) >> 1,
                        _ => {
                            (rp[ro + j] as i32
                                + rp[ro + j + 1] as i32
                                + rp[ro + j + rw] as i32
                                + rp[ro + j + rw + 1] as i32
                                + 2)
                                >> 2
                        }
                    };
                    s += f(cp[co + j] as i32 - r);
                }
            }
            if s > bound {
                return s;
            }
        }
        return s;
    }
    for c in 0..4 {
        for y in by * 8..by * 8 + 8 {
            for x in bx * 8..bx * 8 + 8 {
                s += f(cur.p[c][y * cur.w + x] as i32
                    - rf.half(c, 2 * y as isize + mv.0, 2 * x as isize + mv.1));
            }
        }
        if s > bound {
            return s;
        }
    }
    s
}
fn sad(cur: &Planes, rf: &Planes, mv: (isize, isize), by: usize, bx: usize, bound: u32) -> u32 {
    block_diff(cur, rf, mv, by, bx, bound, |d| d.unsigned_abs())
}
fn copy_cost(cur: &Planes, rf: &Planes, mv: (isize, isize), by: usize, bx: usize) -> u32 {
    block_diff(cur, rf, mv, by, bx, u32::MAX, |d| {
        32 - wrap(d).unsigned_abs().leading_zeros()
    })
}

fn mv_pred(
    modes: &[usize],
    mvs: &[(isize, isize)],
    bw: usize,
    b0: usize,
    by: usize,
    bx: usize,
) -> (isize, isize) {
    let bi = by * bw + bx;
    let tm = |m: usize| m == TCOPY || m == TGRAD;
    if bx > 0 && tm(modes[bi - 1]) {
        mvs[bi - 1]
    } else if by > b0 && tm(modes[bi - bw]) {
        mvs[bi - bw]
    } else {
        (0, 0)
    }
}
fn mode_ctx(modes: &[usize], bw: usize, b0: usize, by: usize, bx: usize) -> usize {
    let bi = by * bw + bx;
    (if bx > 0 { modes[bi - 1] } else { 1 }) * 4 + if by > b0 { modes[bi - bw] } else { 1 }
}

type Decision = (usize, (isize, isize), usize);

fn decide(
    cur: &Planes,
    refs: &[&Planes],
    own: usize,
    light: bool,
    prev_mv: &[(isize, isize)],
    by: usize,
) -> Vec<Decision> {
    let (w, bw) = (cur.w, cur.w / 8);
    let rf = refs[0];
    let view = Cur {
        buf: cur,
        y0: 0,
        top: 0,
    };
    let mut row: Vec<Decision> = Vec::with_capacity(bw);
    for bx in 0..bw {
        let same = (by * 8..by * 8 + 8).all(|y| {
            (0..4).all(|c| {
                cur.p[c][y * w + bx * 8..y * w + bx * 8 + 8]
                    == rf.p[c][y * w + bx * 8..y * w + bx * 8 + 8]
            })
        });
        if same {
            row.push((SAME, (0, 0), 0));
            continue;
        }
        let mut cands = vec![(0isize, 0isize), prev_mv[by * bw + bx]];
        if bx > 0 {
            cands.push(row[bx - 1].1);
        }
        let mut best_mode: Option<(u32, usize, (isize, isize), usize)> = None;
        'refs: for (ri, rr) in refs.iter().enumerate() {
            if light && ri > 0 && ri < own {
                continue;
            }
            let mut best = (u32::MAX, (0, 0));
            for &c in &cands {
                let s = sad(cur, rr, c, by, bx, best.0);
                if s < best.0 {
                    best = (s, c);
                }
            }
            if best.0 > 0 {
                for step in if light {
                    &[2isize][..]
                } else {
                    &[2isize, 1][..]
                } {
                    let step = *step;
                    for _ in 0..16 {
                        let (s0, c0) = best;
                        for d in [
                            (0, step),
                            (0, -step),
                            (step, 0),
                            (-step, 0),
                            (step, step),
                            (step, -step),
                            (-step, step),
                            (-step, -step),
                        ] {
                            let c = (c0.0 + d.0, c0.1 + d.1);
                            if c.0.abs() > 128 || c.1.abs() > 128 {
                                continue;
                            }
                            let s = sad(cur, rr, c, by, bx, best.0);
                            if s < best.0 {
                                best = (s, c);
                            }
                        }
                        if best.0 == s0 {
                            break;
                        }
                    }
                }
            }
            let mv = best.1;
            let rbits = if ri == 0 { 0 } else { 1 + ri as u32 };
            let cc = copy_cost(cur, rr, mv, by, bx) + rbits;
            if best_mode.is_none_or(|b| cc < b.0) {
                best_mode = Some((cc, TCOPY, mv, ri));
            }
            if best.0 == 0 || (ri == 0 && cc <= 32 && refs.len() <= own) {
                break 'refs;
            }
        }
        let mut chosen = best_mode.unwrap_or((u32::MAX, SPATIAL, (0, 0), 0));
        if chosen.0 > 0 {
            let (ri, mv) = (chosen.3, chosen.2);
            let rbits = if ri == 0 { 0 } else { 1 + ri as u32 };
            let cg = block_cost(&view, refs[ri], TGRAD, mv, by, bx) + rbits;
            if cg < chosen.0 {
                chosen = (cg, TGRAD, mv, ri);
            }
            let cs = block_cost(&view, rf, SPATIAL, (0, 0), by, bx);
            if cs <= chosen.0 {
                chosen = (cs, SPATIAL, (0, 0), 0);
            }
        }
        row.push((chosen.1, chosen.2, chosen.3));
    }
    row
}

fn reference_list<'a>(
    own: &'a [Planes],
    extra: &[&'a Planes],
    w: usize,
    h: usize,
) -> Vec<&'a Planes> {
    let mut list: Vec<&Planes> = own.iter().collect();
    list.extend(extra.iter().copied().filter(|p| p.w == w && p.h == h));
    list.truncate(MAXREF);
    list
}

pub(crate) struct Encoder {
    bands: Vec<(Models, Enc)>,
    refs: Vec<Planes>,
    prev_mv: Vec<(isize, isize)>,
    light: bool,
}
impl Encoder {
    pub fn new(light: bool) -> Self {
        Encoder {
            bands: (0..BANDS).map(|_| (Models::new(), Enc::new())).collect(),
            refs: Vec::new(),
            prev_mv: Vec::new(),
            light,
        }
    }
    pub fn frame_extra(&mut self, cur: &Planes, extra: &[&Planes]) {
        let (w, h) = (cur.w, cur.h);
        if self.refs.is_empty() || self.refs[0].w != w || self.refs[0].h != h {
            self.refs = vec![Planes::new(w, h)];
        }
        let own = std::mem::take(&mut self.refs);
        let refs = reference_list(&own, extra, w, h);
        let (bw, bh) = (w / 8, h / 8);
        let prev_mv = if self.prev_mv.len() == bw * bh {
            std::mem::take(&mut self.prev_mv)
        } else {
            vec![(0, 0); bw * bh]
        };
        let (n_own, light) = (own.len(), self.light);
        let dec: Vec<Decision> =
            super::mov_source::par_map(bh, |by| decide(cur, &refs, n_own, light, &prev_mv, by))
                .into_iter()
                .flatten()
                .collect();
        self.prev_mv = dec.iter().map(|d| d.1).collect();
        let dec = &dec;
        let refs_ref = &refs;
        par_each(&mut self.bands, |b, (models, enc)| {
            let (b0, b1) = band_rows(bh, b);
            let view = Cur {
                buf: cur,
                y0: 0,
                top: b0 * 8,
            };
            let mut modes = vec![SPATIAL; bw * bh];
            let mut mvs = vec![(0isize, 0isize); bw * bh];
            let rows = (b1 - b0) * 8;
            let mut resmag: [Vec<u8>; 4] = std::array::from_fn(|_| vec![0u8; w * rows]);
            for by in b0..b1 {
                for bx in 0..bw {
                    let bi = by * bw + bx;
                    let (mode, mv, ri) = dec[bi];
                    let ctx = mode_ctx(&modes, bw, b0, by, bx);
                    let b0bit = (mode >> 1) as u32;
                    enc.bit(&mut models.mode[ctx * 4], b0bit, 4);
                    enc.bit(
                        &mut models.mode[ctx * 4 + 1 + b0bit as usize],
                        (mode & 1) as u32,
                        4,
                    );
                    modes[bi] = mode;
                    mvs[bi] = mv;
                    if mode == TCOPY || mode == TGRAD {
                        for k in 0..refs_ref.len() - 1 {
                            enc.bit(&mut models.refi[k], (ri > k) as u32, 4);
                            if ri <= k {
                                break;
                            }
                        }
                        let pred = mv_pred(&modes, &mvs, bw, b0, by, bx);
                        models.mv.encode(enc, 0, (mv.0 - pred.0) as i32);
                        models.mv.encode(enc, 1, (mv.1 - pred.1) as i32);
                    }
                    if mode == SAME {
                        continue;
                    }
                    let t = usize::from(mode != SPATIAL);
                    for y in by * 8..by * 8 + 8 {
                        for x in bx * 8..bx * 8 + 8 {
                            let mut eg = 0i32;
                            for c in 0..4 {
                                let p = predict(&view, refs_ref[ri], mode, mv, c, y, x);
                                let e = wrap(cur.p[c][y * w + x] as i32 - p);
                                let a = neighbour_act(&resmag[c], w, y - b0 * 8, x);
                                models.res.encode(
                                    enc,
                                    ((c * 2 + t) * ACT + act_bucket(a)) * GB + gbucket(c, eg),
                                    e,
                                );
                                resmag[c][(y - b0 * 8) * w + x] = e.unsigned_abs().min(255) as u8;
                                if c == 1 {
                                    eg = e;
                                }
                            }
                        }
                    }
                }
            }
        });
        self.refs = own;
        self.refs.insert(0, cur.clone());
        self.refs.truncate(NREF);
    }
    pub fn finish(self) -> Vec<u8> {
        let streams: Vec<Vec<u8>> = self.bands.into_iter().map(|(_, e)| e.finish()).collect();
        let mut out = Vec::new();
        for s in &streams {
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        }
        for s in streams {
            out.extend(s);
        }
        out
    }
}

pub(crate) struct Decoder<'a> {
    bands: Vec<(Models, Dec<'a>)>,
    refs: Vec<Planes>,
}
impl<'a> Decoder<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self, String> {
        let mut at = 4 * BANDS;
        let mut bands = Vec::with_capacity(BANDS);
        for b in 0..BANDS {
            let len = data
                .get(b * 4..b * 4 + 4)
                .ok_or("truncated source stream")?;
            let len = u32::from_le_bytes(len.try_into().unwrap()) as usize;
            let s = data
                .get(at..at.checked_add(len).ok_or("invalid source stream")?)
                .ok_or("truncated source stream")?;
            bands.push((Models::new(), Dec::new(s)));
            at += len;
        }
        if at != data.len() {
            return Err("invalid source stream".into());
        }
        Ok(Decoder {
            bands,
            refs: Vec::new(),
        })
    }
    pub fn frame_extra(&mut self, w: usize, h: usize, extra: &[&Planes]) -> Planes {
        if self.refs.is_empty() || self.refs[0].w != w || self.refs[0].h != h {
            self.refs = vec![Planes::new(w, h)];
        }
        let own = std::mem::take(&mut self.refs);
        let refs = reference_list(&own, extra, w, h);
        let (bw, bh) = (w / 8, h / 8);
        let refs_ref = &refs;
        let parts: Vec<Planes> = par_each(&mut self.bands, |b, (models, dec)| {
            let (b0, b1) = band_rows(bh, b);
            let rows = (b1 - b0) * 8;
            let mut buf = Planes::new(w, rows.max(1));
            let mut modes = vec![SPATIAL; bw * bh];
            let mut mvs = vec![(0isize, 0isize); bw * bh];
            let mut resmag: [Vec<u8>; 4] = std::array::from_fn(|_| vec![0u8; w * rows]);
            for by in b0..b1 {
                for bx in 0..bw {
                    let bi = by * bw + bx;
                    let ctx = mode_ctx(&modes, bw, b0, by, bx);
                    let hi = dec.bit(&mut models.mode[ctx * 4], 4) as usize;
                    let mode = (hi << 1) | dec.bit(&mut models.mode[ctx * 4 + 1 + hi], 4) as usize;
                    let (mut mv, mut ri) = ((0, 0), 0usize);
                    if mode == TCOPY || mode == TGRAD {
                        for k in 0..refs_ref.len() - 1 {
                            if dec.bit(&mut models.refi[k], 4) == 1 {
                                ri = k + 1;
                            } else {
                                break;
                            }
                        }
                        let pred = mv_pred(&modes, &mvs, bw, b0, by, bx);
                        let dy = models.mv.decode(dec, 0) as isize;
                        let dx = models.mv.decode(dec, 1) as isize;
                        mv = (pred.0 + dy, pred.1 + dx);
                    }
                    modes[bi] = mode;
                    mvs[bi] = mv;
                    if mode == SAME {
                        for y in by * 8..by * 8 + 8 {
                            for c in 0..4 {
                                let (o, r) = ((y - b0 * 8) * w + bx * 8, y * w + bx * 8);
                                buf.p[c][o..o + 8].copy_from_slice(&refs_ref[0].p[c][r..r + 8]);
                            }
                        }
                        continue;
                    }
                    let t = usize::from(mode != SPATIAL);
                    for y in by * 8..by * 8 + 8 {
                        for x in bx * 8..bx * 8 + 8 {
                            let mut eg = 0i32;
                            for c in 0..4 {
                                let p = predict(
                                    &Cur {
                                        buf: &buf,
                                        y0: b0 * 8,
                                        top: b0 * 8,
                                    },
                                    refs_ref[ri],
                                    mode,
                                    mv,
                                    c,
                                    y,
                                    x,
                                );
                                let a = neighbour_act(&resmag[c], w, y - b0 * 8, x);
                                let e = models.res.decode(
                                    dec,
                                    ((c * 2 + t) * ACT + act_bucket(a)) * GB + gbucket(c, eg),
                                );
                                buf.p[c][(y - b0 * 8) * w + x] = ((p + e) & 255) as u8;
                                resmag[c][(y - b0 * 8) * w + x] = e.unsigned_abs().min(255) as u8;
                                if c == 1 {
                                    eg = e;
                                }
                            }
                        }
                    }
                }
            }
            if rows == 0 {
                buf.h = 0;
                buf.p = std::array::from_fn(|_| Vec::new());
            }
            buf
        });
        let mut cur = Planes {
            w,
            h,
            p: std::array::from_fn(|_| Vec::with_capacity(w * h)),
        };
        for part in parts {
            for c in 0..4 {
                cur.p[c].extend_from_slice(&part.p[c]);
            }
        }
        self.refs = own;
        self.refs.insert(0, cur.clone());
        self.refs.truncate(NREF);
        cur
    }
}
