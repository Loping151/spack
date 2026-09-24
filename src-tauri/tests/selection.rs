use spack::core::{
    container::{self, PackOptions, Progress},
    scan::{self, Filter, Source, Stats},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

fn write(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    path
}

fn names(files: &[Source]) -> BTreeSet<String> {
    files.iter().map(|f| f.name.clone()).collect()
}

fn expected(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|v| (*v).to_owned()).collect()
}

fn snapshot(files: &[Source]) -> Vec<(String, PathBuf, u64)> {
    files
        .iter()
        .map(|f| (f.name.clone(), fs::canonicalize(&f.path).unwrap(), f.bytes))
        .collect()
}

fn stats(stats: &Stats) -> (usize, u64, usize, usize, usize, usize) {
    (
        stats.files,
        stats.bytes,
        stats.gif,
        stats.mov,
        stats.png,
        stats.webp,
    )
}

fn order_independent(roots: &[PathBuf], filter: Filter) -> (Vec<Source>, Stats) {
    let (files, summary) = scan::collect(roots, filter).unwrap();
    let baseline = snapshot(&files);
    for reverse in [false, true] {
        let mut reordered = roots.to_vec();
        if reverse {
            reordered.reverse();
        }
        for _ in 0..reordered.len() {
            reordered.rotate_left(1);
            let (actual, actual_stats) = scan::collect(&reordered, filter).unwrap();
            assert_eq!(snapshot(&actual), baseline, "roots: {reordered:?}");
            assert_eq!(stats(&actual_stats), stats(&summary));
        }
    }
    (files, summary)
}

fn progress(_: Progress) -> bool {
    true
}

fn options(out: PathBuf) -> PackOptions {
    PackOptions {
        preset: "fast".into(),
        out_dir: Some(out),
        ..Default::default()
    }
}

#[test]
fn a_single_file_uses_basename_and_repeated_paths_are_one_selection() {
    let tmp = tempfile::tempdir().unwrap();
    let file = write(tmp.path(), "parent/素材/笑.GIF", b"one selected GIF");
    let alias = file.parent().unwrap().join("./笑.GIF");
    let (files, summary) = order_independent(&[file.clone(), alias, file], Filter::All);
    assert_eq!(names(&files), expected(&["笑.GIF"]));
    assert_eq!(stats(&summary), (1, 16, 1, 0, 0, 0));
}

#[test]
fn selected_ancestors_cover_explicit_files_and_subdirectories_in_any_order() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("素材");
    let a = write(&root, "a.gif", b"gif");
    let b = write(&root, "nested/b.MOV", b"movie");
    write(&root, "nested/deep/c.webp", b"webp");
    write(&root, "nested/ignored.txt", b"ignored");
    let (files, summary) = order_independent(
        &[
            root.join("nested"),
            b,
            root.clone(),
            a,
            root.join("nested/deep"),
            root,
        ],
        Filter::All,
    );
    assert_eq!(
        names(&files),
        expected(&["a.gif", "nested/b.MOV", "nested/deep/c.webp"])
    );
    assert_eq!(stats(&summary), (3, 12, 1, 1, 0, 1));
}

#[test]
fn mixed_file_and_directory_roots_keep_unique_paths_and_stable_names() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("素材");
    let gif = write(&root, "跳.gif", b"gif");
    write(&root, "nested/跳.mov", b"mov");
    let still = write(tmp.path(), "other/one.png", b"still");
    let (files, summary) = order_independent(
        &[gif, still.clone(), root.join("nested"), root, still],
        Filter::All,
    );
    assert_eq!(
        names(&files),
        expected(&["素材/跳.gif", "素材/nested/跳.mov", "one.png"])
    );
    assert_eq!(stats(&summary), (3, 11, 1, 1, 1, 0));
}

#[test]
fn same_named_files_use_the_shortest_unique_parent_suffix() {
    let tmp = tempfile::tempdir().unwrap();
    let roots = [
        write(tmp.path(), "A/共用/笑.gif", b"first"),
        write(tmp.path(), "B/共用/笑.gif", b"second"),
        write(tmp.path(), "C/另名/笑.gif", b"third"),
        write(tmp.path(), "D/单独.gif", b"fourth"),
    ];
    let (files, summary) = order_independent(&roots, Filter::All);
    assert_eq!(
        names(&files),
        expected(&["A/共用/笑.gif", "B/共用/笑.gif", "另名/笑.gif", "单独.gif",])
    );
    assert_eq!(summary.files, 4);
    let original: BTreeSet<_> = roots.iter().map(|p| fs::canonicalize(p).unwrap()).collect();
    assert_eq!(
        files
            .iter()
            .map(|f| f.path.clone())
            .collect::<BTreeSet<_>>(),
        original
    );
}

