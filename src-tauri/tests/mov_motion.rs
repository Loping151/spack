use spack::core::{
    container::{self, Entry, Manifest, Method, Progress},
    mov_motion,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

struct Fixture {
    raw: Vec<u8>,
    record: Vec<u8>,
}
fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let one = mov_fixture();
        let frame = &one[8..];
        let mut raw = ((8 + frame.len() * 2 + 13) as u32).to_be_bytes().to_vec();
        raw.extend_from_slice(b"mdat");
        raw.extend_from_slice(frame);
        raw.extend_from_slice(&[0; 13]);
        raw.extend_from_slice(frame);
        raw.extend_from_slice(&19u32.to_be_bytes());
        raw.extend_from_slice(b"free");
        raw.extend_from_slice(b"exact-tail!");
        let mut last = 0;
        let record = mov_motion::encode_with_progress(&raw, &mut |done, total| {
            assert!(done >= last && done <= total);
            last = done;
            true
        })
        .unwrap();
        assert_eq!(last, 1_000_000);
        assert_eq!(
            mov_motion::decode_with_progress(&record, &mut |_, _| true).unwrap(),
            raw
        );
        Fixture { raw, record }
    })
}
fn u64at(b: &[u8], at: usize) -> usize {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap()) as usize
}
fn side_of(b: &[u8]) -> Vec<u8> {
    let start = 88 + u64at(b, 24) + u64at(b, 32);
    zstd::bulk::decompress(&b[start..], u64at(b, 48)).unwrap()
}
fn with_side(record: &[u8], side: &[u8]) -> Vec<u8> {
    let start = 88 + u64at(record, 24) + u64at(record, 32);
    let mut b = record[..start].to_vec();
    let coded = zstd::bulk::compress(side, 1).unwrap();
    b[40..48].copy_from_slice(&(coded.len() as u64).to_le_bytes());
    b[48..56].copy_from_slice(&(side.len() as u64).to_le_bytes());
    b.extend(coded);
    b
}
fn reject(b: &[u8]) -> String {
    mov_motion::decode_with_progress(b, &mut |_, _| true).unwrap_err()
}

