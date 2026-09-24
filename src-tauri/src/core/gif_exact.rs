use std::collections::HashMap;

const MAGIC: &[u8; 8] = b"SGIF\x01\0\0\0";
const MAX_BYTES: usize = 512 * 1024 * 1024;
const MAX_ORIGINAL_BYTES: usize = 256 * 1024 * 1024;
const MAX_FRAME_PIXELS: usize = 16 * 1024 * 1024;
const TABLE: usize = 4096;

fn put_var(mut n: usize, out: &mut Vec<u8>) {
    while n >= 128 {
        out.push((n as u8) | 128);
        n >>= 7;
    }
    out.push(n as u8);
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).ok_or("GIF length overflow")?;
        let bytes = self.bytes.get(self.pos..end).ok_or("Truncated GIF data")?;
        self.pos = end;
        Ok(bytes)
    }

    fn var(&mut self) -> Result<usize, String> {
        let mut n = 0usize;
        for shift in (0..usize::BITS).step_by(7) {
            let b = self.byte()?;
            let part = (b & 127) as usize;
            if part > (usize::MAX >> shift) {
                return Err("GIF variable integer overflow".into());
            }
            n |= part << shift;
            if b & 128 == 0 {
                return Ok(n);
            }
        }
        Err("GIF variable integer overflow".into())
    }

    fn blob(&mut self) -> Result<&'a [u8], String> {
        let n = self.var()?;
        self.take(n)
    }
}

fn put_blob(bytes: &[u8], out: &mut Vec<u8>) {
    put_var(bytes.len(), out);
    out.extend_from_slice(bytes);
}

struct Dict {
    clear: u16,
    next: u16,
    width: u8,
    minimum: u8,
    prev: Option<u16>,
    prefix: [u16; TABLE],
    suffix: [u8; TABLE],
    first: [u8; TABLE],
    length: [u16; TABLE],
    edges: HashMap<u32, u16>,
}

impl Dict {
    fn new(minimum: u8) -> Result<Self, String> {
        if !(1..=8).contains(&minimum) {
            return Err("Unsupported GIF LZW minimum code size".into());
        }
        let clear = 1u16 << minimum;
        let mut d = Self {
            clear,
            next: clear + 2,
            width: minimum + 1,
            minimum,
            prev: None,
            prefix: [0; TABLE],
            suffix: [0; TABLE],
            first: [0; TABLE],
            length: [0; TABLE],
            edges: HashMap::with_capacity(TABLE),
        };
        for i in 0..clear as usize {
            d.suffix[i] = i as u8;
            d.first[i] = i as u8;
            d.length[i] = 1;
        }
        Ok(d)
    }

    fn reset(&mut self) {
        self.next = self.clear + 2;
        self.width = self.minimum + 1;
        self.prev = None;
        self.edges.clear();
    }

    fn phrase_info(&self, code: u16) -> Result<(usize, u8), String> {
        if code < self.clear || (code > self.clear + 1 && code < self.next) {
            return Ok((
                self.length[code as usize] as usize,
                self.first[code as usize],
            ));
        }
        if code == self.next && code < TABLE as u16 {
            if let Some(prev) = self.prev {
                return Ok((
                    self.length[prev as usize] as usize + 1,
                    self.first[prev as usize],
                ));
            }
        }
        Err("Invalid GIF LZW phrase code".into())
    }

    fn apply(&mut self, code: u16) -> Result<usize, String> {
        let (len, first) = self.phrase_info(code)?;
        if len == 0 || len > TABLE {
            return Err("Invalid GIF LZW phrase length".into());
        }
        if let Some(prev) = self.prev {
            if self.next < TABLE as u16 {
                let n = self.next as usize;
                self.prefix[n] = prev;
                self.suffix[n] = first;
                self.first[n] = self.first[prev as usize];
                self.length[n] = self.length[prev as usize] + 1;
                self.edges
                    .insert(((prev as u32) << 8) | first as u32, self.next);
                self.next += 1;
                if self.next == (1u16 << self.width) && self.width < 12 {
                    self.width += 1;
                }
            }
        }
        self.prev = Some(code);
        Ok(len)
    }

