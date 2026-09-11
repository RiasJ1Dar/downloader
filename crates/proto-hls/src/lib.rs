//! HLS як модуль контракту [`downloader_core::protocol::Protocol`].
//!
//! Сегменти качаємо рушієм `proto-http` (повтори, ліміт, обриви). AES-128
//! розшифровуємо тут: IV явний або з media sequence. Live без
//! `#EXT-X-ENDLIST` — окремий режим: маніфест перечитуємо, сегменти
//! забираємо щойно з'явились (вікно ковзає). DRM не обходимо.

mod decrypt;
mod playlist;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use downloader_core::error::{Error, Result};
use downloader_core::protocol::{
    Cancel, PlannedFile, Probed, Progress, ProgressSink, Protocol, RateLimitSupport,
    ResumeBlob, RunContext, Session,
};
use downloader_proto_http::apply_session;
use downloader_proto_http::download::{Options, download};
use reqwest::Client;

use decrypt::{iv_з_sequence, розшифрувати};
use playlist::{
    Варіант, Доріжка, Маніфест, Медіа, Сегмент, ТипДоріжки, розібрати, розібрати_доріжки,
    схожий_на_hls,
};

/// Модуль HLS: VOD і live-запис до `#EXT-X-ENDLIST` або скасування.
pub struct HlsProtocol {
    client: Client,
    rate_limit: Mutex<u64>,
    /// Для `probe`, де немає [`RunContext`]. `run` бере сесію з контексту.
    session: Mutex<Session>,
}

impl HlsProtocol {
    /// Створити модуль із власним HTTP-клієнтом.
    pub fn new() -> Result<Self> {
        let client = downloader_proto_http::зібрати_клієнт()?;
        Ok(Self {
            client,
            rate_limit: Mutex::new(0),
            session: Mutex::new(Session::default()),
        })
    }

    fn поточна_сесія(&self, ctx: Option<&Session>) -> Session {
        match ctx {
            Some(s) if !s.is_empty() => s.clone(),
            _ => self.session.lock().map(|g| g.clone()).unwrap_or_default(),
        }
    }
}

