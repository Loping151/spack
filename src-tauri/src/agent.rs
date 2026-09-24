use crate::core::{
    container::{self, PackOptions, Progress},
    scan::{self, Filter},
    volumes::Split,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
};

pub fn error_value(error: &str) -> Value {
    let message = crate::locale::render_error("en", error);
    let normalized = crate::locale::normalize_error(error.to_string());
    let code = serde_json::from_str::<Value>(&normalized)
        .ok()
        .and_then(|v| v.get("code").and_then(Value::as_str).map(str::to_string))
        .unwrap_or(normalized);
    json!({ "code": code, "message": message })
}

pub fn is_cancelled(error: &str) -> bool {
    error_value(error)["code"] == "error.cancelled"
}

pub fn info(archive: &PathBuf, include_files: bool) -> Result<Value, String> {
    let m = container::read_manifest(archive)?;
    let mut methods: BTreeMap<String, usize> = BTreeMap::new();
    for f in &m.files {
        let name = serde_json::to_value(&f.method).map_err(|e| e.to_string())?;
        *methods
            .entry(name.as_str().unwrap_or("?").to_string())
            .or_default() += 1;
    }
    let stored: u64 = m.files.iter().map(|f| f.stored).sum();
    let mut v = json!({
        "dir_name": m.dir_name,
        "files": m.files.len(),
        "original_bytes": m.src_bytes,
        "stored_record_bytes": stored,
        "preset": m.preset,
        "methods": methods,
    });
    if include_files {
        v["entries"] = serde_json::to_value(&m.files).map_err(|e| e.to_string())?;
    }
    Ok(v)
}

fn paths(args: &Value, key: &str) -> Result<Vec<PathBuf>, String> {
    let list = args
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("missing array argument '{key}'"))?;
    let out: Vec<PathBuf> = list
        .iter()
        .map(|v| {
            v.as_str()
                .map(PathBuf::from)
                .ok_or_else(|| format!("'{key}' must contain strings"))
        })
        .collect::<Result<_, _>>()?;
    if out.is_empty() {
        return Err(format!("'{key}' must not be empty"));
    }
    Ok(out)
}
fn path(args: &Value, key: &str) -> Result<PathBuf, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing string argument '{key}'"))
}
fn filter(args: &Value) -> Result<Filter, String> {
    match args.get("filter").and_then(Value::as_str) {
        Some(f) => Filter::parse(f),
        None => Ok(Filter::All),
    }
}

pub fn parse_size(s: &str) -> Result<u64, String> {
    let lower = s.trim().to_ascii_lowercase();
    let (number, mul) = if let Some(s) = lower.strip_suffix("gib") {
        (s, 1u64 << 30)
    } else if let Some(s) = lower.strip_suffix("mib") {
        (s, 1 << 20)
    } else if let Some(s) = lower.strip_suffix("kib") {
        (s, 1 << 10)
    } else {
        (lower.as_str(), 1)
    };
    number
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mul))
        .ok_or("error.invalid_size".into())
}

pub fn call(
    tool: &str,
    args: &Value,
    progress: &mut dyn FnMut(Progress) -> bool,
    cancel: Arc<AtomicBool>,
) -> Result<Value, String> {
    match tool {
        "scan" => {
            let (_, stats) = scan::collect(&paths(args, "paths")?, filter(args)?)?;
            to_value(&stats)
        }
        "pack" => {
            let mut opts = PackOptions {
                filter: filter(args)?,
                out_dir: Some(path(args, "output_dir")?),
                ..Default::default()
            };
            if let Some(p) = args.get("preset").and_then(Value::as_str) {
                opts.preset = p.to_string();
            }
            opts.raw = args.get("raw").and_then(Value::as_bool).unwrap_or(false);
            match (args.get("parts"), args.get("part_size")) {
                (Some(_), Some(_)) => return Err("error.split_conflict".into()),
                (Some(n), None) => {
                    let n = n
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or("error.invalid_parts")?;
                    opts.split = Split::Count(n)
                }
                (None, Some(s)) => {
                    let bytes = match s {
                        Value::Number(n) => n.as_u64().ok_or("error.invalid_size")?,
                        Value::String(t) => parse_size(t)?,
                        _ => return Err("error.invalid_size".into()),
                    };
                    opts.split = Split::Size(bytes)
                }
                (None, None) => {}
            }
            let r = container::pack(&paths(args, "paths")?, &opts, progress, Some(cancel))?;
            to_value(&r)
        }
        "unpack" => {
            let target = args
                .get("output_dir")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            let r = container::unpack(
                &path(args, "archive")?,
                target.as_deref(),
                progress,
                Some(cancel),
            )?;
            to_value(&r)
        }
        "info" => info(
            &path(args, "archive")?,
            args.get("include_files")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
        "verify" => {
            let n = container::verify(
                &path(args, "original")?,
                &path(args, "restored")?,
                filter(args)?,
            )?;
            Ok(json!({ "verified": n }))
        }
        _ => Err(format!("unknown tool '{tool}'")),
    }
}

fn to_value<T: serde::Serialize>(v: &T) -> Result<Value, String> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

pub fn tools() -> Value {
    let filter = json!({
        "type": "string",
        "enum": ["all", "gif", "video", "mov", "no-gif", "no-video", "no-mov"],
        "description": "Which media files to include (default all)."
    });
    json!([
        {
            "name": "scan",
            "description": "Count the media files spack would archive under the given files or folders. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Absolute file or folder paths." },
                    "filter": filter
                },
                "required": ["paths"]
            }
        },
        {
            "name": "pack",
            "description": "Archive media files and folders into a .spk archive (optionally split into volumes) that restores every file byte for byte. Source files are never modified. Returns the archive path(s) and sizes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Absolute file or folder paths to archive together." },
                    "output_dir": { "type": "string", "description": "Absolute folder for the archive; created if missing." },
                    "preset": { "type": "string", "enum": ["fast", "balanced", "max"], "description": "Speed/size trade-off (default balanced)." },
                    "filter": filter,
                    "parts": { "type": "integer", "minimum": 1, "description": "Split into this many volumes." },
                    "part_size": { "type": ["string", "integer"], "description": "Maximum volume size, e.g. \"100MiB\" or bytes." },
                    "raw": { "type": "boolean", "description": "Store without media transforms." }
                },
                "required": ["paths", "output_dir"]
            }
        },
        {
            "name": "unpack",
            "description": "Extract a .spk archive (or any of its volumes) into a new folder and verify every file's BLAKE3. Never overwrites existing files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "archive": { "type": "string", "description": "Absolute path of the .spk file or any .spk.NNN volume." },
                    "output_dir": { "type": "string", "description": "Parent folder for the extracted folder (default: the archive's folder)." }
                },
                "required": ["archive"]
            }
        },
        {
            "name": "info",
            "description": "Summarize an archive: file count, original size, stored size and per-method counts. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "archive": { "type": "string", "description": "Absolute path of the .spk file or any volume." },
                    "include_files": { "type": "boolean", "description": "Also return every entry (can be large)." }
                },
                "required": ["archive"]
            }
        },
        {
            "name": "verify",
            "description": "Compare an original folder with an extracted folder by BLAKE3. Read-only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "original": { "type": "string", "description": "Absolute path of the original folder." },
                    "restored": { "type": "string", "description": "Absolute path of the extracted folder." },
                    "filter": filter
                },
                "required": ["original", "restored"]
            }
        }
    ])
}
