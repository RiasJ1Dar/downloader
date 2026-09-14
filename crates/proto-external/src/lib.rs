//! Зовнішній протокол: чужий exe по JSON-RPC на stdio.
//!
//! Байти файла **не** течуть через RPC. Плагін пише у `targets` сам.
//! Скасування — убивство процесу, не окреме повідомлення.
//!
//! Кадр — рядок JSON, не довжина попереду: шляхи в JSON екрануються, тож
//! перенос усередині рядка не рве повідомлення.

pub mod rpc;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use downloader_core::error::{Error, Result};
use downloader_core::protocol::{
    Cancel, PlannedFile, Probed, Progress, ProgressSink, Protocol, RateLimitSupport, ResumeBlob,
    RunContext,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use rpc::{
    ProbeParams, ProbeResult, ProgressDto, RpcRequest, RunParams, RunResult, WireMessage,
};

/// Результат виклику плагіна. Скасування — не помилка.
enum CallOutcome {
    Value(serde_json::Value),
    Cancelled,
}

/// Стеля рядка JSON: RPC не носить байти файла, тож мегабайта досить.
const MAX_LINE: usize = 1024 * 1024;

/// Хвіст stderr у тексті помилки — щоб упізнати збій, не заваливши журнал.
const STDERR_TAIL: usize = 4096;

/// Модуль, що розмовляє з чужим exe.
pub struct ExternalProtocol {
    exe: PathBuf,
    args: Vec<String>,
    next_id: AtomicU64,
}

impl ExternalProtocol {
    /// Плагін за шляхом до виконуваного. Аргументів немає.
    #[must_use]
    pub fn new(exe: impl Into<PathBuf>) -> Self {
        Self {
            exe: exe.into(),
            args: Vec::new(),
            next_id: AtomicU64::new(1),
        }
    }

    /// Додаткові аргументи командного рядка (перед JSON-RPC).
    #[must_use]
    pub fn with_args(mut self, args: impl Into<Vec<String>>) -> Self {
        self.args = args.into();
        self
    }

    async fn rpc_call(
        &self,
        method: &str,
        params: serde_json::Value,
        cancel: Option<&Cancel>,
        sink: Option<&dyn ProgressSink>,
    ) -> Result<CallOutcome> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let запит = RpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: serde_json::json!(id),
            method: method.to_owned(),
            params,
        };
        let рядок = serde_json::to_string(&запит).map_err(|e| {
            Error::Store(format!("не вдалося скласти JSON-RPC `{method}`: {e}"))
        })?;
        if рядок.len() > MAX_LINE {
            return Err(Error::Store(format!(
                "запит `{method}` завеликий: {} байтів при межі {MAX_LINE}",
                рядок.len()
            )));
        }

        tracing::debug!(exe = %self.exe.display(), method, id, "запуск плагіна");

        let mut child = Command::new(&self.exe)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                Error::Store(format!(
                    "не вдалося запустити плагін {}: {e}",
                    self.exe.display()
                ))
            })?;

        let mut stdin = child.stdin.take().ok_or_else(|| {
            Error::Store("плагін запустився без stdin — JSON-RPC писати нікуди".to_owned())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            Error::Store("плагін запустився без stdout — відповіді не буде".to_owned())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            Error::Store("плагін запустився без stderr — збій неможливо пояснити".to_owned())
        })?;

        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut reader = BufReader::new(stderr);
            match reader.read_to_end(&mut buf).await {
                Ok(_) => buf,
                Err(e) => {
                    tracing::debug!(error = %e, "не дочитали stderr плагіна");
                    buf
                }
            }
        });

        stdin.write_all(рядок.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        drop(stdin);

        let (tx, mut rx) = tokio::sync::mpsc::channel::<std::io::Result<String>>(16);
        let reader_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_line(&mut reader).await {
                    Ok(None) => break,
                    Ok(Some(line)) => {
                        if line.is_empty() {
                            continue;
                        }
                        if tx.send(Ok(line)).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        if tx.send(Err(e)).await.is_err() {
                            break;
                        }
                        break;
                    }
                }
            }
        });

        let mut ticker = tokio::time::interval(Duration::from_millis(50));
        let mut відповідь: Option<WireMessage> = None;
        let mut скасовано = false;

        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        None => break,
                        Some(Err(e)) => {
                            return finish_after_io_error(
                                child, stderr_task, reader_task, e,
                            )
                            .await
                            .map(CallOutcome::Value);
                        }
                        Some(Ok(line)) => {
                            match handle_line(&line, id, sink)? {
                                LineAction::Continue => {}
                                LineAction::Done(wire) => {
                                    відповідь = Some(wire);
                                    break;
                                }
                            }
                        }
                    }
                }
                _ = ticker.tick() => {
                    if cancel.is_some_and(Cancel::is_cancelled) {
                        скасовано = true;
                        match child.start_kill() {
                            Ok(()) => {}
                            Err(e) => {
                                tracing::debug!(
                                    error = %e,
                                    "kill плагіна: процес уже не живе або ОС відмовила"
                                );
                            }
                        }
                        break;
                    }
                }
            }
        }

        drop(rx);

        if скасовано {
            if let Err(e) = wait_child(&mut child).await {
                tracing::debug!(error = %e, "wait після cancel");
            }
            drop(злити_фонові(stderr_task, reader_task).await);
            return Ok(CallOutcome::Cancelled);
        }

        let status = wait_child(&mut child).await?;
        let stderr_buf = злити_фонові(stderr_task, reader_task).await;

        if let Some(wire) = відповідь {
            if let Some(err) = wire.error {
                return Err(rpc_fail(&err, status.code(), &stderr_buf));
            }
            if let Some(result) = wire.result {
                if !status.success() {
                    return Err(plugin_fail(status.code(), &stderr_buf));
                }
                return Ok(CallOutcome::Value(result));
            }
        }

        if status.success() {
            Err(Error::Store(format!(
                "плагін закрив stdout без відповіді{}",
                stderr_suffix(&stderr_buf)
            )))
        } else {
            Err(plugin_fail(status.code(), &stderr_buf))
        }
    }
}