#[async_trait]
impl Protocol for HlsProtocol {
    fn name(&self) -> &'static str {
        "hls"
    }

    fn handles(&self, source: &str) -> bool {
        схожий_на_hls(source)
    }

    async fn probe(&self, source: &str) -> Result<Probed> {
        let session = self.поточна_сесія(None);
        let body = fetch_text(&self.client, source, &session).await?;
        match розібрати(body.as_bytes(), source)? {
            Маніфест::Master { mut варіанти } => {
                варіанти.sort_by_key(|v| v.bandwidth);
                let mut files = варіанти
                    .iter()
                    .map(|v| PlannedFile {
                        suggested_name: імʼя_варіанту(v.height, v.bandwidth),
                        size: None,
                        selected: false,
                    })
                    .collect::<Vec<_>>();
                if let Some(last) = files.last_mut() {
                    last.selected = true;
                }
                let доріжки = розібрати_доріжки(body.as_bytes(), source)?;
                додати_доріжки(&mut files, &доріжки);
                Ok(Probed {
                    final_url: source.to_owned(),
                    total_size: None,
                    resumable: false,
                    fingerprint: Some("hls-master".to_owned()),
                    files,
                })
            }
            Маніфест::Media(m) => {
                let fingerprint = if m.end_list {
                    "hls-media"
                } else {
                    "hls-live"
                };
                Ok(Probed {
                    final_url: source.to_owned(),
                    total_size: None,
                    resumable: false,
                    fingerprint: Some(fingerprint.to_owned()),
                    files: vec![PlannedFile {
                        suggested_name: "stream.ts".to_owned(),
                        size: None,
                        selected: true,
                    }],
                })
            }
        }
    }

    async fn run(&self, ctx: RunContext, sink: &dyn ProgressSink) -> Result<Option<ResumeBlob>> {
        if ctx.targets.is_empty() {
            return Err(Error::Store(
                "ядро не дало жодного шляху для запису".to_owned(),
            ));
        }
        let session = self.поточна_сесія(Some(&ctx.session));
        let body = fetch_text(&self.client, &ctx.source, &session).await?;
        let доріжки = розібрати_доріжки(body.as_bytes(), &ctx.source)?;
        // Якість — з першої відео-цілі, не audio-*/subs-*. Якщо всі targets
        // доріжки, відео з master не пишемо (немає куди), варіант не обираємо.
        let video_dest = ctx.targets.iter().find(|p| !це_імʼя_доріжки(p));
        let mut done = 0u64;

        match розібрати(body.as_bytes(), &ctx.source)? {
            Маніфест::Media(m) => {
                if !m.end_list {
                    let Some(dest) = video_dest else {
                        tracing::warn!(
                            "HLS live: немає відео-цілі, додаткові доріжки цей цикл не качаємо"
                        );
                        return Ok(None);
                    };
                    return self
                        .тягнути_live(&ctx.source, m, dest, sink, &ctx.cancel, &session)
                        .await;
                }
                if let Some(dest) = video_dest {
                    if m.сегменти.is_empty() {
                        return Err(Error::Store("media playlist без сегментів".to_owned()));
                    }
                    if let Some(blob) = self
                        .тягнути_сегменти(
                            &m.сегменти,
                            dest,
                            sink,
                            &ctx.cancel,
                            &mut done,
                            &session,
                        )
                        .await?
                    {
                        return Ok(Some(blob));
                    }
                }
            }
            Маніфест::Master { mut варіанти } => {
                варіанти.sort_by_key(|v| v.bandwidth);
                if let Some(dest) = video_dest {
                    let обраний = обрати_варіант(&варіанти, dest)?;
                    let media_txt = fetch_text(&self.client, &обраний.uri, &session).await?;
                    match розібрати(media_txt.as_bytes(), &обраний.uri)? {
                        Маніфест::Media(m) => {
                            if !m.end_list {
                                return self
                                    .тягнути_live(
                                        &обраний.uri,
                                        m,
                                        dest,
                                        sink,
                                        &ctx.cancel,
                                        &session,
                                    )
                                    .await;
                            }
                            if m.сегменти.is_empty() {
                                return Err(Error::Store(
                                    "media playlist без сегментів".to_owned(),
                                ));
                            }
                            if let Some(blob) = self
                                .тягнути_сегменти(
                                    &m.сегменти,
                                    dest,
                                    sink,
                                    &ctx.cancel,
                                    &mut done,
                                    &session,
                                )
                                .await?
                            {
                                return Ok(Some(blob));
                            }
                        }
                        Маніфест::Master { .. } => {
                            return Err(Error::Store(
                                "варіант master вказує знову на master".to_owned(),
                            ));
                        }
                    }
                } else {
                    tracing::warn!(
                        "усі targets — доріжки; відео з master не пишемо (немає відео-цілі)"
                    );
                }
            }
        }

        self.тягнути_додаткові(
            &ctx.targets,
            &доріжки,
            sink,
            &ctx.cancel,
            &mut done,
            &session,
        )
        .await
    }

    fn set_rate_limit(&self, bytes_per_sec: u64) -> RateLimitSupport {
        match self.rate_limit.lock() {
            Ok(mut g) => {
                *g = bytes_per_sec;
                RateLimitSupport::Applied
            }
            Err(_) => RateLimitSupport::Unsupported,
        }
    }

    fn set_session(&self, session: Session) {
        match self.session.lock() {
            Ok(mut g) => *g = session,
            Err(_) => tracing::error!("сесія HLS отруєна, cookie не застосовано"),
        }
    }
}

impl HlsProtocol {
    /// Live: забирати сегменти з ковзного вікна, поки не з'явиться ENDLIST
    /// або скасування. Хто чекає кінцевий плейлист — втратить випалі сегменти.
    async fn тягнути_live(
        &self,
        playlist_url: &str,
        перша: Медіа,
        dest: &Path,
        sink: &dyn ProgressSink,
        cancel: &Cancel,
        session: &Session,
    ) -> Result<Option<ResumeBlob>> {
        let tmp = dest.with_extension("hls-parts");
        std::fs::create_dir_all(&tmp)?;
        let mut seen = HashSet::new();
        let mut зібрані = Vec::new();
        let mut done = 0u64;
        let mut next_offset = 0u64;
        let mut discontinuity = false;
        let mut target_duration = перша.target_duration.max(1);
        let mut підряд_помилок = 0u8;
        let ліміт = self.rate_limit.lock().map(|g| *g).unwrap_or(0);
        let mut медіа = перша;
        loop {
            if cancel.is_cancelled() {
                return Ok(Some(Vec::new()));
            }
            for seg in &медіа.сегменти {
                if !seen.insert(seg.sequence) {
                    continue;
                }
                discontinuity |= seg.discontinuity;
                let part = tmp.join(format!("seg-{}.bin", seg.sequence));
                let bytes = качати_сегмент(
                    &self.client,
                    seg,
                    &part,
                    ліміт,
                    &mut next_offset,
                    session,
                )
                .await?;
                done += bytes.len() as u64;
                sink.report(Progress::Advanced { done });
                sink.report(Progress::Segments { count: seen.len() });
                зібрані.push(part);
            }
            if медіа.end_list {
                break;
            }
            tokio::time::sleep(пауза_live(target_duration)).await;
            if cancel.is_cancelled() {
                return Ok(Some(Vec::new()));
            }
            медіа = match розібрати_медіа(&self.client, playlist_url, dest, session).await {
                Ok((m, _)) => {
                    підряд_помилок = 0;
                    m
                }
                Err(e) => {
                    підряд_помилок = підряд_помилок.saturating_add(1);
                    if підряд_помилок >= 8 {
                        return Err(Error::Store(format!(
                            "HLS live: маніфест не читається 8 разів поспіль: {e}"
                        )));
                    }
                    tracing::warn!("live: не перечитати маніфест ({e}), ще спроба");
                    continue;
                }
            };
            target_duration = медіа.target_duration.max(1);
        }
        if зібрані.is_empty() {
            return Err(Error::Store(
                "HLS live завершився без жодного сегмента".to_owned(),
            ));
        }
        if discontinuity {
            tracing::warn!("EXT-X-DISCONTINUITY: склейка може мати зламані таймстемпи");
        }
        sink.report(Progress::TotalKnown { total: done });
        зшити_або_mp4(&зібрані, dest, discontinuity)?;
        if let Err(e) = std::fs::remove_dir_all(&tmp) {
            tracing::warn!("не прибрати тимчасові сегменти HLS: {e}");
        }
        Ok(None)
    }