#[test]
fn case_only_filename_collisions_are_disambiguated_for_portable_extraction() {
    let tmp = tempfile::tempdir().unwrap();
    let roots = [
        write(tmp.path(), "A/Smile.GIF", b"first"),
        write(tmp.path(), "B/smile.gif", b"second"),
    ];
    let (files, summary) = order_independent(&roots, Filter::All);
    assert_eq!(names(&files), expected(&["A/Smile.GIF", "B/smile.gif"]));
    assert_eq!(summary.files, 2);
}

#[test]
fn same_named_directories_preserve_the_existing_parent_suffix_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let gif = tmp.path().join("GIF/素材");
    let mov = tmp.path().join("MOV/素材");
    write(&gif, "笑.gif", b"gif");
    write(&mov, "笑.mov", b"mov");
    let (files, summary) = order_independent(&[gif, mov], Filter::All);
    assert_eq!(
        names(&files),
        expected(&["GIF/素材/笑.gif", "MOV/素材/笑.mov"])
    );
    assert_eq!(stats(&summary), (2, 6, 1, 1, 0, 0));
}

#[test]
fn expanded_root_prefixes_cannot_cover_another_selected_file() {
    for directory_name in ["clip.gif", "CLIP.GIF"] {
        let tmp = tempfile::tempdir().unwrap();
        let file = write(tmp.path(), "A/clip.gif", b"selected GIF source");
        let nested = tmp.path().join("B").join(directory_name).join("set");
        let other = tmp.path().join("C/set");
        let a = write(&nested, "a.mov", b"selected first MOV source");
        let b = write(&other, "b.mov", b"selected second MOV source");
        let originals: BTreeMap<_, _> = [&file, &a, &b]
            .into_iter()
            .map(|path| (fs::canonicalize(path).unwrap(), fs::read(path).unwrap()))
            .collect();
        let roots = [file, nested, other];
        let (selected, summary) = order_independent(&roots, Filter::All);
        let nested_name = format!("B/{directory_name}/set/a.mov");
        assert_eq!(
            names(&selected),
            expected(&["A/clip.gif", &nested_name, "C/set/b.mov"])
        );
        assert_eq!(summary.files, 3);

        let packed = container::pack(
            &roots,
            &options(tmp.path().join("packs")),
            &mut progress,
            None,
        )
        .unwrap();
        let manifest = container::read_manifest(&packed.pack_path).unwrap();
        assert_eq!(
            manifest
                .files
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<BTreeSet<_>>(),
            names(&selected)
        );
        let parent = tmp.path().join("restored");
        let unpacked =
            container::unpack(&packed.pack_path, Some(&parent), &mut progress, None).unwrap();
        assert_eq!(unpacked.n_files, 3);
        assert_eq!(unpacked.dir.parent(), Some(parent.as_path()));
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 1);
        for source in selected {
            let restored = fs::read(unpacked.dir.join(source.name)).unwrap();
            let original = &originals[&source.path];
            assert_eq!(blake3::hash(&restored), blake3::hash(original));
            assert_eq!(&restored, original);
        }
        for (path, original) in originals {
            assert_eq!(fs::read(path).unwrap(), original);
        }
    }
}

#[test]
fn archive_folder_name_uses_normalized_roots() {
    let tmp = tempfile::tempdir().unwrap();
    let directory = tmp.path().join("素材");
    let file = write(&directory, "笑.gif", b"generated GIF source");
    for (expected_name, selections) in [
        (
            "笑",
            vec![vec![file.clone()], vec![file.clone(), file.clone()]],
        ),
        (
            "素材",
            vec![
                vec![directory.clone()],
                vec![file.clone(), directory.clone(), directory],
            ],
        ),
    ] {
        for (index, selection) in selections.into_iter().enumerate() {
            let packed = container::pack(
                &selection,
                &options(tmp.path().join(format!("packs-{expected_name}-{index}"))),
                &mut progress,
                None,
            )
            .unwrap();
            let manifest = container::read_manifest(&packed.pack_path).unwrap();
            assert_eq!(manifest.dir_name, expected_name);
            assert_eq!(manifest.files.len(), 1);
            assert_eq!(manifest.files[0].name, "笑.gif");
        }
    }
    assert_eq!(fs::read(file).unwrap(), b"generated GIF source");
}

