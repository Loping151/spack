use spack::core::{
    container::{self, Entry, Manifest, Method, Progress},
    mov_model::{self, SharedContext},
    mov_motion,
};
use std::fs;

fn progress(_: Progress) -> bool {
    true
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

fn mov_fixture(dc: i32, marker: &[u8]) -> Vec<u8> {
    let mut picture = vec![0u8; 8 + 630 * 2];
    picture[0] = 8 << 3;
    picture[5..7].copy_from_slice(&630u16.to_be_bytes());
    picture[7] = 0x30;
    let mut table = 0;
    for _ in 0..63 {
        for macroblocks in [8, 8, 8, 8, 8, 8, 8, 4, 2, 1] {
            let planes = [
                constant_plane(macroblocks * 4, dc),
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
    mov.extend_from_slice(&((marker.len() + 8) as u32).to_be_bytes());
    mov.extend_from_slice(b"free");
    mov.extend_from_slice(marker);
    mov
}

fn entry(name: &str, bytes: &[u8], method: Method, stored: usize) -> Entry {
    Entry {
        name: name.into(),
        method,
        size: bytes.len() as u64,
        stored: stored as u64,
        hash: blake3::hash(bytes).to_hex().to_string(),
        reference: None,
    }
}

#[test]
fn stateless_motion_between_shared_records_preserves_sequence_and_all_hashes() {
    let tmp = tempfile::tempdir().unwrap();
    let first = mov_fixture(-3584, b"first shared record");
    let motion = mov_fixture(-2048, b"independent motion record");
    let second = mov_fixture(-3584, b"second shared record");
    assert_ne!(first, second);
    let mut state = SharedContext::default();
    let first_record =
        mov_model::encode_shared_with_progress(&first, &mut state, &mut |_, _| true).unwrap();
    let motion_record = mov_motion::encode_with_progress(&motion, &mut |_, _| true).unwrap();
    let second_record =
        mov_model::encode_shared_with_progress(&second, &mut state, &mut |_, _| true).unwrap();
    assert_eq!(&first_record[..8], b"SPPS\0\0\0\x01");
    assert_eq!(
        u64::from_le_bytes(first_record[28..36].try_into().unwrap()),
        0
    );
    assert_eq!(&motion_record[..8], b"SPMM\0\0\0\x01");
    assert_eq!(
        u64::from_le_bytes(second_record[28..36].try_into().unwrap()),
        1
    );
    let mut duplicate = entry("set/04-motion-copy.mov", &motion, Method::Duplicate, 0);
    duplicate.reference = Some(1);
    let files = vec![
        entry(
            "set/01-shared.mov",
            &first,
            Method::MovShared,
            first_record.len(),
        ),
        entry(
            "set/02-motion.mov",
            &motion,
            Method::MovMotion,
            motion_record.len(),
        ),
        entry(
            "set/03-shared.mov",
            &second,
            Method::MovShared,
            second_record.len(),
        ),
        duplicate,
    ];
    let mut source = blake3::Hasher::new();
    for e in &files {
        source.update(&(e.name.len() as u64).to_le_bytes());
        source.update(e.name.as_bytes());
        source.update(&e.size.to_le_bytes());
        source.update(e.hash.as_bytes());
    }
    let manifest = Manifest {
        version: 2,
        dir_name: "restored".into(),
        src_bytes: files.iter().map(|e| e.size).sum(),
        source_hash: source.finalize().to_hex().to_string(),
        preset: "max".into(),
        files,
    };
    let json = serde_json::to_vec(&manifest).unwrap();
    let mut archive = container::MAGIC.to_vec();
    archive.extend_from_slice(&(json.len() as u32).to_le_bytes());
    archive.extend_from_slice(blake3::hash(&json).as_bytes());
    archive.extend_from_slice(&json);
    let mut payload = first_record;
    payload.extend(motion_record);
    payload.extend(second_record);
    archive.extend(zstd::stream::encode_all(payload.as_slice(), 3).unwrap());
    let path = tmp.path().join("mixed-mov.spack");
    fs::write(&path, archive).unwrap();
    let stored = container::read_manifest(&path).unwrap();
    assert_eq!(
        stored.files.iter().map(|e| &e.method).collect::<Vec<_>>(),
        vec![
            &Method::MovShared,
            &Method::MovMotion,
            &Method::MovShared,
            &Method::Duplicate
        ]
    );
    assert_eq!(stored.files[3].reference, Some(1));
    let restored =
        container::unpack(&path, Some(&tmp.path().join("out")), &mut progress, None).unwrap();
    assert_eq!(restored.n_files, 4);
    assert_eq!(restored.bytes_written, manifest.src_bytes);
    for (entry, expected) in manifest
        .files
        .iter()
        .zip([&first, &motion, &second, &motion])
    {
        let actual = fs::read(restored.dir.join(&entry.name)).unwrap();
        assert_eq!(
            &actual, expected,
            "{} did not restore byte-for-byte",
            entry.name
        );
        assert_eq!(
            blake3::hash(&actual).to_hex().as_str(),
            entry.hash,
            "{} hash differs",
            entry.name
        );
    }
}
