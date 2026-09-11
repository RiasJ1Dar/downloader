//! Пустушка протоколу: `probe` дає `dummy.bin`, `run` пише `b"TEST"` у файл.
//!
//! Байти через RPC не читає і не шле — лише шлях у `targets`.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use downloader_proto_external::rpc::{
    error_line, ok_line, ProbeParams, ProbeResult, RpcFile, RpcRequest, RunParams, RunResult,
};

fn main() -> ExitCode {
    match work() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(1)
        }
    }
}

fn work() -> Result<(), String> {
    let mut рядок = String::new();
    io::stdin()
        .lock()
        .read_line(&mut рядок)
        .map_err(|e| format!("не вдалося прочитати stdin: {e}"))?;
    if рядок.trim().is_empty() {
        return Err("порожній запит на stdin".to_owned());
    }

    let req: RpcRequest =
        serde_json::from_str(&рядок).map_err(|e| format!("запит не JSON-RPC: {e}"))?;

    match req.method.as_str() {
        "probe" => handle_probe(&req),
        "run" => handle_run(&req),
        other => {
            let line = error_line(&req.id, -32601, &format!("немає методу `{other}`"))
                .map_err(|e| format!("не вдалося скласти помилку: {e}"))?;
            write_line(&line)
        }
    }
}

fn handle_probe(req: &RpcRequest) -> Result<(), String> {
    let params: ProbeParams = serde_json::from_value(req.params.clone())
        .map_err(|e| format!("params probe: {e}"))?;
    let result = ProbeResult {
        final_url: params.source,
        total_size: Some(4),
        resumable: false,
        fingerprint: None,
        files: vec![RpcFile {
            suggested_name: "dummy.bin".to_owned(),
            size: Some(4),
            selected: true,
        }],
    };
    let line = ok_line(&req.id, &result).map_err(|e| format!("відповідь probe: {e}"))?;
    write_line(&line)
}

fn handle_run(req: &RpcRequest) -> Result<(), String> {
    let params: RunParams =
        serde_json::from_value(req.params.clone()).map_err(|e| format!("params run: {e}"))?;

    if params.source == "ext:fail" {
        eprintln!("свідомо зламано");
        std::process::exit(7);
    }

    let Some(target) = params.targets.first() else {
        return Err("run без targets — писати нікуди".to_owned());
    };

    std::fs::write(target, b"TEST")
        .map_err(|e| format!("не вдалося записати {target}: {e}"))?;

    let result = RunResult { resume: None };
    let line = ok_line(&req.id, &result).map_err(|e| format!("відповідь run: {e}"))?;
    write_line(&line)
}

fn write_line(line: &str) -> Result<(), String> {
    let mut out = io::stdout().lock();
    out.write_all(line.as_bytes())
        .map_err(|e| format!("не вдалося написати stdout: {e}"))?;
    out.write_all(b"\n")
        .map_err(|e| format!("не вдалося написати stdout: {e}"))?;
    out.flush()
        .map_err(|e| format!("не вдалося скинути stdout: {e}"))?;
    Ok(())
}
