pub(crate) const PROB_INIT: u16 = 2048;

pub(crate) struct Enc {
    low: u64,
    range: u32,
    cache: u8,
    cache_size: u64,
    pub out: Vec<u8>,
}
impl Enc {
    pub fn new() -> Self {
        Enc {
            low: 0,
            range: 0xFFFF_FFFF,
            cache: 0,
            cache_size: 1,
            out: Vec::new(),
        }
    }
    fn shift_low(&mut self) {
        if (self.low as u32) < 0xFF00_0000 || (self.low >> 32) != 0 {
            let carry = (self.low >> 32) as u8;
            let mut temp = self.cache;
            loop {
                self.out.push(temp.wrapping_add(carry));
                temp = 0xFF;
                self.cache_size -= 1;
                if self.cache_size == 0 {
                    break;
                }
            }
            self.cache = ((self.low >> 24) & 0xFF) as u8;
        }
        self.cache_size += 1;
        self.low = (self.low & 0x00FF_FFFF) << 8;
    }
    #[inline]
    pub fn bit(&mut self, p: &mut u16, b: u32, shift: u32) {
        let bound = (self.range >> 12) * (*p as u32);
        if b == 0 {
            self.range = bound;
            *p += (4096 - *p) >> shift;
        } else {
            self.low += bound as u64;
            self.range -= bound;
            *p -= *p >> shift;
        }
        while self.range < (1 << 24) {
            self.range <<= 8;
            self.shift_low();
        }
    }
    pub fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift_low();
        }
        self.out
    }
}

pub(crate) struct Dec<'a> {
    data: &'a [u8],
    pos: usize,
    range: u32,
    code: u32,
}
impl<'a> Dec<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let mut d = Dec {
            data,
            pos: 0,
            range: 0xFFFF_FFFF,
            code: 0,
        };
        for _ in 0..5 {
            d.code = (d.code << 8) | d.byte() as u32;
        }
        d
    }
    #[inline]
    fn byte(&mut self) -> u8 {
        let b = *self.data.get(self.pos).unwrap_or(&0);
        self.pos += 1;
        b
    }
    #[inline]
    pub fn bit(&mut self, p: &mut u16, shift: u32) -> u32 {
        let bound = (self.range >> 12) * (*p as u32);
        let b = if self.code < bound {
            self.range = bound;
            *p += (4096 - *p) >> shift;
            0
        } else {
            self.code -= bound;
            self.range -= bound;
            *p -= *p >> shift;
            1
        };
        while self.range < (1 << 24) {
            self.range <<= 8;
            self.code = (self.code << 8) | self.byte() as u32;
        }
        b
    }
}

pub(crate) struct ResidualModel {
    zero: Vec<u16>,
    sign: Vec<u16>,
    expo: Vec<u16>,
    mant: Vec<u16>,
    ctxs: usize,
}
const EXP_MAX: usize = 16;
impl ResidualModel {
    pub fn new(ctxs: usize) -> Self {
        ResidualModel {
            zero: vec![PROB_INIT; ctxs],
            sign: vec![PROB_INIT; ctxs],
            expo: vec![PROB_INIT; ctxs * EXP_MAX],
            mant: vec![PROB_INIT; ctxs * EXP_MAX * 2],
            ctxs,
        }
    }
    pub fn encode(&mut self, e: &mut Enc, ctx: usize, v: i32) {
        debug_assert!(ctx < self.ctxs);
        if v == 0 {
            e.bit(&mut self.zero[ctx], 0, 4);
            return;
        }
        e.bit(&mut self.zero[ctx], 1, 4);
        e.bit(&mut self.sign[ctx], (v < 0) as u32, 5);
        let m = v.unsigned_abs();
        let k = 31 - m.leading_zeros();
        for i in 0..k as usize {
            e.bit(&mut self.expo[ctx * EXP_MAX + i.min(EXP_MAX - 1)], 1, 4);
        }
        if (k as usize) < EXP_MAX {
            e.bit(&mut self.expo[ctx * EXP_MAX + k as usize], 0, 4);
        }
        if k > 0 {
            let top = (m >> (k - 1)) & 1;
            e.bit(&mut self.mant[(ctx * EXP_MAX + k as usize) * 2], top, 5);
            for i in (0..k - 1).rev() {
                e.bit(
                    &mut self.mant[(ctx * EXP_MAX + k as usize) * 2 + 1],
                    (m >> i) & 1,
                    6,
                );
            }
        }
    }
    pub fn decode(&mut self, d: &mut Dec, ctx: usize) -> i32 {
        if d.bit(&mut self.zero[ctx], 4) == 0 {
            return 0;
        }
        let neg = d.bit(&mut self.sign[ctx], 5) == 1;
        let mut k = 0usize;
        while k < EXP_MAX && d.bit(&mut self.expo[ctx * EXP_MAX + k.min(EXP_MAX - 1)], 4) == 1 {
            k += 1;
        }
        let k = k.min(EXP_MAX - 1);
        let mut m: u32 = 1;
        if k > 0 {
            let top = d.bit(&mut self.mant[(ctx * EXP_MAX + k) * 2], 5);
            m = (m << 1) | top;
            for _ in 0..k - 1 {
                let b = d.bit(&mut self.mant[(ctx * EXP_MAX + k) * 2 + 1], 6);
                m = (m << 1) | b;
            }
        }
        if neg {
            -(m as i32)
        } else {
            m as i32
        }
    }
}