    async fn тягнути_сегменти(
        &self,
        сегменти: &[Сегмент],
        dest: &Path,
        sink: &dyn ProgressSink,
        cancel: &Cancel,
        done: &mut u64,
        session: &Session,
    ) -> Result<Option<ResumeBlob>> {
        if сегменти.iter().any(|s| s.discontinuity) {
            tracing::warn!("EXT-X-DISCONTINUITY: склейка може мати зламані таймстемпи");
        }
        sink.report(Progress::Segments {
            count: сегменти.len(),
        });
        let tmp = dest.with_extension("hls-parts");
        std::fs::create_dir_all(&tmp)?;
        let mut зібрані = Vec::new();
        let mut next_offset = 0u64;
        let ліміт = self.rate_limit.lock().map(|g| *g).unwrap_or(0);
        for (i, seg) in сегменти.iter().enumerate() {
            if cancel.is_cancelled() {
                return Ok(Some(Vec::new()));
            }
            let part = tmp.join(format!("seg-{i}.bin"));
            let bytes = качати_сегмент(
                &self.client,
                seg,
                &part,
                ліміт,
                &mut next_offset,
                session,
            )
            .await?;
            *done = done.saturating_add(bytes.len() as u64);
            sink.report(Progress::Advanced { done: *done });
            зібрані.push(part);
        }
        sink.report(Progress::TotalKnown { total: *done });
        зшити_або_mp4(&зібрані, dest, сегменти.iter().any(|s| s.discontinuity))?;
        if let Err(e) = std::fs::remove_dir_all(&tmp) {
            tracing::warn!("не прибрати тимчасові сегменти HLS: {e}");
        }
        Ok(None)
    }

    /// audio-* як media playlist; subs .vtt — GET; subs .m3u8 — як медіа.
    /// Без URI — warn, не Err. Live-доріжка — GAP.
    async fn тягнути_додаткові(
        &self,
        targets: &[PathBuf],
        доріжки: &[Доріжка],
        sink: &dyn ProgressSink,
        cancel: &Cancel,
        done: &mut u64,
        session: &Session,
    ) -> Result<Option<ResumeBlob>> {
        for dest in targets {
            if !це_імʼя_доріжки(dest) {
                continue;
            }
            if cancel.is_cancelled() {
                return Ok(Some(Vec::new()));
            }
            let Some(d) = знайти_доріжку(доріжки, dest) else {
                tracing::warn!("немає доріжки в master для {}", dest.display());
                continue;
            };
            let Some(uri) = d.uri.as_deref() else {
                tracing::warn!("доріжка {} без URI, пропускаємо", dest.display());
                continue;
            };
            match d.kind {
                ТипДоріжки::Audio => {
                    if let Some(blob) = self
                        .качати_vod_доріжку(uri, dest, sink, cancel, done, session)
                        .await?
                    {
                        return Ok(Some(blob));
                    }
                }
                ТипДоріжки::Subtitles => {
                    if схожий_на_hls(uri) {
                        if let Some(blob) = self
                            .качати_vod_доріжку(uri, dest, sink, cancel, done, session)
                            .await?
                        {
                            return Ok(Some(blob));
                        }
                    } else {
                        let bytes = fetch_bytes(&self.client, uri, session).await?;
                        std::fs::write(dest, &bytes)?;
                        *done = done.saturating_add(bytes.len() as u64);
                        sink.report(Progress::Advanced { done: *done });
                        sink.report(Progress::TotalKnown { total: *done });
                    }
                }
            }
        }
        Ok(None)
    }

