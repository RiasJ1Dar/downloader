//! YouTube через зовнішній `yt-dlp`, не свій екстрактор.
//!
//! Бінарник шукається в PATH. Немає — чесна помилка, не тиха заглушка.
//! JSON `-J` не пишемо в журнал: там прямі URL.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use downloader_core::error::{Error, Result};
use downloader_core::protocol::{
    PlannedFile, Probed, ProgressSink, Protocol, RateLimitSupport, ResumeBlob, RunContext,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Модуль YouTube / yt-dlp.
pub struct YtdlpProtocol {
    /// Ім'я або шлях бінарника. У тестах підміняється.
    bin: PathBuf,
}

impl YtdlpProtocol {
    /// Шукати `yt-dlp` у PATH.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bin: PathBuf::from("yt-dlp"),
        }
    }

    #[must_use]
    pub fn with_bin(bin: impl Into<PathBuf>) -> Self {
        Self { bin: bin.into() }
    }
}

impl Default for YtdlpProtocol {
    fn default() -> Self {
        Self::new()
    }
}

fn хост(source: &str) -> Option<String> {
    let rest = source
        .strip_prefix("https://")
        .or_else(|| source.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    Some(host.trim_start_matches("www.").to_ascii_lowercase())
}

/// Чи це адреса YouTube.
#[must_use]
pub fn схожий_на_youtube(source: &str) -> bool {
    matches!(
        хост(source).as_deref(),
        Some("youtube.com" | "youtu.be" | "music.youtube.com" | "m.youtube.com")
    )
}

fn імʼя_з_title(title: &str) -> String {
    let mut s: String = title
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    s.truncate(180);
    let s = s.trim().trim_end_matches('.');
    if s.is_empty() {
        "video".to_owned()
    } else {
        s.to_owned()
    }
}

fn обрізати_секрети(stderr: &str) -> String {
    stderr
        .lines()
        .map(|line| {
            if line.contains("://") || line.to_ascii_lowercase().contains("token") {
                "[приховано]"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl Protocol for YtdlpProtocol {
    fn name(&self) -> &'static str {
        "ytdlp"
    }

    fn handles(&self, source: &str) -> bool {
        схожий_на_youtube(source)
    }

    async fn probe(&self, source: &str) -> Result<Probed> {
        let out = Command::new(&self.bin)
            .args(["-J", "--no-download", "--no-warnings", source])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|_| Error::Store("yt-dlp не знайдено в PATH".to_owned()))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if err.contains("not found") || err.contains("не знайдено") {
                return Err(Error::Store("yt-dlp не знайдено в PATH".to_owned()));
            }
            return Err(Error::Store(format!(
                "yt-dlp -J завершився з помилкою: {}",
                обрізати_секрети(&err)
            )));
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout)
            .map_err(|e| Error::Store(format!("yt-dlp віддав не JSON: {e}")))?;
        let title = v
            .get("title")
            .and_then(|x| x.as_str())
            .unwrap_or("video");
        let ext = v.get("ext").and_then(|x| x.as_str()).unwrap_or("mp4");
        let name = format!("{}.{}", імʼя_з_title(title), ext);
        Ok(Probed {
            final_url: source.to_owned(),
            total_size: v.get("filesize").and_then(|x| x.as_u64()),
            resumable: false,
            fingerprint: Some("ytdlp".to_owned()),
            files: vec![PlannedFile {
                suggested_name: name,
                size: v.get("filesize").and_then(|x| x.as_u64()),
                selected: true,
            }],
        })
    }

    async fn run(&self, ctx: RunContext, _sink: &dyn ProgressSink) -> Result<Option<ResumeBlob>> {
        let Some(dest) = ctx.targets.first() else {
            return Err(Error::Store(
                "ядро не дало жодного шляху для запису".to_owned(),
            ));
        };
        let шаблон = dest_шаблон(dest);
        let mut child = Command::new(&self.bin)
            .args(["--no-progress", "-o", &шаблон, &ctx.source])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| Error::Store("yt-dlp не знайдено в PATH".to_owned()))?;

        let stderr = child.stderr.take();
        let читання = async {
            let mut tail = String::new();
            if let Some(pipe) = stderr {
                let mut lines = BufReader::new(pipe).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if tail.len() < 4096 {
                        tail.push_str(&line);
                        tail.push('\n');
                    }
                }
            }
            tail
        };

        let wait = async {
            loop {
                if ctx.cancel.is_cancelled() {
                    if let Err(e) = child.start_kill() {
                        tracing::warn!("не вбити yt-dlp: {e}");
                    }
                    return Ok(None);
                }
                match child.try_wait() {
                    Ok(Some(status)) => return Ok(Some(status)),
                    Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
                    Err(e) => return Err(Error::Store(e.to_string())),
                }
            }
        };

        let (tail, status) = tokio::join!(читання, wait);
        let Some(status) = status? else {
            return Ok(Some(Vec::new()));
        };
        if !status.success() {
            return Err(Error::Store(format!(
                "yt-dlp впав: {}",
                обрізати_секрети(&tail)
            )));
        }
        Ok(None)
    }

    fn set_rate_limit(&self, _bytes_per_sec: u64) -> RateLimitSupport {
        RateLimitSupport::Unsupported
    }
}

fn dest_шаблон(dest: &Path) -> String {
    dest.to_string_lossy().into_owned()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "тест")]
mod tests {
    use super::*;

    #[test]
    fn впізнає_youtube() {
        let p = YtdlpProtocol::new();
        assert!(p.handles("https://www.youtube.com/watch?v=abc"));
        assert!(p.handles("https://youtu.be/abc"));
        assert!(p.handles("https://music.youtube.com/watch?v=abc"));
        assert!(!p.handles("https://example.com/watch?v=abc"));
        assert!(!p.handles("https://cdn.example/a.m3u8"));
    }

    #[tokio::test]
    async fn probe_без_бінарника_називає_path() {
        let p = YtdlpProtocol::with_bin("yt-dlp-немає-такого-бінарника-dl");
        let err = p
            .probe("https://www.youtube.com/watch?v=abc")
            .await
            .expect_err("має впасти");
        let msg = err.to_string();
        assert!(
            msg.contains("yt-dlp не знайдено в PATH"),
            "маємо: {msg}"
        );
    }

    #[test]
    fn title_чиститься() {
        assert_eq!(імʼя_з_title("a/b:c*"), "a_b_c_");
        assert_eq!(імʼя_з_title("   "), "video");
    }

    #[test]
    fn секрети_з_stderr_ховаються() {
        let t = обрізати_секрети("ok\nhttp://evil/token=1\nfail");
        assert!(t.contains("ok"));
        assert!(t.contains("[приховано]"));
        assert!(!t.contains("evil"));
    }
}