    fn expand<'b>(&self, mut code: u16, scratch: &'b mut [u8; TABLE]) -> Result<&'b [u8], String> {
        let length = self.length[code as usize] as usize;
        if length == 0 || length > TABLE {
            return Err("Invalid GIF LZW dictionary entry".into());
        }
        let mut pos = length;
        while code >= self.clear {
            if pos == 0 || code >= self.next || code <= self.clear + 1 {
                return Err("Invalid GIF LZW dictionary chain".into());
            }
            pos -= 1;
            scratch[pos] = self.suffix[code as usize];
            code = self.prefix[code as usize];
        }
        if pos != 1 {
            return Err("Invalid GIF LZW dictionary length".into());
        }
        scratch[0] = code as u8;
        Ok(&scratch[..length])
    }

    fn predict(&self, pixels: &[u8], pos: usize, event: usize) -> u16 {
        if event == 0 {
            return self.clear;
        }
        if pos == pixels.len() {
            return self.clear + 1;
        }
        if self.next == TABLE as u16 {
            return self.clear;
        }
        let mut code = pixels[pos] as u16;
        let mut end = pos + 1;
        while end < pixels.len() {
            match self.edges.get(&(((code as u32) << 8) | pixels[end] as u32)) {
                Some(&next) => {
                    code = next;
                    end += 1;
                }
                None => break,
            }
        }
        if let Some(prev) = self.prev {
            let n = self.length[prev as usize] as usize;
            if n + 1 > end - pos
                && pos >= n
                && pos + n < pixels.len()
                && pixels[pos + n] == self.first[prev as usize]
                && pixels[pos..pos + n] == pixels[pos - n..pos]
            {
                return self.next;
            }
        }
        code
    }
}

fn read_code(bytes: &[u8], bit: &mut usize, width: u8) -> Result<u16, String> {
    if *bit + width as usize > bytes.len() * 8 {
        return Err("Missing GIF LZW end code".into());
    }
    let at = *bit / 8;
    let mut word = 0u32;
    for i in 0..3 {
        word |= (bytes.get(at + i).copied().unwrap_or(0) as u32) << (i * 8);
    }
    let code = ((word >> (*bit & 7)) & ((1 << width) - 1)) as u16;
    *bit += width as usize;
    Ok(code)
}

fn write_code(out: &mut Vec<u8>, bit: &mut usize, width: u8, code: u16) -> Result<(), String> {
    if code >= (1u16 << width) {
        return Err("GIF code does not fit its bit width".into());
    }
    let end = *bit + width as usize;
    if end.div_ceil(8) > MAX_BYTES {
        return Err("GIF LZW output exceeds limit".into());
    }
    out.resize(end.div_ceil(8), 0);
    let word = (code as u32) << (*bit & 7);
    for i in 0..3 {
        if let Some(byte) = out.get_mut(*bit / 8 + i) {
            *byte |= (word >> (i * 8)) as u8;
        }
    }
    *bit = end;
    Ok(())
}

struct Image {
    pixels: Vec<u8>,
    exceptions: Vec<u8>,
    events: usize,
    tail: Vec<u8>,
}