    async fn качати_vod_доріжку(
        &self,
        playlist_url: &str,
        dest: &Path,
        sink: &dyn ProgressSink,
        cancel: &Cancel,
        done: &mut u64,
        session: &Session,
    ) -> Result<Option<ResumeBlob>> {
        let txt = fetch_text(&self.client, playlist_url, session).await?;
        match розібрати(txt.as_bytes(), playlist_url)? {
            Маніфест::Media(m) => {
                if !m.end_list {
                    tracing::warn!("HLS live-доріжка {playlist_url}: цей цикл не качаємо");
                    return Ok(None);
                }
                if m.сегменти.is_empty() {
                    tracing::warn!("media playlist без сегментів: {playlist_url}");
                    return Ok(None);
                }
                self.тягнути_сегменти(&m.сегменти, dest, sink, cancel, done, session)
                    .await
            }
            Маніфест::Master { .. } => Err(Error::Store(format!(
                "доріжка вказує знову на master: {playlist_url}"
            ))),
        }
    }
}

fn пауза_live(target_duration: u64) -> Duration {
    Duration::from_millis(target_duration.saturating_mul(250).clamp(50, 1000))
}

async fn розібрати_медіа(
    client: &Client,
    source: &str,
    dest: &Path,
    session: &Session,
) -> Result<(Медіа, String)> {
    let body = fetch_text(client, source, session).await?;
    match розібрати(body.as_bytes(), source)? {
        Маніфест::Media(m) => Ok((m, source.to_owned())),
        Маніфест::Master { mut варіанти } => {
            варіанти.sort_by_key(|v| v.bandwidth);
            let обраний = обрати_варіант(&варіанти, dest)?;
            let media_txt = fetch_text(client, &обраний.uri, session).await?;
            match розібрати(media_txt.as_bytes(), &обраний.uri)? {
                Маніфест::Media(m) => Ok((m, обраний.uri.clone())),
                Маніфест::Master { .. } => Err(Error::Store(
                    "варіант master вказує знову на master".to_owned(),
                )),
            }
        }
    }
}

/// Якість з master: ім'я цілі (`360p.ts`, `360p (1).ts`) або найширший.
fn обрати_варіант<'a>(варіанти: &'a [Варіант], dest: &Path) -> Result<&'a Варіант> {
    let fallback = варіанти.last().ok_or_else(|| {
        Error::Store("master playlist без варіантів".to_owned())
    })?;
    let stem = dest
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let основа = основа_імені(stem);
    if основа.is_empty() {
        return Ok(fallback);
    }
    let want = format!("{основа}.ts");
    Ok(варіанти
        .iter()
        .find(|v| імʼя_варіанту(v.height, v.bandwidth) == want)
        .unwrap_or(fallback))
}

/// `unique_path` додає ` (N)` перед розширенням — відкидаємо цей хвіст.
fn основа_імені(stem: &str) -> &str {
    if let Some(i) = stem.rfind(" (") {
        let tail = &stem[i + 2..];
        if !tail.is_empty()
            && tail
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b')')
        {
            return &stem[..i];
        }
    }
    stem
}

async fn качати_сегмент(
    client: &Client,
    seg: &Сегмент,
    part: &Path,
    rate_limit: u64,
    next_offset: &mut u64,
    session: &Session,
) -> Result<Vec<u8>> {
    let mut bytes = if let Some((length, offset)) = seg.byte_range {
        let start = offset.unwrap_or(*next_offset);
        *next_offset = start.saturating_add(length);
        fetch_range(client, &seg.uri, start, length, session).await?
    } else {
        let opts = Options {
            parts: 1,
            rate_limit,
            session: session.clone(),
            ..Options::default()
        };
        download(client, &seg.uri, part, &opts)
            .await
            .map_err(|e| Error::Store(e.to_string()))?;
        std::fs::read(part)?
    };
    if let Some(k) = &seg.key {
        match k.метод {
            crate::decrypt::МетодКлюча::Aes128 => {}
        }
        let key_bytes = fetch_bytes(client, &k.uri, session).await?;
        let iv = k.iv.unwrap_or_else(|| iv_з_sequence(seg.sequence));
        bytes = розшифрувати(&bytes, &key_bytes, &iv)?;
        std::fs::write(part, &bytes)?;
    } else if seg.byte_range.is_some() {
        std::fs::write(part, &bytes)?;
    }
    Ok(bytes)
}

fn імʼя_варіанту(height: Option<u64>, bandwidth: u64) -> String {
    match height {
        Some(h) => format!("{h}p.ts"),
        None => format!("{bandwidth}bps.ts"),
    }
}

