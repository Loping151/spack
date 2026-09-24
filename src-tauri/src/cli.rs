use crate::core::{
    container::{self, Progress},
    scan::{self, Filter},
    volumes::Split,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};
#[derive(Clone, Copy, Default)]
struct Mode {
    json: bool,
    quiet: bool,
}

pub fn run() {
    let original: Vec<String> = std::env::args().skip(1).collect();
    let mut lang = std::env::var("SPACK_LANG").unwrap_or_else(|_| "en".into());
    let (args, mode) = match language_options(&original, &mut lang).map(output_options) {
        Ok(v) => v,
        Err(error) => {
            eprintln!("spack: {}", crate::locale::render_error(&lang, &error));
            std::process::exit(2);
        }
    };
    if args.is_empty() || matches!(args[0].as_str(), "help" | "--help" | "-h") {
        println!("{}", crate::locale::render(&lang, "cli.help", &[]));
        return;
    }
    if args[0] == "--version" {
        if mode.json {
            println!(
                "{}",
                serde_json::json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") })
            );
        } else {
            println!("spack {}", env!("CARGO_PKG_VERSION"));
        }
        return;
    }
    if args[0] == "mcp" {
        if let Err(e) = crate::mcp::run() {
            eprintln!("spack: {e}");
            std::process::exit(1);
        }
        return;
    }
    let flag = Arc::new(AtomicBool::new(false));
    let cancel = flag.clone();
    let _ = ctrlc::set_handler(move || cancel.store(true, Ordering::Relaxed));
    match execute(&args, flag, &lang, mode) {
        Ok(result) => {
            if mode.json {
                println!(
                    "{}",
                    serde_json::json!({ "ok": true, "command": args[0], "result": result })
                );
            } else if args[0] == "verify" {
                let n = result["verified"].as_u64().unwrap_or(0);
                println!(
                    "{}",
                    crate::locale::render(&lang, "cli.verified", &[n.to_string()])
                );
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).unwrap_or_default()
                );
            }
        }
        Err(e) => {
            if mode.json {
                println!(
                    "{}",
                    serde_json::json!({ "ok": false, "command": args[0], "error": crate::agent::error_value(&e) })
                );
            } else {
                eprintln!("spack: {}", crate::locale::render_error(&lang, &e));
            }
            std::process::exit(exit_code(&e));
        }
    }
}

fn exit_code(error: &str) -> i32 {
    let code = crate::agent::error_value(error)["code"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    match code.as_str() {
        "error.cancelled" => 130,
        "error.usage"
        | "error.unknown_option"
        | "error.missing_option_value"
        | "error.split_conflict"
        | "error.invalid_parts"
        | "error.invalid_size"
        | "error.invalid_filter" => 2,
        _ => 1,
    }
}

fn output_options(args: Vec<String>) -> (Vec<String>, Mode) {
    let mut mode = Mode::default();
    let mut out = Vec::new();
    for (i, a) in args.iter().enumerate() {
        match a.as_str() {
            "--" => {
                out.extend_from_slice(&args[i..]);
                break;
            }
            "--json" => mode.json = true,
            "--quiet" | "-q" => mode.quiet = true,
            _ => out.push(a.clone()),
        }
    }
    (out, mode)
}
fn language_options(args: &[String], lang: &mut String) -> Result<Vec<String>, String> {
    let mut filtered = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--" {
            filtered.extend_from_slice(&args[i..]);
            break;
        }
        if args[i] == "--lang" {
            *lang = value(args, &mut i)?.into();
        } else if let Some(value) = args[i].strip_prefix("--lang=") {
            if value.is_empty() {
                return Err("error.missing_option_value".into());
            }
            *lang = value.into();
        } else {
            filtered.push(args[i].clone());
        }
        i += 1;
    }
    Ok(filtered)
}
fn value<'a>(args: &'a [String], at: &mut usize) -> Result<&'a str, String> {
    *at += 1;
    args.get(*at)
        .map(String::as_str)
        .ok_or("error.missing_option_value".into())
}
fn execute(
    args: &[String],
    flag: Arc<AtomicBool>,
    lang: &str,
    mode: Mode,
) -> Result<serde_json::Value, String> {
    let mut dirs = Vec::new();
    let mut opts = container::PackOptions::default();
    let mut target = None;
    let mut split_set = false;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-o" => opts.out_dir = Some(PathBuf::from(value(args, &mut i)?)),
            "-d" => target = Some(PathBuf::from(value(args, &mut i)?)),
            "--filter" => opts.filter = Filter::parse(value(args, &mut i)?)?,
            "--preset" => opts.preset = value(args, &mut i)?.to_string(),
            "--parts" => {
                if split_set {
                    return Err("error.split_conflict".into());
                }
                split_set = true;
                opts.split = Split::Count(
                    value(args, &mut i)?
                        .parse()
                        .map_err(|_| "error.invalid_parts")?,
                )
            }
            "--part-size" => {
                if split_set {
                    return Err("error.split_conflict".into());
                }
                split_set = true;
                opts.split = Split::Size(crate::agent::parse_size(value(args, &mut i)?)?)
            }
            "--raw" => opts.raw = true,
            "--" => {
                dirs.extend(args[i + 1..].iter().map(PathBuf::from));
                break;
            }
            s if s.starts_with('-') => {
                return Err(crate::locale::message(
                    "error.unknown_option",
                    &[(s).to_string()],
                ))
            }
            s => dirs.push(PathBuf::from(s)),
        }
        i += 1;
    }
    let mut phase = "";
    let mut last = Instant::now();
    let mut cb = |p: Progress| {
        if !mode.quiet
            && (p.phase != phase || last.elapsed().as_millis() > 500 || p.done == p.total)
        {
            if mode.json {
                eprintln!(
                    "{}",
                    serde_json::json!({ "event": "progress", "phase": p.phase, "done": p.done, "total": p.total, "detail": p.detail })
                );
            } else {
                eprintln!(
                    "[{}] {}/{} {}",
                    crate::locale::render(lang, &format!("phase.{}", p.phase), &[]),
                    p.done,
                    p.total,
                    p.detail
                );
            }
            last = Instant::now();
            phase = p.phase;
        }
        !flag.load(Ordering::Relaxed)
    };
    match args[0].as_str() {
        "pack" => to_json(&container::pack(&dirs, &opts, &mut cb, Some(flag.clone()))?),
        "unpack" if dirs.len() == 1 => to_json(&container::unpack(
            &dirs[0],
            target.as_deref(),
            &mut cb,
            Some(flag.clone()),
        )?),
        "scan" => to_json(&scan::collect(&dirs, opts.filter)?.1),
        "info" if dirs.len() == 1 && mode.json => crate::agent::info(&dirs[0], true),
        "info" if dirs.len() == 1 => to_json(&container::read_manifest(&dirs[0])?),
        "verify" if dirs.len() == 2 => {
            let n = container::verify(&dirs[0], &dirs[1], opts.filter)?;
            Ok(serde_json::json!({ "verified": n }))
        }
        _ => Err("error.usage".into()),
    }
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<serde_json::Value, String> {
    serde_json::to_value(v).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_option_preserves_literal_paths_after_separator() {
        let input: Vec<String> = ["--lang", "zh-CN", "pack", "--", "--lang", "en"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let mut lang = "en".into();
        assert_eq!(
            language_options(&input, &mut lang).unwrap(),
            ["pack", "--", "--lang", "en"]
        );
        assert_eq!(lang, "zh-CN");
        assert!(language_options(&["--lang".into()], &mut lang).is_err());
        assert!(language_options(&["--lang=".into()], &mut lang).is_err());
    }
}
