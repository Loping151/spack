use spack::core::{
    container::{self, Method, PackOptions, Progress},
    scan::{self, Filter, VIDEO_EXTENSIONS},
    volumes::Split,
};
use std::{fs, path::Path};

fn progress(_: Progress) -> bool {
    true
}

fn fixture(root: &Path) {
    fs::create_dir_all(root).unwrap();
    for extension in VIDEO_EXTENSIONS {
        let mut bytes = format!("video fixture: {extension}\0").into_bytes();
        bytes.extend(0u8..=255);
        fs::write(
            root.join(format!("clip.{}", extension.to_ascii_uppercase())),
            bytes,
        )
        .unwrap();
    }
    for extension in ["gif", "png", "webp"] {
        fs::write(root.join(format!("still.{extension}")), extension).unwrap();
    }
    fs::write(root.join("notes.txt"), "excluded").unwrap();
}

fn options(output: &Path, filter: Filter, split: Split) -> PackOptions {
    PackOptions {
        out_dir: Some(output.to_path_buf()),
        filter,
        split,
        preset: "fast".into(),
        ..Default::default()
    }
}

#[test]
fn common_video_filters_and_statistics_preserve_mov_semantics() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    fixture(&source);
    for extension in VIDEO_EXTENSIONS {
        assert!(scan::is_video(&extension.to_ascii_uppercase()));
        assert!(Filter::Video.accepts(extension));
        assert!(!Filter::NoVideo.accepts(extension));
        assert_eq!(Filter::Mov.accepts(extension), *extension == "mov");
        assert_eq!(Filter::NoMov.accepts(extension), *extension != "mov");
    }
    assert!(!scan::is_video("gif"));
    assert!(!Filter::Video.accepts("txt"));
    assert!(!Filter::NoVideo.accepts("txt"));
    assert_eq!(Filter::parse("video").unwrap(), Filter::Video);
    assert_eq!(Filter::parse("no-video").unwrap(), Filter::NoVideo);
    let count = VIDEO_EXTENSIONS.len();
    for (filter, files, video, mov) in [
        (Filter::All, count + 3, count, 1),
        (Filter::Video, count, count, 1),
        (Filter::NoVideo, 3, 0, 0),
        (Filter::Mov, 1, 1, 1),
        (Filter::NoMov, count + 2, count - 1, 0),
        (Filter::Gif, 1, 0, 0),
        (Filter::NoGif, count + 2, count, 1),
    ] {
        let (selected, stats) = scan::collect(std::slice::from_ref(&source), filter).unwrap();
        assert_eq!((stats.files, stats.video, stats.mov), (files, video, mov));
        assert_eq!(
            stats.bytes,
            selected.iter().map(|file| file.bytes).sum::<u64>()
        );
        assert_eq!(
            stats.files,
            stats.video + stats.gif + stats.png + stats.webp
        );
    }
}

#[test]
fn mixed_videos_and_images_restore_original_hashes_without_codec_claims() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    fixture(&source);
    for (name, filter) in [
        ("all", Filter::All),
        ("video", Filter::Video),
        ("images", Filter::NoVideo),
    ] {
        let packed = container::pack(
            std::slice::from_ref(&source),
            &options(&tmp.path().join(name), filter, Split::None),
            &mut progress,
            None,
        )
        .unwrap();
        assert_eq!(packed.pack_path.extension().unwrap(), "spk");
        let manifest = container::read_manifest(&packed.pack_path).unwrap();
        for entry in &manifest.files {
            let extension = scan::extension(&entry.name);
            if scan::is_video(&extension) && extension != "mov" {
                assert_eq!(entry.method, Method::Raw);
            }
        }
        let unpacked = container::unpack(
            &packed.pack_path,
            Some(&tmp.path().join(format!("restored-{name}"))),
            &mut progress,
            None,
        )
        .unwrap();
        assert_eq!(
            container::verify(&source, &unpacked.dir, filter).unwrap(),
            packed.n_files
        );
        for entry in &manifest.files {
            assert_eq!(
                container::hash_file(&source.join(&entry.name)).unwrap(),
                entry.hash
            );
            assert_eq!(
                container::hash_file(&unpacked.dir.join(&entry.name)).unwrap(),
                entry.hash
            );
        }
    }
}

#[test]
fn current_and_legacy_single_archive_names_are_case_insensitive() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    fixture(&source);
    let packed = container::pack(
        std::slice::from_ref(&source),
        &options(&tmp.path().join("pack"), Filter::All, Split::None),
        &mut progress,
        None,
    )
    .unwrap();
    for (index, extension) in ["spk", "SPK", "spack", "SPACK"].into_iter().enumerate() {
        let alias = tmp.path().join(format!("archive-{index}.{extension}"));
        fs::copy(&packed.pack_path, &alias).unwrap();
        assert_eq!(
            container::archive_info(&alias).unwrap().files,
            packed.n_files
        );
        let unpacked = container::unpack(&alias, Some(tmp.path()), &mut progress, None).unwrap();
        assert_eq!(
            container::verify(&source, &unpacked.dir, Filter::All).unwrap(),
            packed.n_files
        );
    }
}

#[test]
fn current_and_legacy_volumes_preview_and_extract_from_any_part() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    fixture(&source);
    let packed = container::pack(
        std::slice::from_ref(&source),
        &options(&tmp.path().join("pack"), Filter::All, Split::Count(4)),
        &mut progress,
        None,
    )
    .unwrap();
    assert!(packed.parts[0].to_string_lossy().ends_with(".spk.001"));
    for (index, extension) in ["spk", "SPK", "spack", "SPACK"].into_iter().enumerate() {
        let aliases: Vec<_> = packed
            .parts
            .iter()
            .enumerate()
            .map(|(part, path)| {
                let alias = tmp
                    .path()
                    .join(format!("archive-{index}.{extension}.{:03}", part + 1));
                fs::copy(path, &alias).unwrap();
                alias
            })
            .collect();
        let chosen = &aliases[2];
        let info = container::archive_info(chosen).unwrap();
        assert_eq!((info.files, info.parts), (packed.n_files, 4));
        let unpacked = container::unpack(chosen, Some(tmp.path()), &mut progress, None).unwrap();
        assert_eq!(
            container::verify(&source, &unpacked.dir, Filter::All).unwrap(),
            packed.n_files
        );
    }
}