#[test]
fn invalid_header_and_descriptor_boundaries_fail_before_reconstruction() {
    let f = fixture();
    for n in [0, 7, 87, 88, f.record.len() - 1] {
        assert!(!reject(&f.record[..n]).is_empty())
    }
    for offset in [8, 24, 32, 40, 48] {
        let mut b = f.record.clone();
        b[offset..offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(!reject(&b).is_empty());
    }
    for offset in [16, 20] {
        let mut b = f.record.clone();
        b[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(!reject(&b).is_empty());
    }
    let side = side_of(&f.record);
    let frames = u32::from_le_bytes(f.record[16..20].try_into().unwrap()) as usize;
    let descriptors = frames * 192;
    for variant in 0..9 {
        let mut s = side.clone();
        let d = &mut s[descriptors..descriptors + 24];
        match variant {
            0 => d[..4].copy_from_slice(&u32::MAX.to_le_bytes()),
            1 => d[4..6].copy_from_slice(&0u16.to_le_bytes()),
            2 => d[6..8].copy_from_slice(&(frames as u16).to_le_bytes()),
            3 => d[8..10].copy_from_slice(&630u16.to_le_bytes()),
            4 => d[10] = 3,
            5 => d[11] = 1,
            6 => d[12..16].copy_from_slice(&0u32.to_le_bytes()),
            7 => {
                let key = d[6..11].to_vec();
                d[18..23].copy_from_slice(&key)
            }
            _ => d[4..6].copy_from_slice(&u16::MAX.to_le_bytes()),
        }
        let err = reject(&with_side(&f.record, &s));
        assert!(err.contains("descriptor"), "variant {variant}: {err}");
    }
}

#[test]
fn invalid_quantization_and_motion_vectors_are_rejected() {
    let f = fixture();
    let side = side_of(&f.record);
    let frames = u32::from_le_bytes(f.record[16..20].try_into().unwrap()) as usize;
    let count = u32::from_le_bytes(f.record[20..24].try_into().unwrap()) as usize;
    let blocks = frames * 126 * 126;
    let modes = frames * 192 + count * 12;
    for variant in 0..6 {
        let mut s = side.clone();
        match variant {
            0 => s[0] = 0,
            1 => s[modes] = 1,
            2 => s[modes] = 3,
            3 => s[modes + blocks..modes + blocks + 2].copy_from_slice(&1i16.to_le_bytes()),
            4 => s[modes + blocks..modes + blocks + 2].copy_from_slice(&i16::MIN.to_le_bytes()),
            _ => {
                let i = 126 * 126;
                s[modes + i] = 2;
                s[modes + blocks + 2 * i..modes + blocks + 2 * i + 2]
                    .copy_from_slice(&19i16.to_le_bytes())
            }
        }
        let err = reject(&with_side(&f.record, &s));
        assert!(
            err.contains(if variant == 0 {
                "quantization"
            } else {
                "mode or vector"
            }),
            "{err}"
        );
    }
}

#[test]
fn extra_bytes_in_every_encoded_section_are_rejected() {
    let f = fixture();
    let empty = zstd::bulk::compress(&[], 1).unwrap();
    let mut skip = 0x184d2a50u32.to_le_bytes().to_vec();
    skip.extend_from_slice(&4u32.to_le_bytes());
    skip.extend_from_slice(b"junk");
    for tail in [&empty, &skip] {
        let mut side_tail = f.record.clone();
        side_tail.extend_from_slice(tail);
        let n = u64at(&side_tail, 40) + tail.len();
        side_tail[40..48].copy_from_slice(&(n as u64).to_le_bytes());
        let error = reject(&side_tail);
        assert!(error.to_lowercase().contains("trailing"), "{error}");
        let cross_end = 88 + u64at(&f.record, 24) + u64at(&f.record, 32);
        let mut cross_tail = f.record[..cross_end].to_vec();
        cross_tail.extend_from_slice(tail);
        cross_tail.extend_from_slice(&f.record[cross_end..]);
        let n = u64at(&cross_tail, 32) + tail.len();
        cross_tail[32..40].copy_from_slice(&(n as u64).to_le_bytes());
        assert!(reject(&cross_tail).contains("trailing MOV cross-channel"));
    }
    let end = 88 + u64at(&f.record, 24);
    let mut range_tail = f.record[..end].to_vec();
    range_tail.push(0);
    range_tail.extend_from_slice(&f.record[end..]);
    let n = u64at(&range_tail, 24) + 1;
    range_tail[24..32].copy_from_slice(&(n as u64).to_le_bytes());
    assert!(reject(&range_tail).contains("trailing MOV motion entropy"));
}

#[test]
fn a_single_cancel_signal_stops_header_entropy_and_reconstruction() {
    let f = fixture();
    for target in [0, 50_000, 100_000, 200_000, 300_000, 750_000, 1_000_000] {
        let mut rejected = false;
        let mut called_after = false;
        let err = mov_motion::decode_with_progress(&f.record, &mut |done, _| {
            if rejected {
                called_after = true;
                return true;
            }
            if done >= target {
                rejected = true;
                false
            } else {
                true
            }
        })
        .unwrap_err();
        assert_eq!(err, "error.cancelled");
        assert!(rejected && !called_after);
    }
}

fn progress(_: Progress) -> bool {
    true
}
fn archive(root: &Path, f: &Fixture) -> PathBuf {
    let prefix = b"already staged image";
    let files = vec![
        Entry {
            name: "before.png".into(),
            method: Method::Raw,
            size: prefix.len() as u64,
            stored: prefix.len() as u64,
            hash: blake3::hash(prefix).to_hex().to_string(),
            reference: None,
        },
        Entry {
            name: "action.mov".into(),
            method: Method::MovMotion,
            size: f.raw.len() as u64,
            stored: f.record.len() as u64,
            hash: blake3::hash(&f.raw).to_hex().to_string(),
            reference: None,
        },
    ];
    let mut h = blake3::Hasher::new();
    for e in &files {
        h.update(&(e.name.len() as u64).to_le_bytes());
        h.update(e.name.as_bytes());
        h.update(&e.size.to_le_bytes());
        h.update(e.hash.as_bytes());
    }
    let m = Manifest {
        version: 2,
        dir_name: "restored".into(),
        src_bytes: files.iter().map(|e| e.size).sum(),
        source_hash: h.finalize().to_hex().to_string(),
        preset: "max".into(),
        files,
    };
    let json = serde_json::to_vec(&m).unwrap();
    let mut b = container::MAGIC.to_vec();
    b.extend_from_slice(&(json.len() as u32).to_le_bytes());
    b.extend_from_slice(blake3::hash(&json).as_bytes());
    b.extend(json);
    let mut payload = prefix.to_vec();
    payload.extend_from_slice(&f.record);
    b.extend(zstd::stream::encode_all(payload.as_slice(), 3).unwrap());
    let path = root.join("motion.spack");
    fs::write(&path, b).unwrap();
    path
}

#[test]
fn container_restores_motion_records_and_cancellation_rolls_back() {
    let tmp = tempfile::tempdir().unwrap();
    let f = fixture();
    let packed = archive(tmp.path(), f);
    let restored =
        container::unpack(&packed, Some(&tmp.path().join("out")), &mut progress, None).unwrap();
    assert_eq!(fs::read(restored.dir.join("action.mov")).unwrap(), f.raw);
    assert_eq!(restored.n_files, 2);
    let dest = tmp.path().join("cancelled");
    fs::create_dir(&dest).unwrap();
    fs::write(dest.join("keep.txt"), b"existing").unwrap();
    let mut denied = false;
    let err = container::unpack(
        &packed,
        Some(&dest),
        &mut |p| {
            if !denied && p.phase == "unpack" && p.detail == "action.mov" && p.done > 20 {
                denied = true;
                false
            } else {
                true
            }
        },
        None,
    )
    .unwrap_err();
    assert_eq!(err, "error.cancelled");
    assert!(denied);
    assert_eq!(fs::read_dir(&dest).unwrap().count(), 1);
    assert_eq!(fs::read(dest.join("keep.txt")).unwrap(), b"existing");
}

struct Bits {
    data: Vec<u8>,
    bit: usize,
}
impl Bits {
    fn put(&mut self, value: u32, n: usize) {
        self.data.resize((self.bit + n).div_ceil(8), 0);
        for shift in (0..n).rev() {
            self.data[self.bit / 8] |= (((value >> shift) & 1) as u8) << (7 - self.bit % 8);
            self.bit += 1;
        }
    }
    fn word(&mut self, value: u32, book: u8) {
        let rice = (book >> 5) as usize;
        let exp = ((book >> 2) & 7) as usize;
        let switch = (book & 3) as usize;
        let threshold = ((switch + 1) as u32) << rice;
        if value < threshold {
            self.put(0, (value >> rice) as usize);
            self.put(1, 1);
            self.put(value & ((1 << rice) - 1), rice);
        } else {
            let n = value + (1 << exp) - threshold;
            let log = 31 - n.leading_zeros();
            self.put(0, log as usize - exp + switch + 1);
            self.put(n, log as usize + 1);
        }
    }
}

fn constant_plane(blocks: usize, dc: i32) -> Vec<u8> {
    let mut bits = Bits {
        data: vec![],
        bit: 0,
    };
    bits.word(((dc << 1) ^ (dc >> 31)) as u32, 0xb8);
    for block in 1..blocks {
        bits.word(0, if block == 1 { 0x70 } else { 0x04 });
    }
    bits.data
}

fn mov_fixture() -> Vec<u8> {
    let mut picture = vec![0u8; 8 + 630 * 2];
    picture[0] = 8 << 3;
    picture[5..7].copy_from_slice(&630u16.to_be_bytes());
    picture[7] = 0x30;
    let mut table = 0;
    for _ in 0..63 {
        for macroblocks in [8, 8, 8, 8, 8, 8, 8, 4, 2, 1] {
            let planes = [
                constant_plane(macroblocks * 4, -3584),
                constant_plane(macroblocks * 4, 0),
                constant_plane(macroblocks * 4, 0),
            ];
            let mut slice = vec![8 << 3, 1];
            for p in &planes {
                slice.extend_from_slice(&(p.len() as u16).to_be_bytes());
            }
            for p in planes {
                slice.extend(p);
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
    let mut mov = ((frame.len() + 8) as u32).to_be_bytes().to_vec();
    mov.extend_from_slice(b"mdat");
    mov.extend(frame);
    mov
}
