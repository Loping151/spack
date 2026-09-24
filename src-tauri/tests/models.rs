use spack::core::{
    container::{self, Method, PackOptions, Progress},
    scan::Filter,
    volumes::Split,
};
use std::{collections::HashMap, fs, path::Path};

fn progress(_: Progress) -> bool {
    true
}

fn literal_gif() -> Vec<u8> {
    const W: usize = 128;
    const H: usize = 128;
    let mut gif = b"GIF89a".to_vec();
    gif.extend_from_slice(&(W as u16).to_le_bytes());
    gif.extend_from_slice(&(H as u16).to_le_bytes());
    gif.extend_from_slice(&[0xf7, 0, 0]);
    for i in 0..256 {
        gif.extend_from_slice(&[i as u8, (i * 37) as u8, (i * 71) as u8]);
    }
    let mut seed = 0x39f17abu32;
    for frame in 0..14 {
        let mut pixels = vec![0u8; W * H];
        for i in 0..pixels.len() {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            pixels[i] = if i % W > 0 && !seed.is_multiple_of(5) {
                pixels[i - 1]
            } else if i >= W && seed.is_multiple_of(3) {
                pixels[i - W]
            } else {
                ((seed >> 7) % 64) as u8
            };
        }
        let mut coded = Vec::new();
        let mut bit = 0usize;
        let mut width = 9u32;
        let mut decoder_next = 258u32;
        let mut previous = false;
        let mut emit = |code: u16| {
            let end = bit + width as usize;
            coded.resize(end.div_ceil(8), 0);
            let word = (code as u32) << (bit & 7);
            for k in 0..3 {
                if let Some(byte) = coded.get_mut(bit / 8 + k) {
                    *byte |= (word >> (8 * k)) as u8;
                }
            }
            bit = end;
            if code == 256 {
                width = 9;
                decoder_next = 258;
                previous = false;
            } else if code != 257 {
                if previous && decoder_next < 4096 {
                    decoder_next += 1;
                    if decoder_next == (1 << width) && width < 12 {
                        width += 1;
                    }
                }
                previous = true;
            }
        };
        let mut dictionary: HashMap<(u16, u8), u16> = HashMap::new();
        let mut next = 258u16;
        emit(256);
        let mut phrase = pixels[0] as u16;
        for &ch in &pixels[1..] {
            if let Some(&code) = dictionary.get(&(phrase, ch)) {
                phrase = code;
            } else {
                emit(phrase);
                if next < 4096 {
                    dictionary.insert((phrase, ch), next);
                    next += 1;
                } else {
                    emit(256);
                    dictionary.clear();
                    next = 258;
                }
                phrase = ch as u16;
            }
        }
        emit(phrase);
        emit(257);
        gif.extend_from_slice(&[0x21, 0xf9, 4, 0, frame + 1, 0, 0, 0, 0x2c, 0, 0, 0, 0]);
        gif.extend_from_slice(&(W as u16).to_le_bytes());
        gif.extend_from_slice(&(H as u16).to_le_bytes());
        gif.extend_from_slice(&[0, 8]);
        for block in coded.chunks(253) {
            gif.push(block.len() as u8);
            gif.extend_from_slice(block);
        }
        gif.push(0);
    }
    gif.push(0x3b);
    gif
}

