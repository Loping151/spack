use spack::core::{
    container::{self, Entry, Manifest, Method, Progress},
    scan::Filter,
    volumes::{self, Split},
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

fn progress(_: Progress) -> bool {
    true
}

#[test]
fn gif_predict_references_must_be_prior_bounded_mov_entries() {
    let tmp = tempfile::tempdir().unwrap();
    for variant in 0..7 {
        let mut mov = raw_entry("set/action.mov", b"mov");
        let mut gif = raw_entry("set/action.gif", b"gif");
        gif.method = Method::GifPredict;
        gif.reference = Some(0);
        match variant {
            0 => gif.reference = None,
            1 => gif.reference = Some(1),
            2 => gif.reference = Some(2),
            3 => mov.name = "set/action.png".into(),
            4 => {
                mov.size = (256 << 20) + 1;
                mov.stored = mov.size;
            }
            5 => gif.name = "set/action.webp".into(),
            _ => gif.stored = (512 << 20) + 1,
        }
        let file = archive(
            tmp.path(),
            &format!("predict-{variant}"),
            manifest(vec![mov, gif]),
            b"movgif",
            b"",
        );
        assert!(container::read_manifest(&file).is_err());
        rejected_without_output(tmp.path(), &file, &format!("predict-out-{variant}"));
    }
}

fn raw_entry(name: &str, bytes: &[u8]) -> Entry {
    Entry {
        name: name.into(),
        method: Method::Raw,
        size: bytes.len() as u64,
        stored: bytes.len() as u64,
        hash: blake3::hash(bytes).to_hex().to_string(),
        reference: None,
    }
}

fn manifest(entries: Vec<Entry>) -> Manifest {
    let mut source = blake3::Hasher::new();
    for e in &entries {
        source.update(&(e.name.len() as u64).to_le_bytes());
        source.update(e.name.as_bytes());
        source.update(&e.size.to_le_bytes());
        source.update(e.hash.as_bytes());
    }
    Manifest {
        version: 2,
        dir_name: "restored".into(),
        src_bytes: entries.iter().fold(0u64, |n, e| n.wrapping_add(e.size)),
        source_hash: source.finalize().to_hex().to_string(),
        preset: "fast".into(),
        files: entries,
    }
}

fn archive(root: &Path, label: &str, m: Manifest, payload: &[u8], trailing: &[u8]) -> PathBuf {
    let json = serde_json::to_vec(&m).unwrap();
    let mut data = container::MAGIC.to_vec();
    data.extend_from_slice(&(json.len() as u32).to_le_bytes());
    data.extend_from_slice(blake3::hash(&json).as_bytes());
    data.extend_from_slice(&json);
    data.extend_from_slice(&zstd::stream::encode_all(payload, 3).unwrap());
    data.extend_from_slice(trailing);
    let file = root.join(format!("{label}.spack"));
    fs::write(&file, data).unwrap();
    file
}

fn rejected_without_output(root: &Path, file: &Path, label: &str) {
    let dest = root.join(label);
    fs::create_dir(&dest).unwrap();
    assert!(
        container::unpack(file, Some(&dest), &mut progress, None).is_err(),
        "accepted {}",
        file.display()
    );
    assert_eq!(
        fs::read_dir(dest).unwrap().count(),
        0,
        "failed extraction left an output"
    );
}

#[test]
fn plain_and_nonempty_frame_trailing_bytes_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let payload = b"selected image bytes";
    for (label, trailer) in [
        ("junk", b"unclaimed tail".to_vec()),
        (
            "extra-frame",
            zstd::stream::encode_all(&b"unclaimed"[..], 3).unwrap(),
        ),
    ] {
        let file = archive(
            tmp.path(),
            label,
            manifest(vec![raw_entry("a.png", payload)]),
            payload,
            &trailer,
        );
        rejected_without_output(tmp.path(), &file, &format!("out-{label}"));
    }
}

#[test]
fn empty_zstd_frame_trailing_bytes_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let payload = b"selected image bytes";
    let trailer = zstd::stream::encode_all(&b""[..], 3).unwrap();
    let file = archive(
        tmp.path(),
        "empty-frame",
        manifest(vec![raw_entry("a.png", payload)]),
        payload,
        &trailer,
    );
    rejected_without_output(tmp.path(), &file, "out");
}

