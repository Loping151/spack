use spack::core::{
    container::{self, PackOptions, Progress},
    scan::{self, Filter},
    volumes::Split,
};
use std::{fs, path::PathBuf};
fn progress(_: Progress) -> bool {
    true
}
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("素材");
    let out = tmp.path().join("packs");
    fs::create_dir_all(source.join("nested")).unwrap();
    fs::create_dir_all(&out).unwrap();
    let bytes: Vec<u8> = (0..32000).map(|i| ((i * 17 + i / 7) % 251) as u8).collect();
    fs::write(source.join("first.gif"), &bytes).unwrap();
    fs::write(source.join("nested/duplicate.mov"), &bytes).unwrap();
    fs::write(source.join("still.png"), b"small png placeholder").unwrap();
    fs::write(source.join("empty.webp"), b"").unwrap();
    fs::write(source.join("ignored.txt"), b"not selected").unwrap();
    (tmp, source, out)
}
fn options(out: PathBuf, split: Split) -> PackOptions {
    PackOptions {
        out_dir: Some(out),
        split,
        preset: "fast".into(),
        ..Default::default()
    }
}
#[test]
fn mixed_roundtrip_dedup_hash_and_non_overwrite() {
    let (tmp, src, out) = fixture();
    let p = container::pack(
        std::slice::from_ref(&src),
        &options(out.clone(), Split::None),
        &mut progress,
        None,
    )
    .unwrap();
    assert_eq!(p.n_files, 4);
    let m = container::read_manifest(&p.pack_path).unwrap();
    assert!(m
        .files
        .iter()
        .any(|e| e.method == container::Method::Duplicate));
    let r = container::unpack(&p.pack_path, Some(tmp.path()), &mut progress, None).unwrap();
    assert_ne!(r.dir, src);
    assert_eq!(container::verify(&src, &r.dir, Filter::All).unwrap(), 4);
    assert!(src.join("ignored.txt").exists());
    assert!(container::pack(&[src], &options(out, Split::None), &mut progress, None).is_err());
}
#[test]
fn exact_part_count_any_part_and_missing_volume() {
    let (tmp, src, out) = fixture();
    let p = container::pack(
        std::slice::from_ref(&src),
        &options(out, Split::Count(7)),
        &mut progress,
        None,
    )
    .unwrap();
    assert_eq!(p.parts.len(), 7);
    let sizes: Vec<u64> = p
        .parts
        .iter()
        .map(|p| fs::metadata(p).unwrap().len())
        .collect();
    assert!(sizes.iter().max().unwrap() - sizes.iter().min().unwrap() <= 1);
    let r = container::unpack(&p.parts[3], Some(tmp.path()), &mut progress, None).unwrap();
    container::verify(&src, &r.dir, Filter::All).unwrap();
    fs::remove_file(&p.parts[1]).unwrap();
    assert!(container::unpack(&p.parts[3], Some(tmp.path()), &mut progress, None).is_err());
}
#[test]
fn size_cap_and_small_archive() {
    let (tmp, src, out) = fixture();
    let p = container::pack(
        std::slice::from_ref(&src),
        &options(out, Split::Size(420)),
        &mut progress,
        None,
    )
    .unwrap();
    assert!(p.parts.len() > 1);
    assert!(p
        .parts
        .iter()
        .all(|p| fs::metadata(p).unwrap().len() <= 420));
    container::unpack(&p.parts[0], Some(tmp.path()), &mut progress, None).unwrap();
    let p = container::pack(
        &[src],
        &options(tmp.path().join("large-cap"), Split::Size(1 << 20)),
        &mut progress,
        None,
    )
    .unwrap();
    assert_eq!(p.parts.len(), 1);
    assert_eq!(p.pack_path.extension().unwrap(), "spk");
    let exact_cap = fs::metadata(&p.pack_path).unwrap().len();
    let exact_out = tmp.path().join("exact-cap");
    fs::create_dir(&exact_out).unwrap();
    let capped = spack::core::volumes::publish(
        &p.pack_path,
        &exact_out.join("exact.spk"),
        &Split::Size(exact_cap),
        &mut progress,
        &None,
    )
    .unwrap();
    assert_eq!(capped.len(), 1);
    assert_eq!(fs::metadata(&capped[0]).unwrap().len(), exact_cap);
}
#[test]
fn truncated_corrupt_manifest_and_payload_fail_atomically() {
    let (tmp, src, out) = fixture();
    let p = container::pack(&[src], &options(out, Split::None), &mut progress, None).unwrap();
    let original = fs::read(&p.pack_path).unwrap();
    for variant in 0..3 {
        let mut damaged = original.clone();
        match variant {
            0 => {
                damaged.truncate(damaged.len() - 3);
            }
            1 => damaged[50] ^= 1,
            _ => {
                let i = damaged.len() - 9;
                damaged[i] ^= 1;
            }
        }
        let bad = tmp.path().join(format!("bad-{variant}.spack"));
        fs::write(&bad, damaged).unwrap();
        let dest = tmp.path().join(format!("bad-{variant}"));
        fs::create_dir(&dest).unwrap();
        assert!(container::unpack(&bad, Some(&dest), &mut progress, None).is_err());
        assert_eq!(fs::read_dir(dest).unwrap().count(), 0);
    }
}
#[test]
fn part_bitflip_rejected() {
    let (tmp, src, out) = fixture();
    let p = container::pack(&[src], &options(out, Split::Count(3)), &mut progress, None).unwrap();
    let mut b = fs::read(&p.parts[1]).unwrap();
    b[100] ^= 1;
    fs::write(&p.parts[1], b).unwrap();
    assert!(container::unpack(&p.parts[2], Some(tmp.path()), &mut progress, None).is_err());
}
#[test]
fn cancel_cleans_staging() {
    let (tmp, src, out) = fixture();
    let r = container::pack(
        std::slice::from_ref(&src),
        &options(out.clone(), Split::Count(3)),
        &mut |p| p.phase != "split",
        None,
    );
    assert!(r.is_err());
    assert_eq!(fs::read_dir(&out).unwrap().count(), 0);
    let p = container::pack(&[src], &options(out, Split::None), &mut progress, None).unwrap();
    let target = tmp.path().join("cancelled");
    fs::create_dir(&target).unwrap();
    assert!(container::unpack(&p.pack_path, Some(&target), &mut |_| false, None).is_err());
    assert_eq!(fs::read_dir(target).unwrap().count(), 0);
}
#[test]
fn path_names_are_portable() {
    for name in [
        "../a",
        "a/../b",
        "/absolute",
        "C:/x",
        "a\\b",
        "a:",
        "a.",
        "a ",
        "NUL.gif",
        "x/COM1.png",
        "a//b",
        "",
    ] {
        assert!(container::safe_rel_path(name).is_err(), "{name}");
    }
    assert!(container::safe_rel_path("素材/笑.webp").is_ok());
}
#[test]
fn two_same_named_source_folders_and_filters() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("GIF/素材");
    let b = tmp.path().join("MOV/素材");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("a.gif"), b"gif").unwrap();
    fs::write(b.join("a.mov"), b"mov").unwrap();
    let (files, stats) = scan::collect(&[a.clone(), b.clone()], Filter::All).unwrap();
    assert_eq!(stats.files, 2);
    assert!(files.iter().any(|f| f.name == "GIF/素材/a.gif"));
    assert_eq!(
        scan::collect(&[a.clone(), b.clone()], Filter::NoGif)
            .unwrap()
            .1
            .files,
        1
    );
    assert_eq!(
        scan::collect(&[a.clone(), b.clone()], Filter::NoMov)
            .unwrap()
            .1
            .files,
        1
    );
    let p = container::pack(
        &[a, b],
        &options(tmp.path().join("pack"), Split::None),
        &mut progress,
        None,
    )
    .unwrap();
    let r = container::unpack(&p.pack_path, Some(tmp.path()), &mut progress, None).unwrap();
    assert_eq!(fs::read(r.dir.join("MOV/素材/a.mov")).unwrap(), b"mov");
}