fn movie_fixture() -> Vec<u8> {
    fn plane(coefficients: &[i16]) -> Vec<u8> {
        let mut record = b"SPPR\0\0\0\x01".to_vec();
        record.extend_from_slice(&4096u64.to_le_bytes());
        record.extend_from_slice(&1u32.to_le_bytes());
        record.extend_from_slice(&0u64.to_le_bytes());
        record.extend_from_slice(&4096u32.to_le_bytes());
        record.extend_from_slice(&[4, 3, 0, 0]);
        let values: Vec<u16> = coefficients
            .iter()
            .map(|&c| {
                let c = c as i32;
                ((c << 1) ^ (c >> 31)) as u16
            })
            .collect();
        record.extend(values.iter().map(|&v| v as u8));
        record.extend(values.iter().map(|&v| (v >> 8) as u8));
        let mut packed = spack::core::mov_exact::decode(&record).unwrap();
        packed.truncate(packed.iter().rposition(|&b| b != 0).unwrap() + 1);
        packed
    }
    let mut seed = 0x593157u32;
    let mut patterns = [[0i16; 64]; 32];
    for (n, pattern) in patterns.iter_mut().enumerate() {
        for (frequency, value) in pattern.iter_mut().enumerate() {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            *value = if frequency == 0 {
                (n * 19) as i16 - 250
            } else if seed & 3 == 0 {
                ((seed >> 9) % 31) as i16 - 15
            } else {
                0
            };
        }
        pattern[63] = n as i16 % 7 + 1;
    }
    let mut mdat = Vec::new();
    for _ in 0..64 {
        let mut planes = Vec::new();
        for _ in 0..3 {
            let mut coefficients = vec![0i16; 256];
            for block in 0..4 {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let pattern = &patterns[(seed as usize) % patterns.len()];
                for frequency in 0..64 {
                    coefficients[frequency * 4 + block] = pattern[frequency];
                }
            }
            planes.push(plane(&coefficients));
        }
        let slice_len = 8 + planes.iter().map(Vec::len).sum::<usize>() + 16;
        let picture_len = 10 + slice_len;
        let frame_len = 28 + picture_len + 7;
        let mut frame = vec![0u8; 28];
        frame[..4].copy_from_slice(&(frame_len as u32).to_be_bytes());
        frame[4..8].copy_from_slice(b"icpf");
        frame[8..10].copy_from_slice(&20u16.to_be_bytes());
        frame[16..18].copy_from_slice(&16u16.to_be_bytes());
        frame[18..20].copy_from_slice(&16u16.to_be_bytes());
        frame[20] = 0xc0;
        frame.push(0x40);
        frame.extend_from_slice(&(picture_len as u32).to_be_bytes());
        frame.extend_from_slice(&[0, 1, 0]);
        frame.extend_from_slice(&(slice_len as u16).to_be_bytes());
        frame.extend_from_slice(&[0x40, 1]);
        for p in &planes {
            frame.extend_from_slice(&(p.len() as u16).to_be_bytes());
        }
        for p in &planes {
            frame.extend_from_slice(p);
        }
        frame.extend_from_slice(&[0xaa; 16]);
        frame.extend_from_slice(&[0xf1; 7]);
        mdat.extend_from_slice(&frame);
        mdat.extend_from_slice(&[0; 13]);
    }
    let mut raw = Vec::new();
    for (kind, data) in [
        (b"ftyp", b"qt  ".as_slice()),
        (b"moov", b"unknown metadata".as_slice()),
        (b"mdat", mdat.as_slice()),
        (b"free", b"tail padding".as_slice()),
    ] {
        raw.extend_from_slice(&((data.len() + 8) as u32).to_be_bytes());
        raw.extend_from_slice(kind);
        raw.extend_from_slice(data);
    }
    raw
}

fn options(out: &Path) -> PackOptions {
    PackOptions {
        out_dir: Some(out.to_path_buf()),
        preset: "max".into(),
        split: Split::None,
        ..Default::default()
    }
}

#[test]
fn mixed_model_records_preserve_state_across_raw_and_duplicate_movies() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("source");
    fs::create_dir(&src).unwrap();
    let gif = literal_gif();
    fs::write(src.join("00-animation.gif"), &gif).unwrap();
    fs::write(src.join("01-animation-copy.gif"), &gif).unwrap();
    let a = movie_fixture();
    let mut b = a.clone();
    b[20] ^= 1;
    let mut c = a.clone();
    c[20] ^= 2;
    for (name, bytes) in [
        ("10-a.mov", a.as_slice()),
        ("20-unsupported.mov", b"unsupported MOV codec".as_slice()),
        ("30-a-copy.mov", a.as_slice()),
        ("40-b.mov", b.as_slice()),
        ("50-b-copy.mov", b.as_slice()),
        ("60-c.mov", c.as_slice()),
    ] {
        fs::write(src.join(name), bytes).unwrap();
    }
    let result = container::pack(
        std::slice::from_ref(&src),
        &options(&tmp.path().join("packs")),
        &mut progress,
        None,
    )
    .unwrap();
    let m = container::read_manifest(&result.pack_path).unwrap();
    let methods: HashMap<_, _> = m
        .files
        .iter()
        .map(|e| (e.name.as_str(), e.method.clone()))
        .collect();
    assert_eq!(methods["00-animation.gif"], Method::GifModel, "{methods:?}");
    for name in ["10-a.mov", "40-b.mov", "60-c.mov"] {
        assert_eq!(methods[name], Method::MovShared, "{methods:?}");
    }
    assert_eq!(methods["20-unsupported.mov"], Method::Raw);
    for name in ["01-animation-copy.gif", "30-a-copy.mov", "50-b-copy.mov"] {
        assert_eq!(methods[name], Method::Duplicate);
    }
    let restored = container::unpack(
        &result.pack_path,
        Some(&tmp.path().join("restored")),
        &mut progress,
        None,
    )
    .unwrap();
    assert_eq!(
        container::verify(&src, &restored.dir, Filter::All).unwrap(),
        8
    );
    for entry in &m.files {
        assert_eq!(
            fs::read(src.join(&entry.name)).unwrap(),
            fs::read(restored.dir.join(&entry.name)).unwrap()
        );
    }
}

#[test]
fn a_single_false_gif_model_callback_cancels_instead_of_falling_back() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("source");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("animation.gif"), literal_gif()).unwrap();
    let out = tmp.path().join("packs");
    let mut declined = false;
    let result = container::pack(
        &[src],
        &options(&out),
        &mut |p| {
            if !declined && p.phase == "transform" && p.done > 0 && p.done < p.total {
                declined = true;
                false
            } else {
                true
            }
        },
        None,
    );
    assert!(
        declined,
        "expected progress from inside the GIF probability model"
    );
    assert!(result.unwrap_err().contains("error.cancelled"));
    assert_eq!(fs::read_dir(out).unwrap().count(), 0);
}