fn unpack_lzw(bytes: &[u8], minimum: u8, expected: usize) -> Result<Image, String> {
    if expected == 0 || expected > MAX_FRAME_PIXELS {
        return Err("GIF frame exceeds pixel limit".into());
    }
    let mut dict = Dict::new(minimum)?;
    let mut pixels = Vec::with_capacity(expected);
    let mut scratch = [0u8; TABLE];
    let mut bit = 0;
    let mut events = 0usize;
    loop {
        let code = read_code(bytes, &mut bit, dict.width)?;
        events += 1;
        if code == dict.clear {
            dict.reset();
        } else if code == dict.clear + 1 {
            break;
        } else {
            let n = dict.apply(code)?;
            if n > expected - pixels.len() {
                return Err("GIF LZW pixels exceed frame dimensions".into());
            }
            pixels.extend_from_slice(dict.expand(code, &mut scratch)?);
        }
    }
    if pixels.len() != expected {
        return Err("GIF LZW pixels do not match frame dimensions".into());
    }
    let mut tail = bytes[bit / 8..].to_vec();
    if bit & 7 != 0 {
        tail[0] &= !((1u8 << (bit & 7)) - 1);
    }
    dict.reset();
    bit = 0;
    let mut pos = 0usize;
    let mut exceptions = Vec::new();
    let mut last_exception = 0usize;
    for event in 0..events {
        let code = read_code(bytes, &mut bit, dict.width)?;
        if dict.predict(&pixels, pos, event) != code {
            put_var(event - last_exception, &mut exceptions);
            put_var(code as usize, &mut exceptions);
            last_exception = event + 1;
        }
        if code == dict.clear {
            dict.reset();
        } else if code != dict.clear + 1 {
            pos += dict.apply(code)?;
        }
    }
    Ok(Image {
        pixels,
        exceptions,
        events,
        tail,
    })
}