#[test]
fn zstd_skippable_frame_trailing_bytes_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let payload = b"selected image bytes";
    let mut trailer = 0x184d2a50u32.to_le_bytes().to_vec();
    trailer.extend_from_slice(&9u32.to_le_bytes());
    trailer.extend_from_slice(b"unclaimed");
    let file = archive(
        tmp.path(),
        "skip-frame",
        manifest(vec![raw_entry("a.png", payload)]),
        payload,
        &trailer,
    );
    rejected_without_output(tmp.path(), &file, "out");
}

#[test]
fn undeclared_decompressed_bytes_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let file = archive(
        tmp.path(),
        "extra-payload",
        manifest(vec![raw_entry("a.png", b"image")]),
        b"image extra",
        &[],
    );
    rejected_without_output(tmp.path(), &file, "out");
}

#[test]
fn invalid_duplicate_references_are_rejected_before_writing() {
    let tmp = tempfile::tempdir().unwrap();
    for variant in 0..5 {
        let mut a = raw_entry("a.png", b"image");
        let mut b = raw_entry("b.png", b"image");
        b.method = Method::Duplicate;
        b.stored = 0;
        b.reference = Some(0);
        match variant {
            0 => b.reference = Some(1),
            1 => b.reference = Some(99),
            2 => b.stored = 1,
            3 => b.hash = blake3::hash(b"other").to_hex().to_string(),
            _ => {
                a.method = Method::Duplicate;
                a.stored = 0;
                a.reference = Some(1);
            }
        }
        let label = format!("duplicate-{variant}");
        let file = archive(tmp.path(), &label, manifest(vec![a, b]), b"image", &[]);
        rejected_without_output(tmp.path(), &file, &format!("out-{label}"));
    }
}

#[test]
fn duplicate_chains_and_empty_files_restore_exactly() {
    let tmp = tempfile::tempdir().unwrap();
    let a = raw_entry("a.png", b"");
    let mut b = raw_entry("b.png", b"");
    b.method = Method::Duplicate;
    b.reference = Some(0);
    let mut c = b.clone();
    c.name = "c.png".into();
    c.reference = Some(1);
    let file = archive(
        tmp.path(),
        "duplicate-chain",
        manifest(vec![a, b, c]),
        &[],
        &[],
    );
    let restored = container::unpack(&file, Some(tmp.path()), &mut progress, None).unwrap();
    assert_eq!(restored.n_files, 3);
    assert_eq!(fs::read_dir(&restored.dir).unwrap().count(), 3);
    assert!(fs::read(restored.dir.join("c.png")).unwrap().is_empty());
}

#[test]
fn excessive_record_lengths_and_total_overflow_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    for variant in 0..3 {
        let mut a = raw_entry("a.gif", b"x");
        let entries = match variant {
            0 => {
                a.method = Method::GifExact;
                a.stored = (512 << 20) + 1;
                vec![a]
            }
            1 => {
                a.method = Method::GifExact;
                a.size = (256 << 20) + 1;
                vec![a]
            }
            _ => {
                a.size = u64::MAX;
                a.stored = u64::MAX;
                vec![a, raw_entry("b.png", b"x")]
            }
        };
        let label = format!("length-{variant}");
        let file = archive(tmp.path(), &label, manifest(entries), b"x", &[]);
        rejected_without_output(tmp.path(), &file, &format!("out-{label}"));
    }
}

#[test]
fn file_directory_alias_and_case_alias_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    for (variant, name) in ["a.png/child.webp", "A.PNG"].iter().enumerate() {
        let file = archive(
            tmp.path(),
            &format!("alias-{variant}"),
            manifest(vec![raw_entry("a.png", b"x"), raw_entry(name, b"y")]),
            b"xy",
            &[],
        );
        rejected_without_output(tmp.path(), &file, &format!("out-{variant}"));
    }
}

#[test]
fn traversal_and_windows_aliases_in_rehashed_manifests_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    for (variant, name) in [
        "../escape.png",
        "nested/../../escape.png",
        "/absolute.png",
        "C:/escape.png",
        "\\\\server\\share\\escape.png",
        "a.png:stream",
        "nested//a.png",
        "NUL.png",
        "nested/COM1.png",
        "nested./a.png",
    ]
    .iter()
    .enumerate()
    {
        let file = archive(
            tmp.path(),
            &format!("path-{variant}"),
            manifest(vec![raw_entry(name, b"x")]),
            b"x",
            &[],
        );
        rejected_without_output(tmp.path(), &file, &format!("out-{variant}"));
    }
    assert!(!tmp.path().join("escape.png").exists());
}