#[test]
fn unsupported_direct_files_are_ignored_and_filters_keep_other_valid_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let unsupported = write(tmp.path(), "notes.txt", b"not media");
    let gif = write(tmp.path(), "g.GIF", b"g");
    let mov = write(tmp.path(), "m.mov", b"mm");
    let png = write(tmp.path(), "p.png", b"ppp");
    let webp = write(tmp.path(), "w.webp", b"wwww");
    let roots = [unsupported.clone(), gif, mov, png, webp];
    for (filter, selected, summary) in [
        (
            Filter::All,
            vec!["g.GIF", "m.mov", "p.png", "w.webp"],
            (4, 10, 1, 1, 1, 1),
        ),
        (Filter::Gif, vec!["g.GIF"], (1, 1, 1, 0, 0, 0)),
        (Filter::Mov, vec!["m.mov"], (1, 2, 0, 1, 0, 0)),
        (
            Filter::NoGif,
            vec!["m.mov", "p.png", "w.webp"],
            (3, 9, 0, 1, 1, 1),
        ),
        (
            Filter::NoMov,
            vec!["g.GIF", "p.png", "w.webp"],
            (3, 8, 1, 0, 1, 1),
        ),
    ] {
        let (files, actual) = order_independent(&roots, filter);
        assert_eq!(names(&files), expected(&selected));
        assert_eq!(stats(&actual), summary);
    }
    let (files, actual) = scan::collect(&[unsupported], Filter::All).unwrap();
    assert!(files.is_empty());
    assert_eq!(actual.files, 0);
}

#[test]
fn mixed_selection_packs_each_path_once_and_preserves_all_source_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("素材");
    let gif = write(&dir, "a.gif", b"generated gif placeholder");
    let mov = write(&dir, "nested/b.mov", b"generated mov placeholder");
    let ignored = write(&dir, "notes.txt", b"untouched unsupported source");
    let png = write(tmp.path(), "loose/still.png", b"generated png placeholder");
    let originals: BTreeMap<_, _> = [&gif, &mov, &ignored, &png]
        .into_iter()
        .map(|p| (p.clone(), fs::read(p).unwrap()))
        .collect();
    let roots = [gif, png.clone(), dir.join("nested"), dir.clone(), png, dir];
    let (selected, _) = scan::collect(&roots, Filter::All).unwrap();
    assert_eq!(selected.len(), 3);
    let pack = container::pack(
        &roots,
        &options(tmp.path().join("packs")),
        &mut progress,
        None,
    )
    .unwrap();
    assert_eq!(pack.n_files, 3);
    let manifest = container::read_manifest(&pack.pack_path).unwrap();
    assert_eq!(manifest.files.len(), 3);
    assert_eq!(
        manifest
            .files
            .iter()
            .map(|e| e.name.clone())
            .collect::<BTreeSet<_>>(),
        names(&selected)
    );
    let unpack = container::unpack(
        &pack.pack_path,
        Some(&tmp.path().join("restored")),
        &mut progress,
        None,
    )
    .unwrap();
    assert_eq!(unpack.n_files, 3);
    for source in selected {
        assert_eq!(
            fs::read(unpack.dir.join(source.name)).unwrap(),
            fs::read(source.path).unwrap()
        );
    }
    for (path, bytes) in originals {
        let after = fs::read(path).unwrap();
        assert_eq!(blake3::hash(&after), blake3::hash(&bytes));
        assert_eq!(after, bytes);
    }
}

#[test]
fn flat_direct_files_unpack_into_new_folders_without_scattering_or_overwriting() {
    let tmp = tempfile::tempdir().unwrap();
    let roots = [
        write(tmp.path(), "source/a.gif", b"flat GIF"),
        write(tmp.path(), "source/b.mov", b"flat MOV"),
        write(tmp.path(), "source/c.webp", b"flat WEBP"),
    ];
    let pack = container::pack(
        &roots,
        &options(tmp.path().join("packs")),
        &mut progress,
        None,
    )
    .unwrap();
    let manifest = container::read_manifest(&pack.pack_path).unwrap();
    assert!(manifest.files.iter().all(|f| !f.name.contains('/')));
    let parent = tmp.path().join("destination");
    let sentinel = write(&parent, "sentinel.txt", b"keep existing content");
    let first = container::unpack(&pack.pack_path, Some(&parent), &mut progress, None).unwrap();
    assert_eq!(first.dir.parent(), Some(parent.as_path()));
    assert_ne!(first.dir, parent);
    assert_eq!(fs::read_dir(&parent).unwrap().count(), 2);
    assert_eq!(fs::read_dir(&first.dir).unwrap().count(), 3);
    fs::write(
        first.dir.join("a.gif"),
        b"user changed the first extraction",
    )
    .unwrap();
    let second = container::unpack(&pack.pack_path, Some(&parent), &mut progress, None).unwrap();
    assert_eq!(second.dir.parent(), Some(parent.as_path()));
    assert_ne!(first.dir, second.dir);
    assert_eq!(fs::read_dir(&parent).unwrap().count(), 3);
    assert_eq!(fs::read_dir(&second.dir).unwrap().count(), 3);
    assert_eq!(fs::read(sentinel).unwrap(), b"keep existing content");
    assert_eq!(
        fs::read(first.dir.join("a.gif")).unwrap(),
        b"user changed the first extraction"
    );
    for source in roots {
        let name = source.file_name().unwrap();
        assert!(
            !parent.join(name).exists(),
            "unpack scattered a file into its parent"
        );
        assert_eq!(
            fs::read(second.dir.join(name)).unwrap(),
            fs::read(source).unwrap()
        );
    }
}
