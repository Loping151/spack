use crate::{agent, core::container::Progress};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{BufRead, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const INSTRUCTIONS: &str = "spack archives media files (GIF, MOV and other video, PNG, WebP) into .spk archives and restores them byte for byte. Use absolute paths. Typical flow: scan the inputs, pack them into an output folder, then unpack and verify when restoring. Packing large collections can take minutes; progress notifications are sent when a progress token is supplied.";

type Sender = Arc<dyn Fn(Value) + Send + Sync>;

fn reply(send: &Sender, id: Value, result: Value) {
    send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
}
fn fail(send: &Sender, id: Value, code: i64, message: &str) {
    send(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }));
}

pub fn run() -> Result<(), String> {
    let stdout = Mutex::new(std::io::stdout());
    let send: Sender = Arc::new(move |v: Value| {
        let mut out = stdout.lock().unwrap();
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    });
    let running: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> = Default::default();
    let mut workers = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                fail(&send, Value::Null, -32700, &format!("parse error: {e}"));
                continue;
            }
        };
        let id = msg.get("id").cloned();
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
        match (method.as_str(), id) {
            ("initialize", Some(id)) => {
                let requested = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(VERSIONS[0]);
                let version = VERSIONS
                    .iter()
                    .find(|v| **v == requested)
                    .unwrap_or(&VERSIONS[0]);
                reply(
                    &send,
                    id,
                    json!({
                        "protocolVersion": version,
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": { "name": "spack", "version": env!("CARGO_PKG_VERSION") },
                        "instructions": INSTRUCTIONS
                    }),
                );
            }
            ("ping", Some(id)) => reply(&send, id, json!({})),
            ("tools/list", Some(id)) => reply(&send, id, json!({ "tools": agent::tools() })),
            ("tools/call", Some(id)) => {
                let Some(name) = params
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                else {
                    fail(&send, id, -32602, "missing tool name");
                    continue;
                };
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let token = params
                    .get("_meta")
                    .and_then(|m| m.get("progressToken"))
                    .cloned();
                let flag = Arc::new(AtomicBool::new(false));
                running.lock().unwrap().insert(id.to_string(), flag.clone());
                let (send, running) = (send.clone(), running.clone());
                workers.push(std::thread::spawn(move || {
                    let mut phases: Vec<&'static str> = Vec::new();
                    let (mut sent, mut last) = (0u64, Instant::now() - Duration::from_secs(1));
                    let mut progress = |p: Progress| {
                        if let Some(token) = &token {
                            let rank = match phases.iter().position(|x| *x == p.phase) {
                                Some(r) => r,
                                None => {
                                    phases.push(p.phase);
                                    phases.len() - 1
                                }
                            } as u64;
                            let permille = if p.total == 0 { 0 } else { 1000 * p.done.min(p.total) / p.total };
                            let value = rank * 1000 + permille;
                            if value > sent && (last.elapsed() >= Duration::from_millis(250) || permille == 1000) {
                                sent = value;
                                last = Instant::now();
                                send(json!({
                                    "jsonrpc": "2.0",
                                    "method": "notifications/progress",
                                    "params": { "progressToken": token, "progress": value, "message": format!("{} {}", p.phase, p.detail).trim() }
                                }));
                            }
                        }
                        !flag.load(Ordering::Relaxed)
                    };
                    let result = match agent::call(&name, &args, &mut progress, flag.clone()) {
                        Ok(v) => json!({
                            "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }],
                            "structuredContent": v,
                            "isError": false
                        }),
                        Err(e) => {
                            let error = agent::error_value(&e);
                            json!({
                                "content": [{ "type": "text", "text": serde_json::to_string_pretty(&error).unwrap_or_default() }],
                                "structuredContent": { "error": error },
                                "isError": true
                            })
                        }
                    };
                    running.lock().unwrap().remove(&id.to_string());
                    reply(&send, id, result);
                }));
            }
            ("notifications/cancelled", None) => {
                if let Some(request) = params.get("requestId") {
                    if let Some(flag) = running.lock().unwrap().get(&request.to_string()) {
                        flag.store(true, Ordering::Relaxed);
                    }
                }
            }
            (_, None) => {}
            (_, Some(id)) => fail(&send, id, -32601, &format!("method not found: {method}")),
        }
    }
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}
