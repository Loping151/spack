use super::{
    scan::{self, Filter},
    volumes,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

pub const MAGIC: &[u8; 8] = b"SPACK\0\x02\0";
const MAX_MANIFEST: usize = 64 << 20;
const MAX_RECORD: u64 = 512 << 20;
const TRANSFORM_LIMIT: u64 = 256 << 20;
pub type Cancel = Option<Arc<AtomicBool>>;
pub type ProgressFn<'a> = &'a mut dyn FnMut(Progress) -> bool;
#[derive(Clone, Serialize)]
pub struct Progress {
    pub phase: &'static str,
    pub done: u64,
    pub total: u64,
    pub detail: String,
}
pub fn tick(
    cb: ProgressFn,
    cancel: &Cancel,
    phase: &'static str,
    done: u64,
    total: u64,
    detail: impl Into<String>,
) -> Result<(), String> {
    if cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed))
        || !cb(Progress {
            phase,
            done,
            total,
            detail: detail.into(),
        })
        || cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed))
    {
        Err("error.cancelled".into())
    } else {
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Method {
    Raw,
    GifExact,
    GifModel,
    GifPredict,
    MovExact,
    MovModel,
    MovShared,
    MovMotion,
    MovSource,
    Duplicate,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub method: Method,
    pub size: u64,
    pub stored: u64,
    pub hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<usize>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub dir_name: String,
    pub src_bytes: u64,
    pub source_hash: String,
    pub preset: String,
    pub files: Vec<Entry>,
}
#[derive(Clone, Debug)]
pub struct PackOptions {
    pub filter: Filter,
    pub preset: String,
    pub out_dir: Option<PathBuf>,
    pub split: volumes::Split,
    pub raw: bool,
}
impl Default for PackOptions {
    fn default() -> Self {
        Self {
            filter: Filter::All,
            preset: "balanced".into(),
            out_dir: None,
            split: volumes::Split::None,
            raw: false,
        }
    }
}
#[derive(Debug, Serialize)]
pub struct PackResult {
    pub pack_path: PathBuf,
    pub parts: Vec<PathBuf>,
    pub src_bytes: u64,
    pub packed_bytes: u64,
    pub n_files: usize,
}
#[derive(Debug, Serialize)]
pub struct UnpackResult {
    pub dir: PathBuf,
    pub n_files: usize,
    pub bytes_written: u64,
}
#[derive(Serialize)]
pub struct ArchiveInfo {
    pub files: usize,
    pub bytes: u64,
    pub dir_name: String,
    pub parts: usize,
}
fn io(e: std::io::Error) -> String {
    e.to_string()
}
fn preset(p: &str) -> Result<(i32, u32), String> {
    match p {
        "fast" => Ok((6, 27)),
        "balanced" => Ok((19, 28)),
        "max" => Ok((22, 30)),
        _ => Err("error.invalid_preset".into()),
    }
}

pub fn safe_rel_path(name: &str) -> Result<PathBuf, String> {
    if name.is_empty() || name.len() > 32760 {
        return Err("error.invalid_path".into());
    }
    for part in name.split('/') {
        let stem = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.ends_with(['.', ' '])
            || part.chars().any(|c| c < ' ' || "\\:*?\"<>|".contains(c))
            || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit())
        {
            return Err(crate::locale::message(
                "error.unsafe_path",
                &[(name).to_string()],
            ));
        }
    }
    Ok(PathBuf::from(name))
}
fn safe_base(s: &str) -> String {
    let name: String = s
        .chars()
        .map(|c| {
            if c < ' ' || "\\/:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let name = name.trim_matches(['.', ' ']);
    if safe_rel_path(name).is_ok() && !name.contains('/') {
        name.to_string()
    } else {
        "media".into()
    }
}
fn source_hash(entries: &[Entry]) -> String {
    let mut h = blake3::Hasher::new();
    for e in entries {
        h.update(&(e.name.len() as u64).to_le_bytes());
        h.update(e.name.as_bytes());
        h.update(&e.size.to_le_bytes());
        h.update(e.hash.as_bytes());
    }
    h.finalize().to_hex().to_string()
}
fn gif_model_candidate(sgif: &[u8], notify: &mut dyn FnMut(u64, u64) -> bool) -> Option<Vec<u8>> {
    let model = super::gif_model::encode_transformed_with_progress(sgif, notify).ok()?;
    let plain = zstd::bulk::compress(sgif, 6)
        .map(|v| v.len())
        .unwrap_or(sgif.len());
    (model.len() < plain).then_some(model)
}

fn select_transform(
    raw: &[u8],
    ext: &str,
    opts: &PackOptions,
    paired_mov: Option<&[u8]>,
    mov_context: &mut super::mov_model::SharedContext,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<(Method, Vec<u8>), String> {
    if opts.raw || (ext == "mov" && opts.preset == "fast") {
        return Ok((Method::Raw, raw.to_vec()));
    }
    let active = std::cell::Cell::new(true);
    let predicts_gif = ext == "gif" && opts.preset != "fast" && paired_mov.is_some();
    let predicts_motion = ext == "mov" && opts.preset == "max";
    let phase_start = std::cell::Cell::new(0u64);
    let phase_span = std::cell::Cell::new(if predicts_gif {
        1500u64
    } else if predicts_motion {
        3500u64
    } else {
        9500u64
    });
    let mut notify = |done: u64, total: u64| {
        let fraction = if total == 0 {
            0
        } else {
            (phase_span.get() as u128 * done.min(total) as u128 / total as u128) as u64
        };
        active.set(active.get() && progress(phase_start.get() + fraction, 10000));
        active.get()
    };
    let mut candidate_context = (ext == "mov").then(|| mov_context.clone());
    let mut transformed = match ext {
        "gif" => super::gif_exact::encode(raw).map(|sgif| {
            if opts.preset != "fast" && !predicts_gif {
                if let Some(model) = gif_model_candidate(&sgif, &mut notify) {
                    return (Method::GifModel, model);
                }
            }
            (Method::GifExact, sgif)
        }),
        "mov" => super::mov_model::encode_shared_with_progress(
            raw,
            candidate_context.as_mut().unwrap(),
            &mut notify,
        )
        .map(|v| (Method::MovShared, v)),
        _ => return Ok((Method::Raw, raw.to_vec())),
    };
    if !active.get() {
        return Err("error.cancelled".into());
    }
    if predicts_gif {
        phase_start.set(1500);
        phase_span.set(8000);
        if let Some(mov) = paired_mov {
            let size3 = |d: &[u8]| {
                zstd::bulk::compress(d, 3)
                    .map(|v| v.len())
                    .unwrap_or(usize::MAX)
            };
            let plain = transformed
                .as_ref()
                .ok()
                .map(|(_, d)| size3(d))
                .unwrap_or(usize::MAX);
            let predicted = super::gif_predict::encode(raw, mov, &mut notify)
                .ok()
                .filter(|p| p.len() as u64 <= MAX_RECORD)
                .map(|p| (size3(&p), p));
            let clear_win = predicted
                .as_ref()
                .is_some_and(|(n, _)| *n * 10 < plain.saturating_mul(6));
            let model = if clear_win || !active.get() {
                None
            } else if let Ok((_, sgif)) = transformed.as_ref() {
                gif_model_candidate(sgif, &mut notify).map(|m| (size3(&m), m))
            } else {
                None
            };
            match (predicted, model) {
                (Some((pn, p)), Some((mn, _))) if pn < mn && pn < plain => {
                    transformed = Ok((Method::GifPredict, p))
                }
                (Some((pn, p)), None) if pn < plain => transformed = Ok((Method::GifPredict, p)),
                (_, Some((_, m))) => transformed = Ok((Method::GifModel, m)),
                _ => {}
            }
        }
    }
    if !active.get() {
        return Err("error.cancelled".into());
    }
    if predicts_motion {
        phase_start.set(3500);
        phase_span.set(6000);
        if let Ok(predicted) = super::mov_motion::encode_with_progress(raw, &mut notify) {
            let baseline = transformed
                .as_ref()
                .ok()
                .and_then(|(_, data)| zstd::bulk::compress(data, 3).ok().map(|v| v.len()))
                .unwrap_or(usize::MAX);
            let candidate = zstd::bulk::compress(&predicted, 3)
                .map(|v| v.len())
                .unwrap_or(usize::MAX);
            if predicted.len() as u64 <= MAX_RECORD && candidate < baseline {
                transformed = Ok((Method::MovMotion, predicted));
            }
        }
    }
    if !active.get() {
        return Err("error.cancelled".into());
    }
    phase_start.set(9500);
    phase_span.set(500);
    if let Ok((method, data)) = transformed {
        if data.len() as u64 > MAX_RECORD {
            return Ok((Method::Raw, raw.to_vec()));
        }
        let verified = match method {
            Method::GifExact => super::gif_exact::decode(&data).as_deref() == Ok(raw),
            Method::GifModel => {
                super::gif_model::decode_with_progress(&data, &mut notify).as_deref() == Ok(raw)
            }
            Method::MovShared | Method::MovMotion | Method::GifPredict => true,
            _ => unreachable!(),
        };
        if !active.get() {
            return Err("error.cancelled".into());
        }
        if !verified {
            return Ok((Method::Raw, raw.to_vec()));
        }
        let original = zstd::bulk::compress(raw, 3)
            .map(|v| v.len())
            .unwrap_or(raw.len());
        let candidate = zstd::bulk::compress(&data, 3)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if candidate < original {
            if method == Method::MovShared {
                *mov_context = candidate_context.unwrap();
            }
            return Ok((method, data));
        }
    }
    Ok((Method::Raw, raw.to_vec()))
}

fn pair_mov_sources(sources: &[scan::Source]) -> Vec<Option<usize>> {
    let mut by_stem: HashMap<String, Vec<usize>> = HashMap::new();
    let stem = |path: &Path| path.file_stem().map(|s| s.to_string_lossy().to_lowercase());
    for (i, source) in sources.iter().enumerate() {
        if scan::extension(&source.name) == "mov" && source.bytes <= TRANSFORM_LIMIT {
            if let Some(key) = stem(&source.path) {
                by_stem.entry(key).or_default().push(i);
            }
        }
    }
    sources
        .iter()
        .enumerate()
        .map(|(i, source)| {
            if scan::extension(&source.name) != "gif" {
                return None;
            }
            let candidates = by_stem.get(&stem(&source.path)?)?;
            let parent = source.path.parent();
            for tier in 0..3 {
                let mut matching = candidates.iter().copied().filter(|&j| {
                    j < i
                        && match tier {
                            0 => sources[j].path.parent() == parent,
                            1 => {
                                sources[j].path.parent().and_then(Path::file_name)
                                    == parent.and_then(Path::file_name)
                            }
                            _ => true,
                        }
                });
                if let Some(first) = matching.next() {
                    return if matching.next().is_none() {
                        Some(first)
                    } else {
                        None
                    };
                }
            }
            None
        })
        .collect()
}

fn try_mov_source(
    raw: &[u8],
    pool: &super::mov_source::Pool,
    opts: &PackOptions,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Option<Vec<u8>>, String> {
    let data = match super::mov_source::encode(raw, pool, opts.preset != "max", &mut *progress) {
        Ok(data) => data,
        Err(e) if e == "error.cancelled" => return Err(e),
        Err(_) => return Ok(None),
    };
    match super::mov_source::decode(&data, pool, false, &mut *progress) {
        Err(e) if e == "error.cancelled" => Err(e),
        Ok((bytes, _))
            if bytes == raw && data.len() as u64 <= MAX_RECORD && data.len() < raw.len() =>
        {
            Ok(Some(data))
        }
        _ => Ok(None),
    }
}

fn read_source(src: &scan::Source) -> Result<Vec<u8>, String> {
    let mut file = File::open(&src.path).map_err(|e| format!("{}: {e}", src.name))?;
    if file.metadata().map_err(io)?.len() != src.bytes {
        return Err(crate::locale::message(
            "error.source_changed_scan",
            &[(src.name).to_string()],
        ));
    }
    let mut raw = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(TRANSFORM_LIMIT + 1)
        .read_to_end(&mut raw)
        .map_err(io)?;
    if raw.len() as u64 != src.bytes {
        return Err(crate::locale::message(
            "error.source_changed_read",
            &[(src.name).to_string()],
        ));
    }
    Ok(raw)
}

fn pool_after(
    sources: &[scan::Source],
    j: Option<usize>,
) -> Result<super::mov_source::Pool, String> {
    match j {
        Some(j) => Ok(super::mov_source::pool_of(&read_source(&sources[j])?).unwrap_or_default()),
        None => Ok(super::mov_source::Pool::default()),
    }
}

struct Prepared {
    hash: String,
    result: Option<(Method, Vec<u8>)>,
    pool_from: Option<usize>,
    paired_hash: Option<String>,
}

fn prepare(
    i: usize,
    sources: &[scan::Source],
    paired: &[Option<usize>],
    opts: &PackOptions,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Prepared, String> {
    let src = &sources[i];
    let raw = read_source(src)?;
    let hash = blake3::hash(&raw).to_hex().to_string();
    let ext = scan::extension(&src.name);
    let transforms = !opts.raw && opts.preset != "fast";
    if ext == "mov" && transforms {
        let previous = (i > 0
            && scan::extension(&sources[i - 1].name) == "mov"
            && sources[i - 1].bytes <= TRANSFORM_LIMIT)
            .then(|| i - 1);
        let prev_raw = match previous {
            Some(j) => Some(read_source(&sources[j])?),
            None => None,
        };
        let (pool, pool_from) = match (previous, prev_raw) {
            (Some(j), Some(pr)) if super::mov_source::shares_content(&raw, &pr) => {
                match super::mov_source::pool_of(&pr) {
                    Ok(pool) => (pool, Some(j)),
                    Err(_) => (super::mov_source::Pool::default(), None),
                }
            }
            _ => (super::mov_source::Pool::default(), None),
        };
        let result = try_mov_source(&raw, &pool, opts, progress)?.map(|d| (Method::MovSource, d));
        return Ok(Prepared {
            hash,
            result,
            pool_from,
            paired_hash: None,
        });
    }
    if ext == "mov" {
        return Ok(Prepared {
            hash,
            result: Some((Method::Raw, raw)),
            pool_from: None,
            paired_hash: None,
        });
    }
    let paired_mov = match paired[i] {
        Some(p) => Some(read_source(&sources[p])?),
        None => None,
    };
    let paired_hash = paired_mov
        .as_ref()
        .map(|b| blake3::hash(b).to_hex().to_string());
    let mut unused = super::mov_model::SharedContext::default();
    let result = select_transform(
        &raw,
        &ext,
        opts,
        paired_mov.as_deref(),
        &mut unused,
        progress,
    )?;
    Ok(Prepared {
        hash,
        result: Some(result),
        pool_from: None,
        paired_hash,
    })
}

pub fn pack(
    dirs: &[PathBuf],
    opts: &PackOptions,
    cb: ProgressFn,
    cancel: Cancel,
) -> Result<PackResult, String> {
    let (level, window) = preset(&opts.preset)?;
    opts.split.validate()?;
    tick(cb, &cancel, "scan", 0, 0, "")?;
    let roots = scan::normalize_roots(dirs)?;
    let (mut sources, stats) = scan::collect(dirs, opts.filter)?;
    if sources.is_empty() {
        return Err("error.no_matching_files".into());
    }
    let paired = if !opts.raw && opts.preset != "fast" {
        sources.sort_by_key(|s| scan::extension(&s.name) != "mov");
        pair_mov_sources(&sources)
    } else {
        vec![None; sources.len()]
    };
    let out_dir = opts
        .out_dir
        .clone()
        .unwrap_or_else(|| dirs[0].parent().unwrap_or(Path::new(".")).to_path_buf());
    fs::create_dir_all(&out_dir).map_err(io)?;
    let scratch = tempfile::Builder::new()
        .prefix(".spack-")
        .tempdir_in(&out_dir)
        .map_err(io)?;
    let payload_path = scratch.path().join("records");
    let mut payload = File::create(&payload_path).map_err(io)?;
    let mut entries: Vec<Entry> = Vec::new();
    let mut duplicates = HashMap::new();
    let mut processed = 0;
    let mut mov_context = super::mov_model::SharedContext::default();
    let n = sources.len();
    let workers = std::thread::available_parallelism()
        .map_or(2, |x| x.get() / 4)
        .clamp(2, 4);
    let lookahead = workers * 2;
    let abort = AtomicBool::new(false);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let partial: Vec<std::sync::atomic::AtomicU64> = (0..n)
        .map(|_| std::sync::atomic::AtomicU64::new(0))
        .collect();
    let state = std::sync::Mutex::new((
        0usize,
        (0..n)
            .map(|_| None)
            .collect::<Vec<Option<Result<Prepared, String>>>>(),
    ));
    let ready = std::sync::Condvar::new();
    let stop = || {
        abort.load(Ordering::Relaxed) || cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed))
    };
    let budget = std::thread::available_parallelism()
        .map_or(4, |x| x.get())
        .div_ceil(workers)
        .max(2);
    let result = std::thread::scope(|scope| -> Result<(), String> {
        for _ in 0..workers {
            scope.spawn(|| {
                super::mov_source::with_thread_budget(budget, || loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    {
                        let mut st = state.lock().unwrap();
                        while i >= st.0 + lookahead && !stop() {
                            st = ready
                                .wait_timeout(st, std::time::Duration::from_millis(200))
                                .unwrap()
                                .0;
                        }
                    }
                    if stop() {
                        break;
                    }
                    let prepared = if sources[i].bytes > TRANSFORM_LIMIT {
                        Ok(Prepared {
                            hash: String::new(),
                            result: None,
                            pool_from: None,
                            paired_hash: None,
                        })
                    } else {
                        prepare(i, &sources, &paired, opts, &mut |done, total| {
                            let bytes = if total == 0 {
                                0
                            } else {
                                (sources[i].bytes as u128 * done.min(total) as u128 / total as u128)
                                    as u64
                            };
                            partial[i].store(
                                bytes.min(sources[i].bytes.saturating_sub(1)),
                                Ordering::Relaxed,
                            );
                            !stop()
                        })
                    };
                    state.lock().unwrap().1[i] = Some(prepared);
                    ready.notify_all();
                })
            });
        }
        let mut writer = || -> Result<(), String> {
            let mut pool_state: Option<usize> = None;
            for (source_index, src) in sources.iter().enumerate() {
                let prepared = loop {
                    let in_flight: u64 = partial[source_index..(source_index + lookahead).min(n)]
                        .iter()
                        .map(|p| p.load(Ordering::Relaxed))
                        .sum();
                    tick(
                        cb,
                        &cancel,
                        "transform",
                        processed + in_flight,
                        stats.bytes,
                        &src.name,
                    )?;
                    let mut st = state.lock().unwrap();
                    if let Some(p) = st.1[source_index].take() {
                        break p?;
                    }
                    let _ = ready
                        .wait_timeout(st, std::time::Duration::from_millis(200))
                        .unwrap();
                };
                let size = src.bytes;
                let is_mov = scan::extension(&src.name) == "mov";
                let (method, stored, hash, reference) = if size <= TRANSFORM_LIMIT {
                    let hash = prepared.hash;
                    if let Some(&index) = duplicates.get(&(hash.clone(), size)) {
                        (Method::Duplicate, 0, hash, Some(index))
                    } else {
                        let (method, data) = match prepared.result {
                            Some((Method::MovSource, data))
                                if prepared.pool_from.is_none()
                                    || prepared.pool_from == pool_state =>
                            {
                                (Method::MovSource, data)
                            }
                            Some(result) if !is_mov || result.0 != Method::MovSource => result,
                            other => {
                                let raw = read_source(src)?;
                                if blake3::hash(&raw).to_hex().as_str() != hash {
                                    return Err(crate::locale::message(
                                        "error.source_changed_pack",
                                        &[(src.name).to_string()],
                                    ));
                                }
                                let mut intra = |_: u64, _: u64| {
                                    tick(
                                        cb,
                                        &cancel,
                                        "transform",
                                        processed,
                                        stats.bytes,
                                        &src.name,
                                    )
                                    .is_ok()
                                };
                                let retry = if other.is_some() || prepared.pool_from != pool_state {
                                    try_mov_source(
                                        &raw,
                                        &pool_after(&sources, pool_state)?,
                                        opts,
                                        &mut intra,
                                    )?
                                } else {
                                    None
                                };
                                match retry {
                                    Some(data) => (Method::MovSource, data),
                                    None => select_transform(
                                        &raw,
                                        "mov",
                                        opts,
                                        None,
                                        &mut mov_context,
                                        &mut intra,
                                    )?,
                                }
                            }
                        };
                        if let (Some(p), Some(h)) = (paired[source_index], &prepared.paired_hash) {
                            if entries[p].hash != *h {
                                return Err(crate::locale::message(
                                    "error.source_changed_pack",
                                    &[(sources[p].name).to_string()],
                                ));
                            }
                        }
                        payload.write_all(&data).map_err(io)?;
                        duplicates.insert((hash.clone(), size), entries.len());
                        let reference = if method == Method::GifPredict {
                            paired[source_index]
                        } else {
                            None
                        };
                        (method, data.len() as u64, hash, reference)
                    }
                } else {
                    let mut file =
                        File::open(&src.path).map_err(|e| format!("{}: {e}", src.name))?;
                    let mut hash = blake3::Hasher::new();
                    let mut count = 0;
                    let mut buf = vec![0; 1 << 20];
                    loop {
                        let n = file.read(&mut buf).map_err(io)?;
                        if n == 0 {
                            break;
                        }
                        payload.write_all(&buf[..n]).map_err(io)?;
                        hash.update(&buf[..n]);
                        count += n as u64;
                        tick(
                            cb,
                            &cancel,
                            "transform",
                            processed + count,
                            stats.bytes,
                            &src.name,
                        )?;
                    }
                    if count != size {
                        return Err(crate::locale::message(
                            "error.source_changed_read",
                            &[(src.name).to_string()],
                        ));
                    }
                    (
                        Method::Raw,
                        size,
                        hash.finalize().to_hex().to_string(),
                        None,
                    )
                };
                if is_mov {
                    pool_state = (method == Method::MovSource).then_some(source_index);
                }
                entries.push(Entry {
                    name: src.name.clone(),
                    method,
                    size,
                    stored,
                    hash,
                    reference,
                });
                processed += size;
                state.lock().unwrap().0 = source_index + 1;
                ready.notify_all();
                tick(cb, &cancel, "transform", processed, stats.bytes, &src.name)?;
            }
            Ok(())
        };
        let written = writer();
        if written.is_err() {
            abort.store(true, Ordering::Relaxed);
            ready.notify_all();
        }
        written
    });
    result?;
    payload.sync_all().map_err(io)?;
    let payload_size = payload.metadata().map_err(io)?.len();
    drop(payload);
    let root_name = if roots.len() == 1 && roots[0].is_file() {
        roots[0]
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "media".into())
    } else if roots.iter().all(|d| d.file_name() == roots[0].file_name()) {
        roots[0]
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "media".into())
    } else {
        "media".into()
    };
    let manifest = Manifest {
        version: 2,
        dir_name: safe_base(&root_name),
        src_bytes: stats.bytes,
        source_hash: source_hash(&entries),
        preset: opts.preset.clone(),
        files: entries,
    };
    validate_manifest(&manifest)?;
    let json = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
    if json.len() > MAX_MANIFEST {
        return Err("error.manifest_too_large".into());
    }
    let archive = scratch.path().join("archive.spk");
    let mut out = File::create(&archive).map_err(io)?;
    out.write_all(MAGIC).map_err(io)?;
    out.write_all(&(json.len() as u32).to_le_bytes())
        .map_err(io)?;
    out.write_all(blake3::hash(&json).as_bytes()).map_err(io)?;
    out.write_all(&json).map_err(io)?;
    let mut encoder = zstd::stream::write::Encoder::new(out, level).map_err(io)?;
    encoder.window_log(window).map_err(io)?;
    encoder.long_distance_matching(true).map_err(io)?;
    encoder.include_checksum(true).map_err(io)?;
    encoder
        .multithread(std::thread::available_parallelism().map_or(2, |n| n.get().min(4)) as u32)
        .map_err(io)?;
    let mut payload = File::open(&payload_path).map_err(io)?;
    let mut buf = vec![0; 1 << 20];
    let mut count = 0;
    let mut active_level = level;
    for entry in &manifest.files {
        let record_level = if matches!(
            entry.method,
            Method::MovModel
                | Method::MovShared
                | Method::MovMotion
                | Method::MovSource
                | Method::GifModel
                | Method::GifPredict
        ) {
            1
        } else {
            level
        };
        if record_level != active_level && entry.stored > 0 {
            encoder.flush().map_err(io)?;
            encoder
                .set_parameter(zstd::zstd_safe::CParameter::CompressionLevel(record_level))
                .map_err(io)?;
            active_level = record_level;
        }
        let mut remaining = entry.stored;
        while remaining > 0 {
            tick(cb, &cancel, "compress", count, payload_size, &entry.name)?;
            let n = remaining.min(buf.len() as u64) as usize;
            payload.read_exact(&mut buf[..n]).map_err(io)?;
            encoder.write_all(&buf[..n]).map_err(io)?;
            count += n as u64;
            remaining -= n as u64;
        }
    }
    let out = encoder.finish().map_err(io)?;
    out.sync_all().map_err(io)?;
    drop(out);
    tick(cb, &cancel, "compress", payload_size, payload_size, "")?;
    let filename = format!(
        "{}.{}.{}{}.spk",
        manifest.dir_name,
        &manifest.source_hash[..12],
        opts.preset,
        if opts.raw { ".raw" } else { "" }
    );
    let parts = volumes::publish(&archive, &out_dir.join(filename), &opts.split, cb, &cancel)?;
    let packed_bytes = parts
        .iter()
        .try_fold(0u64, |sum, p| fs::metadata(p).map(|m| sum + m.len()))
        .map_err(io)?;
    Ok(PackResult {
        pack_path: parts[0].clone(),
        parts,
        src_bytes: stats.bytes,
        packed_bytes,
        n_files: sources.len(),
    })
}

