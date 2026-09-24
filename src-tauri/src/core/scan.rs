use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Component, Path, PathBuf},
};

pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mov", "mp4", "m4v", "mkv", "webm", "avi", "wmv", "flv", "mpg", "mpeg", "m2v", "ts", "mts",
    "m2ts", "3gp", "3g2", "ogv", "vob", "mxf",
];

pub fn is_video(extension: &str) -> bool {
    VIDEO_EXTENSIONS
        .iter()
        .any(|supported| supported.eq_ignore_ascii_case(extension))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Filter {
    #[default]
    All,
    Gif,
    Video,
    Mov,
    NoGif,
    NoVideo,
    NoMov,
}
impl Filter {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "all" => Ok(Self::All),
            "gif" => Ok(Self::Gif),
            "video" => Ok(Self::Video),
            "mov" => Ok(Self::Mov),
            "no-gif" => Ok(Self::NoGif),
            "no-video" => Ok(Self::NoVideo),
            "no-mov" => Ok(Self::NoMov),
            _ => Err(crate::locale::message(
                "error.unsupported_filter",
                &[(s).to_string()],
            )),
        }
    }
    pub fn accepts(self, extension: &str) -> bool {
        let extension = extension.to_ascii_lowercase();
        let video = is_video(&extension);
        if !video && !matches!(extension.as_str(), "gif" | "png" | "webp") {
            return false;
        }
        match self {
            Self::All => true,
            Self::Gif => extension == "gif",
            Self::Video => video,
            Self::Mov => extension == "mov",
            Self::NoGif => extension != "gif",
            Self::NoVideo => !video,
            Self::NoMov => extension != "mov",
        }
    }
}
#[derive(Debug, Default, Clone, Serialize)]
pub struct Stats {
    pub files: usize,
    pub bytes: u64,
    pub gif: usize,
    pub video: usize,
    pub mov: usize,
    pub png: usize,
    pub webp: usize,
}
#[derive(Debug, Clone)]
pub struct Source {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
}
pub fn extension(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

pub(crate) fn normalize_roots(dirs: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    if dirs.is_empty() {
        return Err("error.selection_empty".into());
    }
    let mut roots = Vec::new();
    let mut directories = HashSet::new();
    for d in dirs {
        let p = fs::canonicalize(d).map_err(|e| format!("{}: {e}", d.display()))?;
        let metadata = fs::metadata(&p).map_err(|e| format!("{}: {e}", d.display()))?;
        if metadata.is_dir() {
            directories.insert(p.clone());
        } else if !metadata.is_file() {
            return Err(crate::locale::message(
                "error.unsupported_file_type",
                &[(d.display()).to_string()],
            ));
        }
        if !roots.contains(&p) {
            roots.push(p);
        }
    }
    roots.retain(|p| {
        !directories
            .iter()
            .any(|parent| p != parent && p.starts_with(parent))
    });
    roots.sort();
    Ok(roots)
}

fn root_prefixes(roots: &[PathBuf], single_directory: bool) -> Result<Vec<String>, String> {
    if single_directory {
        return Ok(vec![String::new()]);
    }
    let components: Vec<Vec<_>> = roots
        .iter()
        .map(|root| {
            root.components()
                .filter_map(|c| match c {
                    Component::Normal(name) => Some(name),
                    _ => None,
                })
                .collect()
        })
        .collect();
    if components.iter().any(Vec::is_empty) {
        return Err("error.multiple_disk_roots".into());
    }
    let mut depths = vec![1; roots.len()];
    loop {
        let prefixes: Vec<String> = components
            .iter()
            .zip(&depths)
            .map(|(parts, &depth)| {
                parts[parts.len() - depth..]
                    .iter()
                    .map(|part| part.to_str().ok_or("error.filename_encoding"))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|parts| parts.join("/"))
            })
            .collect::<Result<_, _>>()?;
        let folded: Vec<Vec<_>> = prefixes
            .iter()
            .map(|prefix| prefix.split('/').map(str::to_lowercase).collect())
            .collect();
        let mut conflicts = vec![false; roots.len()];
        for i in 0..folded.len() {
            for j in i + 1..folded.len() {
                if folded[i].starts_with(&folded[j]) || folded[j].starts_with(&folded[i]) {
                    conflicts[i] = true;
                    conflicts[j] = true;
                }
            }
        }
        if !conflicts.iter().any(|&conflict| conflict) {
            return Ok(prefixes);
        }
        for (i, conflict) in conflicts.into_iter().enumerate() {
            if conflict {
                if depths[i] == components[i].len() {
                    return Err("error.selection_ambiguous".into());
                }
                depths[i] += 1;
            }
        }
    }
}