/// Після варіантів якості: audio, потім subs. Без URI — нічого качати.
fn додати_доріжки(files: &mut Vec<PlannedFile>, доріжки: &[Доріжка]) {
    for kind in [ТипДоріжки::Audio, ТипДоріжки::Subtitles] {
        for d in доріжки {
            if d.kind != kind {
                continue;
            }
            let Some(uri) = d.uri.as_deref() else {
                continue;
            };
            files.push(PlannedFile {
                suggested_name: імʼя_доріжки(d, uri),
                size: None,
                selected: match d.kind {
                    ТипДоріжки::Audio => d.default,
                    ТипДоріжки::Subtitles => false,
                },
            });
        }
    }
}

fn мітка_доріжки(d: &Доріжка) -> &str {
    d.language
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(d.name.as_str())
}

fn імʼя_доріжки(d: &Доріжка, uri: &str) -> String {
    let мітка = мітка_доріжки(d);
    match d.kind {
        ТипДоріжки::Audio => format!("audio-{мітка}.m3u8"),
        ТипДоріжки::Subtitles => {
            let path = uri.split(['?', '#']).next().unwrap_or(uri);
            let розширення = if path.to_ascii_lowercase().ends_with(".m3u8") {
                "m3u8"
            } else {
                "vtt"
            };
            format!("subs-{мітка}.{розширення}")
        }
    }
}

/// `audio-*` / `subs-*` з урахуванням `unique_path` (`audio-uk (1).m3u8`).
fn це_імʼя_доріжки(path: &Path) -> bool {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let основа = основа_імені(stem).to_ascii_lowercase();
    основа.starts_with("audio-") || основа.starts_with("subs-")
}

fn знайти_доріжку<'a>(доріжки: &'a [Доріжка], dest: &Path) -> Option<&'a Доріжка> {
    let stem = dest.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let основа = основа_імені(stem).to_ascii_lowercase();
    let (kind, мітка) = if let Some(rest) = основа.strip_prefix("audio-") {
        (ТипДоріжки::Audio, rest)
    } else {
        let rest = основа.strip_prefix("subs-")?;
        (ТипДоріжки::Subtitles, rest)
    };
    доріжки.iter().find(|d| {
        d.kind == kind && мітка_доріжки(d).eq_ignore_ascii_case(мітка)
    })
}