fn pack_lzw(
    minimum: u8,
    pixels: &[u8],
    exceptions: &[u8],
    events: usize,
    tail: &[u8],
    limit: usize,
) -> Result<Vec<u8>, String> {
    if pixels.is_empty()
        || pixels.len() > MAX_FRAME_PIXELS
        || events == 0
        || events > limit.saturating_mul(8)
    {
        return Err("GIF image record exceeds limits".into());
    }
    let mut dict = Dict::new(minimum)?;
    if pixels.iter().any(|&p| p as u16 >= dict.clear) {
        return Err("GIF pixel exceeds LZW alphabet".into());
    }
    let mut changes = Reader {
        bytes: exceptions,
        pos: 0,
    };
    let mut next_change = if changes.pos < changes.bytes.len() {
        Some(changes.var()?)
    } else {
        None
    };
    let mut output = Vec::with_capacity(limit.min(1024 * 1024));
    let mut bit = 0usize;
    let mut pos = 0usize;
    let mut ended = false;
    let mut scratch = [0u8; TABLE];
    for event in 0..events {
        let code = if next_change == Some(event) {
            let code = changes.var()?;
            if code >= TABLE {
                return Err("Invalid GIF exception code".into());
            }
            next_change = if changes.pos < changes.bytes.len() {
                let gap = changes.var()?;
                Some(
                    event
                        .checked_add(1)
                        .and_then(|n| n.checked_add(gap))
                        .ok_or("Invalid GIF exception position")?,
                )
            } else {
                None
            };
            code as u16
        } else {
            dict.predict(pixels, pos, event)
        };
        write_code(&mut output, &mut bit, dict.width, code)?;
        if output.len() > limit {
            return Err("GIF LZW record exceeds declared length".into());
        }
        if code == dict.clear {
            dict.reset();
        } else if code == dict.clear + 1 {
            if event + 1 != events || pos != pixels.len() {
                return Err("Unexpected GIF LZW end code".into());
            }
            ended = true;
        } else {
            let n = dict.apply(code)?;
            let end = pos.checked_add(n).ok_or("GIF pixel position overflow")?;
            if pixels.get(pos..end) != Some(dict.expand(code, &mut scratch)?) {
                return Err("GIF exception does not match decoded pixels".into());
            }
            pos = end;
        }
    }
    if !ended || next_change.is_some() || changes.pos != changes.bytes.len() {
        return Err("Incomplete GIF LZW decisions".into());
    }
    if tail.len() != limit.saturating_sub(bit / 8) {
        return Err("GIF LZW tail length mismatch".into());
    }
    if bit & 7 != 0 {
        if tail.is_empty() || tail[0] & ((1u8 << (bit & 7)) - 1) != 0 {
            return Err("Invalid GIF LZW padding bits".into());
        }
        *output.last_mut().ok_or("Missing GIF LZW bytes")? |= tail[0];
        output.extend_from_slice(&tail[1..]);
    } else {
        output.extend_from_slice(tail);
    }
    if output.len() != limit {
        return Err("GIF LZW length mismatch".into());
    }
    Ok(output)
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

fn encode_inner(raw: &[u8]) -> Result<Vec<u8>, String> {
    if raw.len() > MAX_ORIGINAL_BYTES
        || raw.len() < 13
        || (&raw[..6] != b"GIF87a" && &raw[..6] != b"GIF89a")
    {
        return Err("Unsupported GIF header or size".into());
    }
    let mut input = Reader {
        bytes: raw,
        pos: 13,
    };
    if raw[10] & 128 != 0 {
        input.take(3usize << ((raw[10] & 7) + 1))?;
    }
    let mut output = MAGIC.to_vec();
    put_var(raw.len(), &mut output);
    let mut saved = 0usize;
    let mut frames = 0usize;
    loop {
        match input.byte()? {
            0x3b => break,
            0x21 => {
                input.byte()?;
                skip_blocks(&mut input)?;
            }
            0x2c => {
                let descriptor = input.take(9)?;
                let width = u16::from_le_bytes([descriptor[4], descriptor[5]]) as usize;
                let height = u16::from_le_bytes([descriptor[6], descriptor[7]]) as usize;
                if descriptor[8] & 128 != 0 {
                    input.take(3usize << ((descriptor[8] & 7) + 1))?;
                }
                let minimum = input.byte()?;
                if input.pos - saved > MAX_BYTES.saturating_sub(output.len() + 16) {
                    return Err("GIF transformed data exceeds limit".into());
                }
                output.push(0);
                put_blob(&raw[saved..input.pos], &mut output);
                let mut lengths = Vec::new();
                let mut lzw = Vec::new();
                loop {
                    let n = input.byte()?;
                    if n == 0 {
                        break;
                    }
                    lengths.push(n);
                    lzw.extend_from_slice(input.take(n as usize)?);
                }
                let image = unpack_lzw(&lzw, minimum, width * height)?;
                let added = lengths
                    .len()
                    .checked_add(image.pixels.len())
                    .and_then(|n| n.checked_add(image.exceptions.len()))
                    .and_then(|n| n.checked_add(image.tail.len()))
                    .ok_or("GIF transform size overflow")?;
                if added > MAX_BYTES.saturating_sub(output.len() + 64) {
                    return Err("GIF transformed data exceeds limit".into());
                }
                output.push(1);
                output.push(minimum);
                put_blob(&lengths, &mut output);
                put_blob(&image.pixels, &mut output);
                put_var(image.events, &mut output);
                put_blob(&image.exceptions, &mut output);
                put_blob(&image.tail, &mut output);
                saved = input.pos;
                frames += 1;
            }
            _ => return Err("Unsupported GIF block (store original bytes instead)".into()),
        }
    }
    if frames == 0 {
        return Err("GIF contains no image frames".into());
    }
    if raw.len() - saved > MAX_BYTES.saturating_sub(output.len() + 16) {
        return Err("GIF transformed data exceeds limit".into());
    }
    output.push(0);
    put_blob(&raw[saved..], &mut output);
    output.push(255);
    if output.len() > MAX_BYTES {
        return Err("GIF transformed data exceeds limit".into());
    }
    Ok(output)
}

pub fn encode(raw: &[u8]) -> Result<Vec<u8>, String> {
    let encoded = encode_inner(raw)?;
    if decode(&encoded)? != raw {
        return Err("GIF byte-exact transform self-check failed".into());
    }
    Ok(encoded)
}

pub fn decode(encoded: &[u8]) -> Result<Vec<u8>, String> {
    if encoded.len() > MAX_BYTES {
        return Err("GIF transformed data exceeds limit".into());
    }
    let mut input = Reader {
        bytes: encoded,
        pos: 0,
    };
    if input.take(MAGIC.len())? != MAGIC {
        return Err("Invalid GIF transform signature".into());
    }
    let expected = input.var()?;
    if expected > MAX_ORIGINAL_BYTES {
        return Err("GIF original size exceeds limit".into());
    }
    let mut output = Vec::with_capacity(expected.min(1024 * 1024));
    loop {
        match input.byte()? {
            0 => {
                let bytes = input.blob()?;
                if bytes.len() > expected.saturating_sub(output.len()) {
                    return Err("GIF literal exceeds declared length".into());
                }
                output.extend_from_slice(bytes);
            }
            1 => {
                let minimum = input.byte()?;
                let lengths = input.blob()?;
                if lengths.is_empty() || lengths.contains(&0) {
                    return Err("Invalid GIF sub-block boundaries".into());
                }
                let packed_len = lengths
                    .iter()
                    .try_fold(0usize, |n, &b| n.checked_add(b as usize))
                    .ok_or("GIF block length overflow")?;
                let framed_len = packed_len
                    .checked_add(lengths.len())
                    .and_then(|n| n.checked_add(1))
                    .ok_or("GIF block length overflow")?;
                if framed_len > expected.saturating_sub(output.len()) {
                    return Err("GIF image exceeds declared length".into());
                }
                let pixels = input.blob()?;
                let events = input.var()?;
                let exceptions = input.blob()?;
                let tail = input.blob()?;
                let packed = pack_lzw(minimum, pixels, exceptions, events, tail, packed_len)?;
                let mut pos = 0usize;
                for &n in lengths {
                    output.push(n);
                    output.extend_from_slice(&packed[pos..pos + n as usize]);
                    pos += n as usize;
                }
                output.push(0);
            }
            255 => break,
            _ => return Err("Invalid GIF transform record".into()),
        }
    }
    if input.pos != encoded.len() || output.len() != expected {
        return Err("GIF transform total length mismatch".into());
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code_stream(minimum: u8, codes: &[u16], padding: u8) -> Vec<u8> {
        let mut dict = Dict::new(minimum).unwrap();
        let mut packed = Vec::new();
        let mut bit = 0;
        for &code in codes {
            write_code(&mut packed, &mut bit, dict.width, code).unwrap();
            if code == dict.clear {
                dict.reset();
            } else if code != dict.clear + 1 {
                dict.apply(code).unwrap();
            }
        }
        if bit & 7 != 0 {
            *packed.last_mut().unwrap() |= padding & !((1u8 << (bit & 7)) - 1);
        }
        packed
    }

    fn gif(width: u16, height: u16, minimum: u8, packed: &[u8], block_size: usize) -> Vec<u8> {
        let mut raw =
            b"GIF89a\x01\0\x01\0\x80\0\0\0\0\0\xff\xff\xff\x21\xfe\x03ABC\0\x2c\0\0\0\0".to_vec();
        raw.extend_from_slice(&width.to_le_bytes());
        raw.extend_from_slice(&height.to_le_bytes());
        raw.extend_from_slice(&[0x40, minimum]);
        for chunk in packed.chunks(block_size) {
            raw.push(chunk.len() as u8);
            raw.extend_from_slice(chunk);
        }
        raw.extend_from_slice(&[0, 0x3b, 0xde, 0xad]);
        raw
    }

    #[test]
    fn exact_clear_codes_subblocks_padding_and_trailer() {
        let mut packed = code_stream(2, &[4, 0, 1, 4, 4, 1, 0, 5], 255);
        packed.extend_from_slice(&[0xef, 0xbe]);
        let raw = gif(2, 2, 2, &packed, 1);
        assert_eq!(decode(&encode(&raw).unwrap()).unwrap(), raw);
    }

    #[test]
    fn exact_non_greedy_dictionary_and_kwkwk() {
        for codes in [&[4, 0, 6, 0, 0, 5][..], &[0, 0, 0, 0, 0, 5][..]] {
            let packed = code_stream(2, codes, 255);
            let raw = gif(5, 1, 2, &packed, 2);
            assert_eq!(decode(&encode(&raw).unwrap()).unwrap(), raw);
        }
    }

    #[test]
    fn exact_code_width_growth_and_frozen_full_dictionary() {
        let mut codes = vec![256];
        codes.extend((0..20_000).map(|n| (n % 256) as u16));
        codes.push(257);
        let packed = code_stream(8, &codes, 255);
        let raw = gif(200, 100, 8, &packed, 251);
        assert_eq!(decode(&encode(&raw).unwrap()).unwrap(), raw);
    }

    #[test]
    fn malformed_inputs_and_resource_limits_are_errors() {
        for raw in [&b""[..], &b"GIF89a"[..], &b"GIF89a\0\0\0\0\0\0\0\x3b"[..]] {
            assert!(encode(raw).is_err());
        }
        let packed = code_stream(2, &[4, 0, 5], 0);
        assert!(encode(&gif(65_535, 65_535, 2, &packed, 255)).is_err());
        let encoded = encode(&gif(1, 1, 2, &packed, 255)).unwrap();
        for end in 0..encoded.len() {
            assert!(decode(&encoded[..end]).is_err());
        }
        let mut oversized = MAGIC.to_vec();
        put_var(MAX_BYTES + 1, &mut oversized);
        assert!(decode(&oversized).is_err());
    }

    #[test]
    fn arbitrary_mutations_never_panic() {
        let packed = code_stream(2, &[4, 0, 1, 0, 1, 5], 255);
        let raw = gif(2, 2, 2, &packed, 1);
        let encoded = encode(&raw).unwrap();
        for pos in 0..encoded.len() {
            for value in [0, 1, 127, 128, 255] {
                let mut changed = encoded.clone();
                changed[pos] = value;
                let _ = decode(&changed);
            }
        }
        for pos in 0..raw.len() {
            let mut changed = raw.clone();
            changed[pos] ^= 255;
            let _ = encode(&changed);
        }
    }

    #[test]
    fn randomized_valid_lzw_decisions_are_byte_exact() {
        let mut random = 0x7ac5_38d1u32;
        for minimum in 2..=8 {
            for _ in 0..12 {
                let mut dict = Dict::new(minimum).unwrap();
                let mut packed = Vec::new();
                let mut bit = 0;
                let mut pixels = 0usize;
                for _ in 0..2000 {
                    random ^= random << 13;
                    random ^= random >> 17;
                    random ^= random << 5;
                    let code = if random.is_multiple_of(29) {
                        dict.clear
                    } else if random.is_multiple_of(7) && dict.prev.is_some() && dict.next < 4096 {
                        dict.next
                    } else if random.is_multiple_of(3) && dict.next > dict.clear + 2 {
                        dict.clear + 2 + (random % (dict.next - dict.clear - 2) as u32) as u16
                    } else {
                        (random % dict.clear as u32) as u16
                    };
                    write_code(&mut packed, &mut bit, dict.width, code).unwrap();
                    if code == dict.clear {
                        dict.reset();
                    } else {
                        pixels += dict.apply(code).unwrap();
                    }
                }
                write_code(&mut packed, &mut bit, dict.width, dict.clear + 1).unwrap();
                assert!(pixels > 0 && pixels <= u16::MAX as usize);
                let raw = gif(pixels as u16, 1, minimum, &packed, 79);
                assert_eq!(decode(&encode(&raw).unwrap()).unwrap(), raw);
            }
        }
    }
}
