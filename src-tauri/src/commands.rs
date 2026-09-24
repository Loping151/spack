use crate::core::{
    container::{self, Progress},
    scan, volumes,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tauri::Emitter;
static JOB: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);
#[tauri::command]
pub fn locale_messages(lang: String) -> crate::locale::Messages {
    crate::locale::messages(&lang)
}
struct Lease;
impl Drop for Lease {
    fn drop(&mut self) {
        if let Ok(mut job) = JOB.lock() {
            *job = None;
        }
    }
}
fn acquire() -> Result<(Lease, Arc<AtomicBool>), String> {
    let mut job = JOB.lock().map_err(|_| "error.job_unavailable")?;
    if job.is_some() {
        return Err("error.job_running".into());
    }
    let flag = Arc::new(AtomicBool::new(false));
    *job = Some(flag.clone());
    Ok((Lease, flag))
}
fn progress_callback(app: tauri::AppHandle, flag: Arc<AtomicBool>) -> impl FnMut(Progress) -> bool {
    let mut last_emit = Instant::now();
    let mut last_phase = "";
    let mut last_detail = String::new();
    move |p: Progress| {
        if flag.load(Ordering::Relaxed) {
            return false;
        }
        if p.phase != last_phase
            || p.detail != last_detail
            || (p.total > 0 && p.done == p.total)
            || last_emit.elapsed() >= Duration::from_millis(100)
        {
            last_phase = p.phase;
            last_detail.clone_from(&p.detail);
            last_emit = Instant::now();
            let _ = app.emit("spack://progress", p);
        }
        !flag.load(Ordering::Relaxed)
    }
}
#[tauri::command]
pub async fn scan_selection(dirs: Vec<String>, filter: String) -> Result<scan::Stats, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let dirs: Vec<_> = dirs.into_iter().map(PathBuf::from).collect();
        scan::collect(&dirs, scan::Filter::parse(&filter)?).map(|(_, stats)| stats)
    })
    .await
    .map_err(|e| crate::locale::normalize_error(e.to_string()))?
    .map_err(crate::locale::normalize_error)
}
#[tauri::command]
pub async fn archive_info(pack_path: String) -> Result<container::ArchiveInfo, String> {
    tauri::async_runtime::spawn_blocking(move || container::archive_info(&PathBuf::from(pack_path)))
        .await
        .map_err(|e| crate::locale::normalize_error(e.to_string()))?
        .map_err(crate::locale::normalize_error)
}
#[tauri::command]
pub async fn pack(
    app: tauri::AppHandle,
    dirs: Vec<String>,
    filter: String,
    preset: String,
    out_dir: Option<String>,
    split_mode: String,
    split_value: f64,
) -> Result<container::PackResult, String> {
    let filter = scan::Filter::parse(&filter)?;
    if !split_value.is_finite() || split_value < 0.0 {
        return Err("error.invalid_split".into());
    }
    let split = match split_mode.as_str() {
        "none" => volumes::Split::None,
        "count" if split_value.fract() == 0.0 && split_value <= 10000.0 => {
            volumes::Split::Count(split_value as u32)
        }
        "size" if split_value > 0.0 && split_value <= (u64::MAX / (1 << 20)) as f64 => {
            volumes::Split::Size((split_value * 1048576.0) as u64)
        }
        _ => return Err("error.invalid_split".into()),
    };
    split.validate()?;
    let (lease, flag) = acquire()?;
    tauri::async_runtime::spawn_blocking(move || {
        let _lease = lease;
        let dirs: Vec<_> = dirs.into_iter().map(PathBuf::from).collect();
        let opts = container::PackOptions {
            filter,
            preset,
            out_dir: out_dir.map(PathBuf::from),
            split,
            raw: false,
        };
        let mut cb = progress_callback(app, flag.clone());
        container::pack(&dirs, &opts, &mut cb, Some(flag.clone()))
    })
    .await
    .map_err(|e| crate::locale::normalize_error(e.to_string()))?
    .map_err(crate::locale::normalize_error)
}
#[tauri::command]
pub async fn unpack(
    app: tauri::AppHandle,
    pack_path: String,
    target_dir: Option<String>,
) -> Result<container::UnpackResult, String> {
    let (lease, flag) = acquire()?;
    tauri::async_runtime::spawn_blocking(move || {
        let _lease = lease;
        let parent = target_dir.map(PathBuf::from);
        let mut cb = progress_callback(app, flag.clone());
        container::unpack(
            &PathBuf::from(pack_path),
            parent.as_deref(),
            &mut cb,
            Some(flag.clone()),
        )
    })
    .await
    .map_err(|e| crate::locale::normalize_error(e.to_string()))?
    .map_err(crate::locale::normalize_error)
}
#[tauri::command]
pub fn cancel() {
    if let Ok(job) = JOB.lock() {
        if let Some(flag) = job.as_ref() {
            flag.store(true, Ordering::Relaxed);
        }
    }
}