fn read_header<R: Read>(file: &mut R) -> Result<Manifest, String> {
    let mut magic = [0; 8];
    file.read_exact(&mut magic).map_err(io)?;
    if &magic != MAGIC {
        return Err("error.unsupported_archive_version".into());
    }
    let mut length = [0; 4];
    file.read_exact(&mut length).map_err(io)?;
    let len = u32::from_le_bytes(length) as usize;
    if len == 0 || len > MAX_MANIFEST {
        return Err("error.invalid_manifest_length".into());
    }
    let mut hash = [0; 32];
    file.read_exact(&mut hash).map_err(io)?;
    let mut json = vec![0; len];
    file.read_exact(&mut json).map_err(io)?;
    if blake3::hash(&json).as_bytes() != &hash {
        return Err("error.manifest_checksum".into());
    }
    let m: Manifest = serde_json::from_slice(&json).map_err(|e| e.to_string())?;
    validate_manifest(&m)?;
    Ok(m)
}
fn validate_manifest(m: &Manifest) -> Result<(), String> {
    if m.version != 2 || m.files.is_empty() || m.files.len() > 1_000_000 {
        return Err("error.invalid_archive".into());
    }
    if safe_rel_path(&m.dir_name)?.components().count() != 1 {
        return Err("error.invalid_output_name".into());
    }
    let mut names = HashSet::new();
    let mut bytes = 0u64;
    for (i, e) in m.files.iter().enumerate() {
        safe_rel_path(&e.name)?;
        if !names.insert(e.name.to_lowercase()) {
            return Err("error.duplicate_manifest_path".into());
        }
        if e.hash.len() != 64 || !e.hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("error.invalid_file_hash".into());
        }
        bytes = bytes
            .checked_add(e.size)
            .ok_or("error.file_size_overflow")?;
        match e.method {
            Method::Duplicate => {
                let r = e
                    .reference
                    .filter(|r| *r < i)
                    .ok_or("error.invalid_duplicate_reference")?;
                let previous = &m.files[r];
                if e.stored != 0 || previous.hash != e.hash || previous.size != e.size {
                    return Err("error.duplicate_mismatch".into());
                }
            }
            Method::Raw => {
                if e.stored != e.size || e.reference.is_some() {
                    return Err("error.raw_length_mismatch".into());
                }
            }
            Method::GifPredict => {
                let r = e
                    .reference
                    .filter(|r| *r < i)
                    .ok_or("error.invalid_gif_reference")?;
                if scan::extension(&e.name) != "gif"
                    || scan::extension(&m.files[r].name) != "mov"
                    || m.files[r].size > TRANSFORM_LIMIT
                    || e.stored > MAX_RECORD
                    || e.size > TRANSFORM_LIMIT
                {
                    return Err("error.invalid_gif_record".into());
                }
            }
            _ => {
                if e.stored > MAX_RECORD || e.size > TRANSFORM_LIMIT || e.reference.is_some() {
                    return Err("error.record_limit".into());
                }
            }
        }
    }
    for name in &names {
        let mut p = Path::new(name).parent();
        while let Some(parent) = p {
            if names.contains(&parent.to_string_lossy().replace('\\', "/")) {
                return Err("error.path_conflict".into());
            }
            p = parent.parent();
        }
    }
    if bytes != m.src_bytes || source_hash(&m.files) != m.source_hash {
        return Err("error.source_manifest_checksum".into());
    }
    Ok(())
}
pub fn read_manifest(path: &Path) -> Result<Manifest, String> {
    let mut reader = volumes::preview_reader(path)?;
    read_header(&mut reader)
}
pub fn archive_info(path: &Path) -> Result<ArchiveInfo, String> {
    let parts = volumes::part_count(path)?;
    let m = read_manifest(path)?;
    Ok(ArchiveInfo {
        files: m.files.len(),
        bytes: m.src_bytes,
        dir_name: m.dir_name,
        parts,
    })
}
fn available_target(parent: &Path, name: &str) -> PathBuf {
    let mut candidate = parent.join(name);
    let mut n = 2;
    while candidate.exists() {
        candidate = parent.join(format!("{name} ({n})"));
        n += 1;
    }
    candidate
}