#[test]
fn cancelling_after_a_part_is_published_removes_only_this_runs_parts() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("source");
    fs::write(&src, vec![23u8; 4096]).unwrap();
    let out = tmp.path().join("out");
    fs::create_dir(&out).unwrap();
    fs::write(out.join("keep.txt"), b"user file").unwrap();
    let result = volumes::publish(
        &src,
        &out.join("pack.spack"),
        &Split::Count(3),
        &mut |p| !(p.phase == "split" && p.done > 0),
        &None,
    );
    assert!(result.is_err());
    assert_eq!(fs::read_dir(&out).unwrap().count(), 1);
    assert_eq!(fs::read(out.join("keep.txt")).unwrap(), b"user file");
}

#[test]
fn volume_publication_collision_preserves_the_other_file_and_rolls_back() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("source");
    fs::write(&src, vec![23u8; 4096]).unwrap();
    let out = tmp.path().join("out");
    fs::create_dir(&out).unwrap();
    let collision = out.join("pack.spack.002");
    let result = volumes::publish(
        &src,
        &out.join("pack.spack"),
        &Split::Count(3),
        &mut |p| {
            if p.phase == "split" && p.detail == "2 / 3" {
                fs::write(&collision, b"concurrent user file").unwrap();
            }
            true
        },
        &None,
    );
    assert!(result.is_err());
    assert_eq!(fs::read_dir(&out).unwrap().count(), 1);
    assert_eq!(fs::read(collision).unwrap(), b"concurrent user file");
}

#[test]
fn cancellation_at_final_verification_does_not_publish_a_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let file = archive(
        tmp.path(),
        "cancel",
        manifest(vec![raw_entry("a.png", b"image")]),
        b"image",
        &[],
    );
    let out = tmp.path().join("out");
    fs::create_dir(&out).unwrap();
    assert!(container::unpack(&file, Some(&out), &mut |p| p.phase != "verify", None).is_err());
    assert_eq!(fs::read_dir(out).unwrap().count(), 0);
}

#[test]
fn output_appearing_at_final_verification_is_preserved() {
    let tmp = tempfile::tempdir().unwrap();
    let file = archive(
        tmp.path(),
        "race",
        manifest(vec![raw_entry("a.png", b"image")]),
        b"image",
        &[],
    );
    let out = tmp.path().join("out");
    fs::create_dir(&out).unwrap();
    assert!(container::unpack(
        &file,
        Some(&out),
        &mut |p| {
            if p.phase == "verify" {
                fs::create_dir(out.join("restored")).unwrap();
                fs::write(out.join("restored/keep.txt"), b"user file").unwrap();
            }
            true
        },
        None
    )
    .is_err());
    assert_eq!(fs::read_dir(&out).unwrap().count(), 1);
    assert_eq!(
        fs::read(out.join("restored/keep.txt")).unwrap(),
        b"user file"
    );
}

#[test]
fn final_progress_cancellation_flag_prevents_publication() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    fs::write(&source, b"archive").unwrap();
    let destination = tmp.path().join("result.spack");
    let flag = Arc::new(AtomicBool::new(false));
    let result = volumes::publish(
        &source,
        &destination,
        &Split::None,
        &mut |p| {
            if p.phase == "split" && p.done == p.total {
                flag.store(true, Ordering::Relaxed);
            }
            true
        },
        &Some(flag.clone()),
    );
    assert!(result.is_err(), "late cancellation was ignored");
    assert!(!destination.exists());
}

#[test]
fn verify_filtered_files_does_not_claim_full_directory_equivalence() {
    let tmp = tempfile::tempdir().unwrap();
    let original = tmp.path().join("original");
    fs::create_dir(&original).unwrap();
    let restored = tmp.path().join("restored");
    fs::create_dir(&restored).unwrap();
    fs::write(original.join("a.png"), b"image").unwrap();
    fs::write(original.join("notes.txt"), b"not selected").unwrap();
    fs::write(restored.join("a.png"), b"image").unwrap();
    assert_eq!(
        container::verify(&original, &restored, Filter::All).unwrap(),
        1
    );
}