fn validate_names(files: &[Source]) -> Result<(), String> {
    let mut names = HashSet::new();
    for file in files {
        super::container::safe_rel_path(&file.name)?;
        if !names.insert(file.name.to_lowercase()) {
            return Err(crate::locale::message(
                "error.duplicate_target",
                &[(file.name).to_string()],
            ));
        }
    }
    let mut directories = HashMap::new();
    for file in files {
        for (end, _) in file.name.match_indices('/') {
            let parent = &file.name[..end];
            let folded = parent.to_lowercase();
            if names.contains(&folded) {
                return Err(crate::locale::message(
                    "error.path_conflict_at",
                    &[(parent).to_string()],
                ));
            }
            if let Some(previous) = directories.insert(folded, parent) {
                if previous != parent {
                    return Err(crate::locale::message(
                        "error.path_case_conflict",
                        &[(previous).to_string(), (parent).to_string()],
                    ));
                }
            }
        }
    }
    Ok(())
}

pub fn collect(dirs: &[PathBuf], filter: Filter) -> Result<(Vec<Source>, Stats), String> {
    let roots = normalize_roots(dirs)?;
    let metadata = roots
        .iter()
        .map(|root| fs::metadata(root).map_err(|e| format!("{}: {e}", root.display())))
        .collect::<Result<Vec<_>, _>>()?;
    let prefixes = root_prefixes(&roots, roots.len() == 1 && metadata[0].is_dir())?;
    let mut files = Vec::new();
    for ((root, metadata), prefix) in roots.iter().zip(&metadata).zip(prefixes) {
        if metadata.is_dir() {
            walk(root, &prefix, filter, &mut files)?;
        } else if filter.accepts(&extension(&prefix)) {
            files.push(Source {
                name: prefix,
                path: root.clone(),
                bytes: metadata.len(),
            });
        }
    }
    files.sort_by_cached_key(|f| {
        (
            extension(&f.name),
            Path::new(&f.name)
                .parent()
                .map(|p| p.to_string_lossy().to_string()),
            f.name.clone(),
        )
    });
    validate_names(&files)?;
    let mut stats = Stats::default();
    for f in &files {
        stats.bytes = stats
            .bytes
            .checked_add(f.bytes)
            .ok_or("error.source_size_overflow")?;
        stats.files += 1;
        if is_video(&extension(&f.name)) {
            stats.video += 1;
        }
        match extension(&f.name).as_str() {
            "gif" => stats.gif += 1,
            "mov" => stats.mov += 1,
            "png" => stats.png += 1,
            "webp" => stats.webp += 1,
            _ => {}
        }
    }
    Ok((files, stats))
}
fn walk(dir: &Path, prefix: &str, filter: Filter, files: &mut Vec<Source>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let e = entry.map_err(|e| e.to_string())?;
        let ft = e.file_type().map_err(|e| e.to_string())?;
        if ft.is_symlink() {
            continue;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if e.metadata().map_err(|e| e.to_string())?.file_attributes() & 0x400 != 0 {
                if ft.is_dir() && fs::read_link(e.path()).is_ok() {
                    continue;
                }
            }
        }
        let name = e
            .file_name()
            .into_string()
            .map_err(|_| "error.filename_encoding")?;
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if ft.is_dir() {
            walk(&e.path(), &rel, filter, files)?;
        } else if ft.is_file() && filter.accepts(&extension(&name)) {
            files.push(Source {
                name: rel,
                path: e.path(),
                bytes: e.metadata().map_err(|e| e.to_string())?.len(),
            });
        }
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    fn sources(names: &[&str]) -> Vec<Source> {
        names
            .iter()
            .map(|name| Source {
                name: (*name).into(),
                path: PathBuf::from(name),
                bytes: 0,
            })
            .collect()
    }

    #[test]
    fn selection_rejects_internal_case_and_file_directory_conflicts() {
        for names in [
            vec!["set/a.gif", "set/A.GIF"],
            vec!["set/clip.gif", "set/CLIP.GIF/a.mov"],
            vec!["set/CLIP.GIF/a.mov", "set/clip.gif"],
            vec!["set/Model/a.gif", "set/model/b.mov"],
            vec!["Set/nested/a.gif", "set/nested/b.mov"],
        ] {
            assert!(validate_names(&sources(&names)).is_err(), "{names:?}");
        }
        assert!(validate_names(&sources(&[
            "set/model/a.gif",
            "set/model/b.mov",
            "set/clip.gif/a.png",
            "set/clip.gif.other"
        ]))
        .is_ok());
    }

    #[test]
    fn selection() {
        assert!(Filter::All.accepts("png"));
        assert!(!Filter::All.accepts("exe"));
        assert!(!Filter::NoMov.accepts("mov"));
        assert!(Filter::NoMov.accepts("webp"));
        assert!(Filter::Gif.accepts("gif"));
        assert!(!Filter::Gif.accepts("png"));
        assert!(!Filter::NoGif.accepts("gif"));
        assert!(Filter::NoGif.accepts("mov"));
    }
}
