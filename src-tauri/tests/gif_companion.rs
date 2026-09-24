use spack::core::{
    container::{self, Entry, Manifest, Method, PackOptions, Progress},
    gif_predict,
    scan::Filter,
    volumes::Split,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::OnceLock,
};

fn progress(_: Progress) -> bool {
    true
}

struct Fixture {
    gif: Vec<u8>,
    mov: Vec<u8>,
    record: Vec<u8>,
}
fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let gif = gif_fixture();
        let mov = mov_fixture();
        let record = gif_predict::encode(&gif, &mov, &mut |_, _| true).unwrap();
        assert_eq!(
            gif_predict::decode(&record, &mov, &mut |_, _| true).unwrap(),
            gif
        );
        Fixture { gif, mov, record }
    })
}

fn gif_fixture() -> Vec<u8> {
    let mut raw = b"GIF89a\x2c\x01\x2c\x01\x80\0\0\0\0\0\xff\xff\xff\x21\xf9\x04\x01\0\0\0\0\x2c\0\0\0\0\x2c\x01\x2c\x01\0\x02".to_vec();
    let mut coded = vec![0u8; (90_000 * 6 + 3_usize).div_ceil(8)];
    let mut bit = 0;
    for code in (0..90_000).flat_map(|_| [4u8, 0]).chain([5]) {
        for shift in 0..3 {
            coded[bit / 8] |= ((code >> shift) & 1) << (bit % 8);
            bit += 1;
        }
    }
    *coded.last_mut().unwrap() |= 0xe0;
    for block in coded.chunks(251) {
        raw.push(block.len() as u8);
        raw.extend_from_slice(block);
    }
    raw.extend_from_slice(&[0, 0x3b, 0xaa]);
    raw
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

fn entry(name: &str, bytes: &[u8]) -> Entry {
    Entry {
        name: name.into(),
        method: Method::Raw,
        size: bytes.len() as u64,
        stored: bytes.len() as u64,
        hash: blake3::hash(bytes).to_hex().to_string(),
        reference: None,
    }
}

fn write_archive(root: &Path, label: &str, mov: &[u8], f: &Fixture) -> PathBuf {
    let original = entry("set/original.mov", mov);
    let mut duplicate = entry("set/action.mov", mov);
    duplicate.method = Method::Duplicate;
    duplicate.stored = 0;
    duplicate.reference = Some(0);
    let mut gif = entry("set/action.gif", &f.gif);
    gif.method = Method::GifPredict;
    gif.stored = f.record.len() as u64;
    gif.reference = Some(1);
    let files = vec![original, duplicate, gif];
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
    archive.extend(json);
    let mut payload = mov.to_vec();
    payload.extend_from_slice(&f.record);
    archive.extend(zstd::stream::encode_all(payload.as_slice(), 3).unwrap());
    let path = root.join(format!("{label}.spack"));
    fs::write(&path, archive).unwrap();
    path
}

#[test]
fn predicted_gif_restores_through_a_duplicate_mov_reference() {
    let tmp = tempfile::tempdir().unwrap();
    let f = fixture();
    let archive = write_archive(tmp.path(), "companion", &f.mov, f);
    let manifest = container::read_manifest(&archive).unwrap();
    assert_eq!(manifest.files[1].method, Method::Duplicate);
    assert_eq!(manifest.files[2].method, Method::GifPredict);
    assert_eq!(manifest.files[2].reference, Some(1));
    let result =
        container::unpack(&archive, Some(&tmp.path().join("out")), &mut progress, None).unwrap();
    assert_eq!(result.n_files, 3);
    assert_eq!(
        fs::read(result.dir.join("set/original.mov")).unwrap(),
        f.mov
    );
    assert_eq!(fs::read(result.dir.join("set/action.mov")).unwrap(), f.mov);
    assert_eq!(fs::read(result.dir.join("set/action.gif")).unwrap(), f.gif);
}

#[test]
fn wrong_but_individually_valid_mov_reference_rolls_back_all_output() {
    let tmp = tempfile::tempdir().unwrap();
    let f = fixture();
    let mut wrong_mov = f.mov.clone();
    wrong_mov.extend_from_slice(&8u32.to_be_bytes());
    wrong_mov.extend_from_slice(b"free");
    let archive = write_archive(tmp.path(), "wrong-companion", &wrong_mov, f);
    assert!(container::read_manifest(&archive).is_ok());
    let dest = tmp.path().join("out");
    fs::create_dir(&dest).unwrap();
    fs::write(dest.join("existing.txt"), b"preserve existing output").unwrap();
    let err = container::unpack(&archive, Some(&dest), &mut progress, None).unwrap_err();
    assert!(err.contains("MOV or payload checksum mismatch"), "{err}");
    assert_eq!(
        fs::read(dest.join("existing.txt")).unwrap(),
        b"preserve existing output"
    );
    assert_eq!(
        fs::read_dir(&dest).unwrap().count(),
        1,
        "staged output escaped rollback"
    );
}

#[test]
fn filtering_a_mixed_folder_to_gif_produces_no_companion_dependency() {
    let tmp = tempfile::tempdir().unwrap();
    let f = fixture();
    let source = tmp.path().join("mixed");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("action.gif"), &f.gif).unwrap();
    fs::write(source.join("action.mov"), &f.mov).unwrap();
    for (index, filter) in [Filter::Gif, Filter::NoMov].into_iter().enumerate() {
        let options = PackOptions {
            filter,
            preset: "max".into(),
            out_dir: Some(tmp.path().join(format!("pack-{index}"))),
            split: Split::None,
            raw: false,
        };
        let packed =
            container::pack(std::slice::from_ref(&source), &options, &mut progress, None).unwrap();
        let m = container::read_manifest(&packed.pack_path).unwrap();
        assert_eq!(m.files.len(), 1);
        assert_eq!(m.files[0].name, "action.gif");
        assert_ne!(m.files[0].method, Method::GifPredict);
        assert_eq!(m.files[0].reference, None);
        let restored = container::unpack(
            &packed.pack_path,
            Some(&tmp.path().join(format!("out-{index}"))),
            &mut progress,
            None,
        )
        .unwrap();
        assert_eq!(restored.n_files, 1);
        assert_eq!(fs::read(restored.dir.join("action.gif")).unwrap(), f.gif);
    }
}