enum LineAction {
    Continue,
    Done(WireMessage),
}

fn handle_line(
    line: &str,
    id: u64,
    sink: Option<&dyn ProgressSink>,
) -> Result<LineAction> {
    let wire: WireMessage = serde_json::from_str(line).map_err(|e| {
        Error::Store(format!("плагін віддав не JSON: {e}; рядок={line}"))
    })?;

    if let Some(method) = wire.method.as_deref()
        && wire.id.is_none()
    {
        if method == "progress" {
            if let (Some(sink), Some(params)) = (sink, wire.params.clone()) {
                report_progress(sink, params);
            }
        } else {
            tracing::debug!(method, "невідоме сповіщення плагіна — ігноруємо");
        }
        return Ok(LineAction::Continue);
    }

    match wire.id {
        Some(rid) if rid == id => Ok(LineAction::Done(wire)),
        Some(rid) => Err(Error::Store(format!(
            "плагін відповів id={rid}, чекали {id}"
        ))),
        None => Err(Error::Store(format!(
            "рядок без id і без method: {line}"
        ))),
    }
}

fn report_progress(sink: &dyn ProgressSink, params: serde_json::Value) {
    match serde_json::from_value::<ProgressDto>(params) {
        Ok(ProgressDto::TotalKnown { total }) => {
            sink.report(Progress::TotalKnown { total });
        }
        Ok(ProgressDto::Advanced { done }) => {
            sink.report(Progress::Advanced { done });
        }
        Ok(ProgressDto::Segments { count }) => {
            sink.report(Progress::Segments { count });
        }
        Ok(ProgressDto::Checkpoint { resume }) => {
            sink.report(Progress::Checkpoint { resume });
        }
        Err(e) => {
            tracing::debug!(error = %e, "плагін надіслав незрозумілий progress");
        }
    }
}