pub fn unpack(
    path: &Path,
    parent: Option<&Path>,
    cb: ProgressFn,
    cancel: Cancel,
) -> Result<UnpackResult, String> {
    let resolved = volumes::resolve(path, cb, &cancel)?;
    let mut file = File::open(resolved.path()).map_err(io)?;
    let m = read_header(&mut file)?;
    let parent = parent.unwrap_or_else(|| path.parent().unwrap_or(Path::new(".")));
    fs::create_dir_all(parent).map_err(io)?;
    let target = available_target(parent, &m.dir_name);
    let staging = tempfile::Builder::new()
        .prefix(".spack-unpack-")
        .tempdir_in(parent)
        .map_err(io)?;
    let mut decoder = zstd::stream::read::Decoder::new(file)
        .map_err(io)?
        .single_frame();
    decoder.window_log_max(30).map_err(io)?;
    struct Job {
        index: usize,
        data: Vec<u8>,
        dep: Option<usize>,
        pool_from: Option<usize>,
    }
    struct Shared {
        queue: std::collections::VecDeque<Job>,
        done: Vec<bool>,
        pending: usize,
        finished: u64,
        error: Option<String>,
    }
    let n = m.files.len();
    let workers = std::thread::available_parallelism()
        .map_or(2, |x| x.get() / 4)
        .clamp(2, 4);
    let budget = std::thread::available_parallelism()
        .map_or(4, |x| x.get())
        .div_ceil(workers)
        .max(2);
    let shared = std::sync::Mutex::new(Shared {
        queue: Default::default(),
        done: vec![false; n],
        pending: 0,
        finished: 0,
        error: None,
    });
    let signal = std::sync::Condvar::new();
    let closed = AtomicBool::new(false);
    let abort = AtomicBool::new(false);
    let staging_path = staging.path();
    let stop = || {
        abort.load(Ordering::Relaxed) || cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed))
    };
    let fail = |e: String| {
        let mut sh = shared.lock().unwrap();
        sh.error.get_or_insert(e);
        abort.store(true, Ordering::Relaxed);
        signal.notify_all();
    };
    let finish = |index: usize| {
        let mut sh = shared.lock().unwrap();
        sh.done[index] = true;
        sh.finished += m.files[index].size;
        signal.notify_all();
    };
    let create = |index: usize| -> Result<File, String> {
        let dst = staging_path.join(safe_rel_path(&m.files[index].name)?);
        fs::create_dir_all(dst.parent().unwrap()).map_err(io)?;
        File::options()
            .write(true)
            .create_new(true)
            .open(&dst)
            .map_err(io)
    };
    let write_entry = |index: usize, raw: &[u8]| -> Result<(), String> {
        let entry = &m.files[index];
        if raw.len() as u64 != entry.size {
            return Err(crate::locale::message(
                "error.reconstruction_length",
                &[(entry.name).to_string()],
            ));
        }
        if blake3::hash(raw).to_hex().as_str() != entry.hash {
            return Err(crate::locale::message(
                "error.file_checksum",
                &[(entry.name).to_string()],
            ));
        }
        let mut out = create(index)?;
        out.write_all(raw).map_err(io)?;
        out.sync_all().map_err(io)
    };
    let read_entry = |index: usize| -> Result<Vec<u8>, String> {
        let entry = &m.files[index];
        let mut v = Vec::new();
        File::open(staging_path.join(&entry.name))
            .map_err(io)?
            .take(TRANSFORM_LIMIT + 1)
            .read_to_end(&mut v)
            .map_err(io)?;
        if v.len() as u64 != entry.size {
            return Err("error.gif_reference_length".into());
        }
        Ok(v)
    };
    let run_job = |job: &Job,
                   report: &std::sync::mpsc::Sender<(usize, u64)>|
     -> Result<(), String> {
        let entry = &m.files[job.index];
        let mut progress = |done: u64, total: u64| {
            let bytes = if total == 0 {
                0
            } else {
                (entry.size as u128 * done.min(total) as u128 / total as u128) as u64
            };
            let _ = report.send((job.index, bytes));
            !stop()
        };
        if entry.method == Method::Duplicate {
            let mut source = File::open(staging_path.join(&m.files[entry.reference.unwrap()].name))
                .map_err(io)?;
            let mut out = create(job.index)?;
            let (mut h, mut count, mut buf) = (blake3::Hasher::new(), 0u64, vec![0; 1 << 20]);
            loop {
                let k = source.read(&mut buf).map_err(io)?;
                if k == 0 {
                    break;
                }
                out.write_all(&buf[..k]).map_err(io)?;
                h.update(&buf[..k]);
                count += k as u64;
            }
            if count != entry.size || h.finalize().to_hex().as_str() != entry.hash {
                return Err(crate::locale::message(
                    "error.file_checksum",
                    &[(entry.name).to_string()],
                ));
            }
            return out.sync_all().map_err(io);
        }
        let raw = match entry.method {
            Method::GifExact => super::gif_exact::decode(&job.data)?,
            Method::MovExact => super::mov_exact::decode(&job.data)?,
            Method::GifModel => super::gif_model::decode_with_progress(&job.data, &mut progress)?,
            Method::GifPredict => super::gif_predict::decode(
                &job.data,
                &read_entry(entry.reference.unwrap())?,
                &mut progress,
            )?,
            Method::MovMotion => super::mov_motion::decode_with_progress(&job.data, &mut progress)?,
            Method::MovModel => super::mov_model::decode_with_progress(&job.data, &mut progress)?,
            Method::MovSource => {
                let pool = match job.pool_from {
                    Some(j) => super::mov_source::pool_of(&read_entry(j)?)?,
                    None => super::mov_source::Pool::default(),
                };
                super::mov_source::decode(&job.data, &pool, false, &mut progress)?.0
            }
            _ => unreachable!(),
        };
        write_entry(job.index, &raw)
    };
    let (report_tx, report_rx) = std::sync::mpsc::channel::<(usize, u64)>();
    let result = std::thread::scope(|scope| -> Result<(), String> {
        for _ in 0..workers {
            let report = report_tx.clone();
            let (shared, signal, closed, stop, run_job, finish, fail) =
                (&shared, &signal, &closed, &stop, &run_job, &finish, &fail);
            scope.spawn(move || {
                super::mov_source::with_thread_budget(budget, || loop {
                    let job = {
                        let mut sh = shared.lock().unwrap();
                        loop {
                            if stop() {
                                return;
                            }
                            let ready = sh
                                .queue
                                .iter()
                                .position(|j| j.dep.is_none_or(|d| sh.done[d]));
                            if let Some(k) = ready {
                                break sh.queue.remove(k).unwrap();
                            }
                            if sh.queue.is_empty() && closed.load(Ordering::Relaxed) {
                                return;
                            }
                            sh = signal
                                .wait_timeout(sh, std::time::Duration::from_millis(100))
                                .unwrap()
                                .0;
                        }
                    };
                    match run_job(&job, &report) {
                        Ok(()) => {
                            finish(job.index);
                            let mut sh = shared.lock().unwrap();
                            sh.pending -= 1;
                            signal.notify_all();
                        }
                        Err(e) => fail(e),
                    }
                })
            });
        }
        drop(report_tx);
        let relay = |cb: ProgressFn| -> Result<(), String> {
            while let Ok((index, bytes)) = report_rx.try_recv() {
                let finished = shared.lock().unwrap().finished;
                tick(
                    cb,
                    &cancel,
                    "unpack",
                    (finished + bytes).min(m.src_bytes),
                    m.src_bytes,
                    &m.files[index].name,
                )?;
            }
            Ok(())
        };
        let mut reader = || -> Result<(), String> {
            let mut mov_context = super::mov_model::SharedContext::default();
            let mut buf = vec![0; 1 << 20];
            for (index, entry) in m.files.iter().enumerate() {
                relay(&mut *cb)?;
                let finished = shared.lock().unwrap().finished;
                tick(
                    cb,
                    &cancel,
                    "unpack",
                    finished,
                    m.src_bytes,
                    format!("{} / {} · {}", index + 1, n, entry.name),
                )?;
                if let Some(e) = shared.lock().unwrap().error.clone() {
                    return Err(e);
                }
                match entry.method {
                    Method::Raw => {
                        let mut out = create(index)?;
                        let (mut h, mut remaining) = (blake3::Hasher::new(), entry.size);
                        while remaining > 0 {
                            let take = remaining.min(buf.len() as u64) as usize;
                            decoder.read_exact(&mut buf[..take]).map_err(io)?;
                            out.write_all(&buf[..take]).map_err(io)?;
                            h.update(&buf[..take]);
                            remaining -= take as u64;
                        }
                        if h.finalize().to_hex().as_str() != entry.hash {
                            return Err(crate::locale::message(
                                "error.file_checksum",
                                &[(entry.name).to_string()],
                            ));
                        }
                        out.sync_all().map_err(io)?;
                        finish(index);
                    }
                    Method::MovShared => {
                        let mut data = vec![0; entry.stored as usize];
                        decoder.read_exact(&mut data).map_err(io)?;
                        let raw = super::mov_model::decode_shared_with_progress(
                            &data,
                            &mut mov_context,
                            &mut |_, _| !stop(),
                        )?;
                        write_entry(index, &raw)?;
                        finish(index);
                    }
                    _ => {
                        let mut data = vec![0; entry.stored as usize];
                        decoder.read_exact(&mut data).map_err(io)?;
                        let (mut dep, mut pool_from) = (entry.reference, None);
                        if entry.method == Method::MovSource && super::mov_source::uses_pool(&data)?
                        {
                            let j = index
                                .checked_sub(1)
                                .filter(|&j| m.files[j].method == Method::MovSource)
                                .ok_or("error.invalid_archive")?;
                            dep = Some(j);
                            pool_from = Some(j);
                        }
                        let mut sh = shared.lock().unwrap();
                        while sh.pending >= workers * 2 && sh.error.is_none() && !stop() {
                            sh = signal
                                .wait_timeout(sh, std::time::Duration::from_millis(100))
                                .unwrap()
                                .0;
                        }
                        sh.queue.push_back(Job {
                            index,
                            data,
                            dep,
                            pool_from,
                        });
                        sh.pending += 1;
                        signal.notify_all();
                    }
                }
            }
            closed.store(true, Ordering::Relaxed);
            loop {
                let (pending, finished, error) = {
                    let sh = shared.lock().unwrap();
                    (sh.pending, sh.finished, sh.error.clone())
                };
                relay(&mut *cb)?;
                if let Some(e) = error {
                    return Err(e);
                }
                if pending == 0 {
                    return relay(&mut *cb);
                }
                tick(cb, &cancel, "unpack", finished, m.src_bytes, "")?;
                let sh = shared.lock().unwrap();
                let _ = signal
                    .wait_timeout(sh, std::time::Duration::from_millis(200))
                    .unwrap();
            }
        };
        let r = reader();
        if r.is_err() {
            abort.store(true, Ordering::Relaxed);
            closed.store(true, Ordering::Relaxed);
            signal.notify_all();
        }
        r
    });
    result?;
    let written = shared.lock().unwrap().finished;
    let mut extra = [0; 1];
    if decoder.read(&mut extra).map_err(io)? != 0 {
        return Err("error.undeclared_data".into());
    }
    if decoder.finish().read(&mut extra).map_err(io)? != 0 {
        return Err("error.trailing_data".into());
    }
    tick(
        cb,
        &cancel,
        "verify",
        m.files.len() as u64,
        m.files.len() as u64,
        "BLAKE3",
    )?;
    if target.exists() {
        return Err("error.output_exists".into());
    }
    fs::rename(staging.path(), &target).map_err(io)?;
    Ok(UnpackResult {
        dir: target,
        n_files: m.files.len(),
        bytes_written: written,
    })
}