async fn fetch_text(client: &Client, url: &str, session: &Session) -> Result<String> {
    let resp = apply_session(client.get(url), session)
        .send()
        .await
        .map_err(|e| Error::Store(format!("GET {url}: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::Store(format!("GET {url}: HTTP {status}")));
    }
    resp.text()
        .await
        .map_err(|e| Error::Store(format!("тіло {url}: {e}")))
}

async fn fetch_range(
    client: &Client,
    url: &str,
    start: u64,
    length: u64,
    session: &Session,
) -> Result<Vec<u8>> {
    if length == 0 {
        return Ok(Vec::new());
    }
    let end = start.saturating_add(length).saturating_sub(1);
    let resp = apply_session(client.get(url), session)
        .header(reqwest::header::RANGE, format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(|e| Error::Store(format!("GET Range {url}: {e}")))?;
    let status = resp.status();
    if status != reqwest::StatusCode::PARTIAL_CONTENT && !status.is_success() {
        return Err(Error::Store(format!(
            "GET Range {url}: HTTP {status}"
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| Error::Store(format!("тіло Range {url}: {e}")))?;
    if status == reqwest::StatusCode::PARTIAL_CONTENT {
        if bytes.len() as u64 != length {
            return Err(Error::Store(format!(
                "Range {url}: чекали {length} байтів, маємо {}",
                bytes.len()
            )));
        }
        return Ok(bytes.to_vec());
    }
    // Сервер знехтував Range і віддав усе — ріжемо самі, не пишемо зайве.
    let start = usize::try_from(start).map_err(|_| {
        Error::Store("зсув BYTERANGE не вміщується в usize".to_owned())
    })?;
    let end = start
        .checked_add(usize::try_from(length).map_err(|_| {
            Error::Store("довжина BYTERANGE не вміщується в usize".to_owned())
        })?)
        .ok_or_else(|| Error::Store("переповнення BYTERANGE".to_owned()))?;
    if end > bytes.len() {
        return Err(Error::Store(format!(
            "BYTERANGE {start}..{end} поза тілом {}",
            bytes.len()
        )));
    }
    Ok(bytes[start..end].to_vec())
}

async fn fetch_bytes(client: &Client, url: &str, session: &Session) -> Result<Vec<u8>> {
    let resp = apply_session(client.get(url), session)
        .send()
        .await
        .map_err(|e| Error::Store(format!("GET {url}: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::Store(format!("GET {url}: HTTP {status}")));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| Error::Store(format!("тіло {url}: {e}")))
}

fn зшити(parts: &[PathBuf], dest: &Path) -> Result<()> {
    let mut out = std::fs::File::create(dest)?;
    for p in parts {
        let mut f = std::fs::File::open(p)?;
        std::io::copy(&mut f, &mut out)?;
    }
    Ok(())
}

/// Склеїти `.ts`. Якщо ціль `.mp4` і є `ffmpeg` — remux `copy` + `faststart`.
/// При `EXT-X-DISCONTINUITY` concat не надійний — лишаємо `.ts`.
fn зшити_або_mp4(parts: &[PathBuf], dest: &Path, discontinuity: bool) -> Result<()> {
    let хоче_mp4 = dest
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("mp4"));
    if !хоче_mp4 || discontinuity {
        if discontinuity && хоче_mp4 {
            tracing::warn!(
                "EXT-X-DISCONTINUITY: ffmpeg concat може зламати таймстемпи, лишаємо TS"
            );
        }
        return зшити(parts, dest);
    }
    let ts = dest.with_extension("ts");
    зшити(parts, &ts)?;
    match спробувати_ffmpeg_mp4(&ts, dest) {
        Ok(()) => {
            if let Err(e) = std::fs::remove_file(&ts) {
                tracing::warn!("не прибрати проміжний TS: {e}");
            }
            Ok(())
        }
        Err(e) => {
            tracing::warn!("ffmpeg remux не вийшов ({e}), лишаємо TS як є");
            std::fs::rename(&ts, dest).map_err(Error::from)
        }
    }
}

fn спробувати_ffmpeg_mp4(ts: &Path, dest: &Path) -> Result<()> {
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-loglevel",
            "error",
            "-i",
        ])
        .arg(ts)
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(dest)
        .status()
        .map_err(|e| Error::Store(format!("не запустити ffmpeg: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Store(format!(
            "ffmpeg завершився з кодом {status}"
        )))
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "у тестах падіння і є повідомлення")]
mod tests {
    use super::*;
    use downloader_core::protocol::Cancel;
    use downloader_testserver::EvilServer;
    use sha2::{Digest, Sha256};

    struct Німий;

    impl ProgressSink for Німий {
        fn report(&self, _: Progress) {}
    }

    #[test]
    fn впізнає_лише_маніфести() {
        let p = HlsProtocol::new().unwrap();
        assert!(p.handles("https://cdn.example/a/master.m3u8"));
        assert!(p.handles("http://127.0.0.1/x.m3u8?token=1"));
        assert!(p.handles("https://cdn.example/list.m3u"));
        assert!(
            !p.handles("https://cdn.example/video.mp4"),
            "звичайний HTTP не має перехоплювати HLS"
        );
    }

    #[tokio::test]
    async fn vod_склеює_сегменти_і_звіряє_хеш() {
        let server = EvilServer::start().await.unwrap();
        let url = server.url("/hls/vod/media.m3u8");
        let p = HlsProtocol::new().unwrap();
        let probed = p.probe(&url).await.unwrap();
        assert_eq!(probed.files.len(), 1);

        let dir = std::env::temp_dir().join(format!("hls-vod-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("out.ts");
        p.run(
            RunContext {
                task_id: 1,
                source: url,
                targets: vec![dest.clone()],
                resume: None,
                cancel: Cancel::new(),
                session: Session::default(),
            },
            &Німий,
        )
        .await
        .unwrap();

        let got = std::fs::read(&dest).unwrap();
        let mut expect = Vec::new();
        expect.extend_from_slice(b"SEG0-PAYLOAD-AAAAAAAAAAAAAAAA");
        expect.extend_from_slice(b"SEG1-PAYLOAD-BBBBBBBBBBBBBBBB");
        assert_eq!(got, expect, "склейка сегментів не збіглась");
        let digest = Sha256::digest(&got);
        assert_eq!(digest.len(), 32);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn master_обирає_найширший_варіант() {
        let server = EvilServer::start().await.unwrap();
        let url = server.url("/hls/vod/master.m3u8");
        let p = HlsProtocol::new().unwrap();
        let probed = p.probe(&url).await.unwrap();
        assert_eq!(probed.files.len(), 2);
        assert!(
            probed.files[1].selected,
            "мав бути вибраний варіант із більшим bitrate"
        );
        assert!(probed.files[1].suggested_name.contains("720"));
        server.shutdown().await;
    }

    #[tokio::test]
    async fn master_з_доріжками_кладе_audio_і_subs_у_files() {
        let server = EvilServer::start().await.unwrap();
        let url = server.url("/hls/media/master.m3u8");
        let p = HlsProtocol::new().unwrap();
        let probed = p.probe(&url).await.unwrap();

        let video = probed
            .files
            .iter()
            .find(|f| f.suggested_name.contains("720"))
            .unwrap();
        assert!(video.selected, "720p має бути selected");
        assert!(!video.suggested_name.is_empty());

        let audio = probed
            .files
            .iter()
            .find(|f| f.suggested_name.contains("audio"))
            .unwrap();
        assert!(audio.selected, "DEFAULT=YES audio має бути selected");
        assert!(!audio.suggested_name.is_empty());

        let subs = probed
            .files
            .iter()
            .find(|f| f.suggested_name.contains("subs"))
            .unwrap();
        assert!(!subs.selected, "субтитри за замовчуванням не качаємо");
        assert!(!subs.suggested_name.is_empty());

        server.shutdown().await;
    }

    #[tokio::test]
    async fn master_качає_audio_і_subs_разом_із_відео() {
        let server = EvilServer::start().await.unwrap();
        let url = server.url("/hls/media/master.m3u8");
        let p = HlsProtocol::new().unwrap();
        let probed = p.probe(&url).await.unwrap();

        let video_name = probed
            .files
            .iter()
            .find(|f| {
                !f.suggested_name.starts_with("audio-") && !f.suggested_name.starts_with("subs-")
            })
            .unwrap()
            .suggested_name
            .clone();
        let audio_name = probed
            .files
            .iter()
            .find(|f| f.suggested_name.starts_with("audio-"))
            .unwrap()
            .suggested_name
            .clone();
        let subs_name = probed
            .files
            .iter()
            .find(|f| f.suggested_name.starts_with("subs-"))
            .unwrap()
            .suggested_name
            .clone();
        assert_eq!(video_name, "720p.ts");
        assert_eq!(audio_name, "audio-uk.m3u8");
        assert_eq!(subs_name, "subs-uk.vtt");

        let dir = std::env::temp_dir().join(format!("hls-tracks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest_video = dir.join(&video_name);
        let dest_audio = dir.join(&audio_name);
        let dest_subs = dir.join(&subs_name);

        p.run(
            RunContext {
                task_id: 9,
                source: url,
                targets: vec![dest_video.clone(), dest_audio.clone(), dest_subs.clone()],
                resume: None,
                cancel: Cancel::new(),
                session: Session::default(),
            },
            &Німий,
        )
        .await
        .unwrap();

        let video = std::fs::read(&dest_video).unwrap();
        assert!(!video.is_empty(), "відео не має бути порожнім");
        let mut expect = Vec::new();
        expect.extend_from_slice(b"SEG0-PAYLOAD-AAAAAAAAAAAAAAAA");
        expect.extend_from_slice(b"SEG1-PAYLOAD-BBBBBBBBBBBBBBBB");
        assert_eq!(video, expect, "відео має бути склейкою seg0+seg1");

        let audio = std::fs::read(&dest_audio).unwrap();
        assert!(
            dest_audio.is_file() && !audio.is_empty(),
            "audio файл має існувати й не бути порожнім"
        );
        assert_eq!(audio, b"AUDIO-TRACK-PAYLOAD");

        let subs = std::fs::read_to_string(&dest_subs).unwrap();
        assert!(
            subs.contains("WEBVTT") || subs.contains("українські субтитри"),
            "субтитри мають містити WEBVTT або текст зі стенда, маємо {subs:?}"
        );

        server.shutdown().await;
    }

    #[tokio::test]
    async fn master_качає_якість_з_імені_цілі() {
        let server = EvilServer::start().await.unwrap();
        let url = server.url("/hls/vod/master.m3u8");
        let p = HlsProtocol::new().unwrap();
        let dir = std::env::temp_dir().join(format!("hls-q-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("360p.ts");
        p.run(
            RunContext {
                task_id: 5,
                source: url,
                targets: vec![dest.clone()],
                resume: None,
                cancel: Cancel::new(),
                session: Session::default(),
            },
            &Німий,
        )
        .await
        .unwrap();
        let got = std::fs::read(&dest).unwrap();
        assert_eq!(
            got, b"LOW-QUALITY-PAYLOAD-360P-AAAA",
            "ціль 360p.ts мала дати низьку якість, не 720p"
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn master_без_підказки_бере_найширший() {
        let server = EvilServer::start().await.unwrap();
        let url = server.url("/hls/vod/master.m3u8");
        let p = HlsProtocol::new().unwrap();
        let dir = std::env::temp_dir().join(format!("hls-qhi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("out.ts");
        p.run(
            RunContext {
                task_id: 6,
                source: url,
                targets: vec![dest.clone()],
                resume: None,
                cancel: Cancel::new(),
                session: Session::default(),
            },
            &Німий,
        )
        .await
        .unwrap();
        let got = std::fs::read(&dest).unwrap();
        let mut expect = Vec::new();
        expect.extend_from_slice(b"SEG0-PAYLOAD-AAAAAAAAAAAAAAAA");
        expect.extend_from_slice(b"SEG1-PAYLOAD-BBBBBBBBBBBBBBBB");
        assert_eq!(got, expect, "без 360p/720p в імені — найширший варіант");
        server.shutdown().await;
    }

    #[test]
    fn unique_path_не_ламає_вибір_якості() {
        assert_eq!(основа_імені("360p"), "360p");
        assert_eq!(основа_імені("360p (1)"), "360p");
        assert_eq!(основа_імені("360p (12)"), "360p");
        assert_eq!(основа_імені("video (copy)"), "video (copy)");
        assert!(це_імʼя_доріжки(Path::new("audio-uk.m3u8")));
        assert!(це_імʼя_доріжки(Path::new("audio-uk (1).m3u8")));
        assert!(це_імʼя_доріжки(Path::new("subs-uk.vtt")));
        assert!(!це_імʼя_доріжки(Path::new("720p.ts")));
        assert!(!це_імʼя_доріжки(Path::new("out.ts")));
    }

    #[tokio::test]
    async fn byterange_склеює_з_одного_файла() {
        let server = EvilServer::start().await.unwrap();
        let url = server.url("/hls/range/media.m3u8");
        let p = HlsProtocol::new().unwrap();
        let dir = std::env::temp_dir().join(format!("hls-br-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("out.ts");
        p.run(
            RunContext {
                task_id: 2,
                source: url,
                targets: vec![dest.clone()],
                resume: None,
                cancel: Cancel::new(),
                session: Session::default(),
            },
            &Німий,
        )
        .await
        .unwrap();
        let got = std::fs::read(&dest).unwrap();
        let mut expect = Vec::new();
        expect.extend_from_slice(b"SEG0-PAYLOAD-AAAAAAAAAAAAAAAA");
        expect.extend_from_slice(b"SEG1-PAYLOAD-BBBBBBBBBBBBBBBB");
        assert_eq!(got, expect);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn aes128_розшифровує_сегменти() {
        let server = EvilServer::start().await.unwrap();
        let url = server.url("/hls/aes/media.m3u8");
        let p = HlsProtocol::new().unwrap();
        let dir = std::env::temp_dir().join(format!("hls-aes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("out.ts");
        p.run(
            RunContext {
                task_id: 3,
                source: url,
                targets: vec![dest.clone()],
                resume: None,
                cancel: Cancel::new(),
                session: Session::default(),
            },
            &Німий,
        )
        .await
        .unwrap();
        let got = std::fs::read(&dest).unwrap();
        let mut expect = Vec::new();
        expect.extend_from_slice(b"SEG0-PAYLOAD-AAAAAAAAAAAAAAAA");
        expect.extend_from_slice(b"SEG1-PAYLOAD-BBBBBBBBBBBBBBBB");
        assert_eq!(got, expect, "AES-128 IV/sequence розʼїхались");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn live_probe_не_падає() {
        let server = EvilServer::start().await.unwrap();
        let url = server.url("/hls/live/media.m3u8");
        let p = HlsProtocol::new().unwrap();
        let probed = p.probe(&url).await.unwrap();
        assert_eq!(probed.fingerprint.as_deref(), Some("hls-live"));
        server.shutdown().await;
    }

    #[tokio::test]
    async fn live_забирає_сегмент_доки_не_випав_з_вікна() {
        // Стенд: 1–2 запити маніфесту → лише seg0; 3-й → лише seg1;
        // 4-й → ENDLIST. Хто чекав кінцевий плейлист — отримає тільки seg1.
        let server = EvilServer::start().await.unwrap();
        let url = server.url("/hls/live/media.m3u8");
        let p = HlsProtocol::new().unwrap();
        let dir = std::env::temp_dir().join(format!("hls-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("out.ts");
        p.run(
            RunContext {
                task_id: 4,
                source: url,
                targets: vec![dest.clone()],
                resume: None,
                cancel: Cancel::new(),
                session: Session::default(),
            },
            &Німий,
        )
        .await
        .unwrap();
        let got = std::fs::read(&dest).unwrap();
        let mut expect = Vec::new();
        expect.extend_from_slice(b"SEG0-PAYLOAD-AAAAAAAAAAAAAAAA");
        expect.extend_from_slice(b"SEG1-PAYLOAD-BBBBBBBBBBBBBBBB");
        assert_eq!(
            got, expect,
            "live мав зклеїти обидва сегменти, не лише вікно на ENDLIST"
        );
        server.shutdown().await;
    }
}