async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    loop {
        let data = reader.fill_buf().await?;
        if data.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
        if let Some(i) = data.iter().position(|&b| b == b'\n') {
            if buf.len() + i + 1 > MAX_LINE {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("рядок JSON-RPC перевищив межу {MAX_LINE}"),
                ));
            }
            buf.extend_from_slice(&data[..=i]);
            reader.consume(i + 1);
            break;
        }
        if buf.len() + data.len() > MAX_LINE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("рядок JSON-RPC перевищив межу {MAX_LINE}"),
            ));
        }
        let n = data.len();
        buf.extend_from_slice(data);
        reader.consume(n);
    }
    while buf.last().copied() == Some(b'\n') || buf.last().copied() == Some(b'\r') {
        buf.pop();
    }
    if buf.is_empty() {
        return Ok(Some(String::new()));
    }
    String::from_utf8(buf).map(Some).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("stdout плагіна не UTF-8: {e}"),
        )
    })
}

async fn wait_child(child: &mut tokio::process::Child) -> Result<std::process::ExitStatus> {
    match tokio::time::timeout(Duration::from_secs(30), child.wait()).await {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(e)) => Err(Error::Store(format!(
            "не дочекались завершення плагіна: {e}"
        ))),
        Err(_) => Err(Error::Store(
            "плагін не завершився за 30 с після відповіді або kill".to_owned(),
        )),
    }
}

async fn злити_фонові(
    stderr_task: tokio::task::JoinHandle<Vec<u8>>,
    reader_task: tokio::task::JoinHandle<()>,
) -> Vec<u8> {
    let stderr = match stderr_task.await {
        Ok(buf) => buf,
        Err(e) => {
            tracing::debug!(error = %e, "збір stderr плагіна зірвався");
            Vec::new()
        }
    };
    match reader_task.await {
        Ok(()) => {}
        Err(e) => {
            tracing::debug!(error = %e, "читач stdout плагіна зірвався");
        }
    }
    stderr
}

async fn finish_after_io_error(
    mut child: tokio::process::Child,
    stderr_task: tokio::task::JoinHandle<Vec<u8>>,
    reader_task: tokio::task::JoinHandle<()>,
    e: std::io::Error,
) -> Result<serde_json::Value> {
    match child.start_kill() {
        Ok(()) => {}
        Err(kill_e) => {
            tracing::debug!(error = %kill_e, "kill після помилки читання");
        }
    }
    let status = wait_child(&mut child).await;
    let stderr_buf = злити_фонові(stderr_task, reader_task).await;
    match status {
        Ok(st) if !st.success() => Err(plugin_fail(st.code(), &stderr_buf)),
        _ => Err(Error::Store(format!(
            "не вдалося прочитати stdout плагіна: {e}{}",
            stderr_suffix(&stderr_buf)
        ))),
    }
}

fn plugin_fail(code: Option<i32>, stderr: &[u8]) -> Error {
    let код = code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "сигнал".to_owned());
    let tail = stderr_tail(stderr);
    if tail.is_empty() {
        Error::Store(format!("плагін завершився з кодом {код}"))
    } else {
        Error::Store(format!("плагін завершився з кодом {код}: {tail}"))
    }
}

fn rpc_fail(err: &rpc::RpcError, code: Option<i32>, stderr: &[u8]) -> Error {
    let код = code
        .map(|c| format!(", процес={c}"))
        .unwrap_or_default();
    Error::Store(format!(
        "плагін RPC [{}]: {}{код}{}",
        err.code,
        err.message,
        stderr_suffix(stderr)
    ))
}

fn stderr_tail(bytes: &[u8]) -> String {
    let slice = if bytes.len() <= STDERR_TAIL {
        bytes
    } else {
        &bytes[bytes.len() - STDERR_TAIL..]
    };
    String::from_utf8_lossy(slice).trim().to_owned()
}

fn stderr_suffix(bytes: &[u8]) -> String {
    let tail = stderr_tail(bytes);
    if tail.is_empty() {
        String::new()
    } else {
        format!("; stderr: {tail}")
    }
}