pub fn hash_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(io)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = file.read(&mut buf).map_err(io)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().to_hex().to_string())
}
pub fn verify(original: &Path, restored: &Path, filter: Filter) -> Result<usize, String> {
    let (a, _) = scan::collect(&[original.to_path_buf()], filter)?;
    let (b, _) = scan::collect(&[restored.to_path_buf()], filter)?;
    if a.len() != b.len() || a.is_empty() {
        return Err("error.file_count_mismatch".into());
    }
    for (x, y) in a.iter().zip(&b) {
        if x.name != y.name || x.bytes != y.bytes || hash_file(&x.path)? != hash_file(&y.path)? {
            return Err(crate::locale::message(
                "error.verify_file",
                &[(x.name).to_string()],
            ));
        }
    }
    Ok(a.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(paths: &[&str]) -> Vec<scan::Source> {
        paths
            .iter()
            .map(|path| scan::Source {
                path: PathBuf::from(path),
                name: (*path).into(),
                bytes: 10,
            })
            .collect()
    }

    #[test]
    fn companion_selection_respects_set_and_folder_identity() {
        let input = sources(&[
            "mov/set/wave.mov",
            "mov/V4/wave.mov",
            "mixed/jump.MOV",
            "gif/set/wave.gif",
            "gif/V4/wave.gif",
            "mixed/jump.gif",
            "unrelated/wave.gif",
            "other/jump.gif",
            "gif/missing.gif",
        ]);
        assert_eq!(
            pair_mov_sources(&input),
            vec![
                None,
                None,
                None,
                Some(0),
                Some(1),
                Some(2),
                None,
                Some(2),
                None,
            ]
        );
        let same_folder = sources(&["a/set/wave.mov", "b/set/wave.mov", "b/set/wave.gif"]);
        assert_eq!(pair_mov_sources(&same_folder)[2], Some(1));
        let ambiguous = sources(&["a/set/wave.mov", "b/set/wave.mov", "c/set/wave.gif"]);
        assert_eq!(pair_mov_sources(&ambiguous)[2], None);
    }

    #[test]
    fn companions_must_precede_gif_and_fit_memory_limit() {
        let mut input = sources(&["set/a.mov", "set/b.gif", "set/b.mov", "set/a.gif"]);
        input[0].bytes = TRANSFORM_LIMIT + 1;
        assert_eq!(pair_mov_sources(&input), vec![None; 4]);
    }
}
