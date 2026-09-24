use std::io::{Read, Write};

const MAGIC: &[u8; 8] = b"SPGPM\x01\0\0";
const HEADER: usize = 92;
const LIMIT: usize = 512 << 20;
const MEMORY_LOG: u8 = 25;
const ORDER: u8 = 8;
const CHUNK: usize = 64 << 10;

fn report(
    progress: &mut dyn FnMut(u64, u64) -> bool,
    done: usize,
    total: usize,
) -> Result<(), String> {
    if progress(done as u64, total as u64) {
        Ok(())
    } else {
        Err("error.cancelled".into())
    }
}

struct BoundedOutput(Vec<u8>);
impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > (LIMIT - HEADER).saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("GIF model output exceeds limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn encode(raw: &[u8]) -> Result<Vec<u8>, String> {
    encode_transformed(&super::gif_exact::encode(raw)?)
}

pub fn encode_transformed(sgif: &[u8]) -> Result<Vec<u8>, String> {
    encode_transformed_with_progress(sgif, &mut |_, _| true)
}

pub fn encode_transformed_with_progress(
    sgif: &[u8],
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Vec<u8>, String> {
    if sgif.len() > LIMIT || !sgif.starts_with(b"SGIF\x01\0\0\0") {
        return Err("Invalid or excessive SGIF model input".into());
    }
    let total = sgif.len() * 2;
    report(progress, 0, total)?;
    let mut encoder =
        ppmd_rust::Ppmd7Encoder::new(BoundedOutput(Vec::new()), ORDER as u32, 1u32 << MEMORY_LOG)
            .map_err(|e| e.to_string())?;
    let mut processed = 0;
    for chunk in sgif.chunks(CHUNK) {
        encoder.write_all(chunk).map_err(|e| e.to_string())?;
        processed += chunk.len();
        report(progress, processed, total)?;
    }
    let packed = encoder.finish(true).map_err(|e| e.to_string())?.0;
    let mut output = Vec::with_capacity(HEADER + packed.len());
    output.extend_from_slice(MAGIC);
    output.extend_from_slice(&(sgif.len() as u64).to_le_bytes());
    output.extend_from_slice(&[ORDER, MEMORY_LOG, 0, 0]);
    output.extend_from_slice(&(packed.len() as u64).to_le_bytes());
    output.extend_from_slice(blake3::hash(sgif).as_bytes());
    output.extend_from_slice(blake3::hash(&packed).as_bytes());
    output.extend_from_slice(&packed);
    if decode_transform_with_progress(&output, &mut |done, _| {
        progress(sgif.len() as u64 + done, total as u64)
    })? != sgif
    {
        return Err("GIF probability-model self-check failed".into());
    }
    Ok(output)
}

fn decode_transform(encoded: &[u8]) -> Result<Vec<u8>, String> {
    decode_transform_with_progress(encoded, &mut |_, _| true)
}

fn decode_transform_with_progress(
    encoded: &[u8],
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Vec<u8>, String> {
    if encoded.len() < HEADER || encoded.len() > LIMIT || &encoded[..8] != MAGIC {
        return Err("Invalid GIF probability-model header".into());
    }
    let plain_len = u64::from_le_bytes(encoded[8..16].try_into().unwrap());
    let order = encoded[16];
    let memory_log = encoded[17];
    let packed_len = u64::from_le_bytes(encoded[20..28].try_into().unwrap());
    if plain_len < 9
        || plain_len > LIMIT as u64
        || packed_len != (encoded.len() - HEADER) as u64
        || !(2..=16).contains(&order)
        || memory_log != MEMORY_LOG
        || encoded[18..20] != [0, 0]
    {
        return Err("GIF probability-model parameters exceed limits".into());
    }
    let packed = &encoded[HEADER..];
    if blake3::hash(packed).as_bytes() != &encoded[60..92] {
        return Err("GIF probability-model payload checksum mismatch".into());
    }
    report(progress, 0, plain_len as usize)?;
    let mut decoder = ppmd_rust::Ppmd7Decoder::new(packed, order as u32, 1u32 << memory_log)
        .map_err(|e| e.to_string())?;
    let mut sgif = Vec::new();
    let mut chunk = [0u8; CHUNK];
    while sgif.len() < plain_len as usize {
        let take = chunk.len().min(plain_len as usize - sgif.len());
        let n = decoder
            .read(&mut chunk[..take])
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("Truncated GIF probability-model output".into());
        }
        sgif.extend_from_slice(&chunk[..n]);
        report(progress, sgif.len(), plain_len as usize)?;
    }
    if decoder.read(&mut chunk[..1]).map_err(|e| e.to_string())? != 0 {
        return Err("GIF probability-model output exceeds declared length".into());
    }
    if sgif.len() as u64 != plain_len
        || !decoder.into_inner().is_empty()
        || blake3::hash(&sgif).as_bytes() != &encoded[28..60]
    {
        return Err("GIF probability-model length or checksum mismatch".into());
    }
    Ok(sgif)
}

pub fn decode(encoded: &[u8]) -> Result<Vec<u8>, String> {
    super::gif_exact::decode(&decode_transform(encoded)?)
}

pub fn decode_with_progress(
    encoded: &[u8],
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Vec<u8>, String> {
    super::gif_exact::decode(&decode_transform_with_progress(encoded, progress)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        b"GIF89a\x02\0\x02\0\x80\0\0\0\0\0\xff\xff\xff\x2c\0\0\0\0\x02\0\x02\0\0\x02\x03\x44\x10\x05\0\x3b".to_vec()
    }

    #[test]
    fn gif_model_preserves_original_bytes() {
        let original = fixture();
        let encoded = encode(&original).unwrap();
        assert_eq!(decode(&encoded).unwrap(), original);
    }

    #[test]
    fn malformed_and_truncated_models_fail_before_publication() {
        let encoded = encode(&fixture()).unwrap();
        for end in 0..encoded.len() {
            assert!(decode(&encoded[..end]).is_err());
        }
        for pos in [8, 16, 17, 18, 20, 28, 60, HEADER] {
            let mut changed = encoded.clone();
            changed[pos] ^= 255;
            assert!(decode(&changed).is_err());
        }
        let mut extra = encoded.clone();
        extra.push(0);
        assert!(decode(&extra).is_err());
    }

    #[test]
    fn checksummed_extra_coded_bytes_are_not_silently_accepted() {
        let mut encoded = encode(&fixture()).unwrap();
        encoded.extend_from_slice(&[0; 8]);
        let length = (encoded.len() - HEADER) as u64;
        encoded[20..28].copy_from_slice(&length.to_le_bytes());
        let hash = blake3::hash(&encoded[HEADER..]);
        encoded[60..92].copy_from_slice(hash.as_bytes());
        assert!(decode(&encoded).is_err());
    }

    #[test]
    fn progress_is_bounded_and_cancellable() {
        let sgif = super::super::gif_exact::encode(&fixture()).unwrap();
        assert!(encode_transformed_with_progress(&sgif, &mut |_, _| false).is_err());
        let mut last = 0;
        let model = encode_transformed_with_progress(&sgif, &mut |done, total| {
            assert!(done >= last && done <= total);
            last = done;
            true
        })
        .unwrap();
        assert_eq!(last, sgif.len() as u64 * 2);
        assert!(decode_with_progress(&model, &mut |_, _| false).is_err());
        assert_eq!(
            decode_with_progress(&model, &mut |_, _| true).unwrap(),
            fixture()
        );
    }
}