fn probed_from(source: &str, r: ProbeResult) -> Probed {
    Probed {
        final_url: if r.final_url.is_empty() {
            source.to_owned()
        } else {
            r.final_url
        },
        total_size: r.total_size,
        resumable: r.resumable,
        fingerprint: r.fingerprint,
        files: r
            .files
            .into_iter()
            .map(|f| PlannedFile {
                suggested_name: f.suggested_name,
                size: f.size,
                selected: f.selected,
            })
            .collect(),
        variants: Vec::new(),
    }
}

#[async_trait]
impl Protocol for ExternalProtocol {
    fn name(&self) -> &'static str {
        "external"
    }

    fn handles(&self, source: &str) -> bool {
        source.starts_with("ext:")
    }

    async fn probe(&self, source: &str) -> Result<Probed> {
        let params = serde_json::to_value(ProbeParams {
            source: source.to_owned(),
        })
        .map_err(|e| Error::Store(format!("не вдалося скласти params probe: {e}")))?;
        let value = match self.rpc_call("probe", params, None, None).await? {
            CallOutcome::Value(v) => v,
            CallOutcome::Cancelled => {
                return Err(Error::Store(
                    "probe скасовано — у контракті probe немає cancel".to_owned(),
                ));
            }
        };
        let parsed: ProbeResult = serde_json::from_value(value).map_err(|e| {
            Error::Store(format!("плагін віддав непридатний probe: {e}"))
        })?;
        Ok(probed_from(source, parsed))
    }

    async fn run(&self, ctx: RunContext, sink: &dyn ProgressSink) -> Result<Option<ResumeBlob>> {
        if ctx.targets.is_empty() {
            return Err(Error::Store(
                "ядро не дало жодного шляху для запису".to_owned(),
            ));
        }

        if ctx.cancel.is_cancelled() {
            return Ok(Some(Vec::new()));
        }

        let params = serde_json::to_value(RunParams {
            source: ctx.source.clone(),
            targets: ctx
                .targets
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
        })
        .map_err(|e| Error::Store(format!("не вдалося скласти params run: {e}")))?;

        let value = match self
            .rpc_call("run", params, Some(&ctx.cancel), Some(sink))
            .await?
        {
            CallOutcome::Cancelled => return Ok(Some(Vec::new())),
            CallOutcome::Value(v) => v,
        };

        if ctx.cancel.is_cancelled() {
            return Ok(Some(Vec::new()));
        }

        let parsed: RunResult = serde_json::from_value(value).map_err(|e| {
            Error::Store(format!("плагін віддав непридатний run: {e}"))
        })?;
        Ok(parsed.resume)
    }

    fn set_rate_limit(&self, _bytes_per_sec: u64) -> RateLimitSupport {
        RateLimitSupport::Unsupported
    }

    async fn verify(&self, ctx: &RunContext) -> Result<()> {
        for path in &ctx.targets {
            if !path.exists() {
                return Err(Error::Store(format!(
                    "файл {} зник одразу після завантаження",
                    path.display()
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_лише_схему_ext() {
        let p = ExternalProtocol::new("неважливо");
        assert!(p.handles("ext:x"));
        assert!(p.handles("ext:fail"));
        assert!(!p.handles("http://example.com/a"));
        assert!(!p.handles("https://example.com/a"));
        assert!(!p.handles("hls://master.m3u8"));
        assert_eq!(p.name(), "external");
    }

    #[test]
    fn ліміт_швидкості_чесно_непідтримуваний() {
        let p = ExternalProtocol::new("неважливо");
        assert_eq!(p.set_rate_limit(1000), RateLimitSupport::Unsupported);
    }

    #[test]
    fn хвіст_stderr_не_порожній_коли_є_байти() {
        assert_eq!(stderr_tail("  бум  \n".as_bytes()), "бум");
        assert!(stderr_tail(b"").is_empty());
    }
}
