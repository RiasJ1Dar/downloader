//! DASH як модуль контракту [`downloader_core::protocol::Protocol`].
//!
//! VOD: розбираємо MPD, обираємо representation (найвища або за іменем
//! цілі, як HLS-якість), сегменти качаємо рушієм `proto-http` і склеюємо
//! в `dest`. Init-сегмент — першим. Live / динамічний MPD без фіксованого
//! списку — чесна відмова. DRM (ContentProtection / cenc / Widevine) не
//! обходимо.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use dash_mpd::{AdaptationSet, Period, Representation, SegmentList};
use downloader_core::error::{Error, Result};
use downloader_core::protocol::{
    Variant,
    Cancel, PlannedFile, Probed, Progress, ProgressSink, Protocol, RateLimitSupport,
    ResumeBlob, RunContext, Session,
};
use downloader_proto_http::download::{Options, download};
use reqwest::Client;

/// Модуль DASH: лише VOD із фіксованим списком сегментів.
pub struct DashProtocol {
    client: Client,
    rate_limit: Mutex<u64>,
    session: Mutex<Session>,
}

impl DashProtocol {
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
impl Protocol for DashProtocol {
    fn name(&self) -> &'static str {
        "dash"
    }

    fn handles(&self, source: &str) -> bool {
        схожий_на_dash(source)
    }

    async fn probe(&self, source: &str) -> Result<Probed> {
        let якості = self.розібрати_джерело(source, None).await?;
        let mut files = якості
            .iter()
            .map(|я| PlannedFile {
                suggested_name: я.name.clone(),
                size: None,
                selected: false,
            })
            .collect::<Vec<_>>();
        if let Some(last) = files.last_mut() {
            last.selected = true;
        }
        Ok(Probed {
            final_url: source.to_owned(),
            total_size: None,
            resumable: false,
            fingerprint: Some("dash-vod".to_owned()),
            files,
            variants: варіанти(&якості),
        })
    }

    /// Проба з урахуванням обраної якості.
    ///
    /// Різниця з звичайною одна: обраним лишається саме той файл, який
    /// людина вибрала, а не найширший за бітрейтом.
    async fn probe_variant(&self, source: &str, variant: Option<&str>) -> Result<Probed> {
        let mut probed = self.probe(source).await?;

        let Some(id) = variant else {
            return Ok(probed);
        };

        if !probed.files.iter().any(|f| f.suggested_name == id) {
            // MPD міг змінитись між пробою і вибором: краще качати те, що
            // є, ніж не качати нічого.
            return Ok(probed);
        }

        for f in &mut probed.files {
            f.selected = f.suggested_name == id;
        }

        Ok(probed)
    }

    async fn run(&self, ctx: RunContext, sink: &dyn ProgressSink) -> Result<Option<ResumeBlob>> {
        let dest = ctx.targets.first().ok_or_else(|| {
            Error::Store("ядро не дало жодного шляху для запису".to_owned())
        })?;
        let session = self.поточна_сесія(Some(&ctx.session));
        let якості = self.розібрати_джерело(&ctx.source, Some(&session)).await?;
        let обрана = за_вибором(&якості, ctx.variant.as_deref())
            .map_or_else(|| обрати_якість(&якості, dest), Ok)?;
        if обрана.сегменти.is_empty() {
            return Err(Error::Store(
                "обрана representation без сегментів".to_owned(),
            ));
        }
        // Найкращий звук окремою доріжкою. У DASH це звичайна річ: відео й
        // звук — різні Representation, і без зведення людина отримала б
        // німе відео, навіть не зрозумівши чому.
        let звук = якості
            .iter()
            .filter(|я| я.звук && !я.сегменти.is_empty())
            .max_by_key(|я| я.bandwidth);

        let (Some(звук), false) = (звук, обрана.звук) else {
            // Звуку окремо немає, або людина свідомо обрала саму доріжку
            // звуку — качаємо як просили.
            return self
                .тягнути_сегменти(&обрана.сегменти, dest, sink, &ctx.cancel, &session)
                .await;
        };

        if !downloader_ffmpeg::є() {
            // ⚠️ Мовчати тут не можна: людина отримає відео без звуку й
            // вважатиме це дефектом джерела.
            tracing::warn!(
                "звук лишається окремо: ffmpeg немає, зводити нічим —                  поставте його командою `dl ffmpeg-install`"
            );
            return self
                .тягнути_сегменти(&обрана.сегменти, dest, sink, &ctx.cancel, &session)
                .await;
        }

        let відео_файл = dest.with_extension("video.part");
        let звук_файл = dest.with_extension("audio.part");

        if let Some(blob) = self
            .тягнути_сегменти(&обрана.сегменти, &відео_файл, sink, &ctx.cancel, &session)
            .await?
        {
            return Ok(Some(blob));
        }

        if let Some(blob) = self
            .тягнути_сегменти(&звук.сегменти, &звук_файл, sink, &ctx.cancel, &session)
            .await?
        {
            return Ok(Some(blob));
        }

        downloader_ffmpeg::звести(&відео_файл, &звук_файл, dest)
            .await
            .map_err(|e| Error::Store(format!("не звести відео зі звуком: {e}")))?;

        for тимчасовий in [&відео_файл, &звук_файл] {
            if let Err(e) = std::fs::remove_file(тимчасовий) {
                tracing::warn!("не прибрати {}: {e}", тимчасовий.display());
            }
        }

        Ok(None)
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
        if let Ok(mut g) = self.session.lock() {
            *g = session;
        }
    }
}

impl DashProtocol {
    async fn розібрати_джерело(
        &self,
        source: &str,
        session: Option<&Session>,
    ) -> Result<Vec<Якість>> {
        let session = self.поточна_сесія(session);
        let body = fetch_text(&self.client, source, &session).await?;
        розібрати_mpd(&body, source)
    }

    async fn тягнути_сегменти(
        &self,
        сегменти: &[String],
        dest: &Path,
        sink: &dyn ProgressSink,
        cancel: &Cancel,
        session: &Session,
    ) -> Result<Option<ResumeBlob>> {
        sink.report(Progress::Segments {
            count: сегменти.len(),
        });
        let tmp = dest.with_extension("dash-parts");
        std::fs::create_dir_all(&tmp)?;
        let mut зібрані = Vec::new();
        let mut done = 0u64;
        let ліміт = self.rate_limit.lock().map(|g| *g).unwrap_or(0);
        for (i, url) in сегменти.iter().enumerate() {
            if cancel.is_cancelled() {
                return Ok(Some(Vec::new()));
            }
            let part = tmp.join(format!("seg-{i}.bin"));
            let opts = Options {
                parts: 1,
                rate_limit: ліміт,
                cancel: Some(cancel.clone()),
                session: session.clone(),
                ..Options::default()
            };
            let outcome = download(&self.client, url, &part, &opts)
                .await
                .map_err(|e| Error::Store(e.to_string()))?;
            if outcome.cancelled {
                return Ok(Some(Vec::new()));
            }
            done = done.saturating_add(outcome.bytes);
            sink.report(Progress::Advanced { done });
            зібрані.push(part);
        }
        sink.report(Progress::TotalKnown { total: done });
        зшити(&зібрані, dest)?;
        if let Err(e) = std::fs::remove_dir_all(&tmp) {
            tracing::warn!("не прибрати тимчасові сегменти DASH: {e}");
        }
        Ok(None)
    }
}

/// Representation з готовими абсолютними URL сегментів (init першим).
struct Якість {
    name: String,
    bandwidth: u64,
    /// Висота кадру, якщо MPD її називає. Потрібна переліку якостей.
    height: Option<u64>,
    /// Це доріжка звуку, а не відео.
    ///
    /// ⚠️ DASH роздає їх окремими Representation, і без цієї ознаки звук
    /// опинявся в переліку якостей нарівні з відео — людина обирала «0.1
    /// Мбіт/с» і отримувала файл без зображення.
    звук: bool,
    сегменти: Vec<String>,
}

/// Чи URL схожий на DASH-маніфест: шлях містить `.mpd`.
#[must_use]
pub fn схожий_на_dash(source: &str) -> bool {
    let path = source.split(['?', '#']).next().unwrap_or(source);
    path.to_ascii_lowercase().contains(".mpd")
}

fn розібрати_mpd(xml: &str, source: &str) -> Result<Vec<Якість>> {
    if xml_схоже_на_drm(xml) {
        return Err(Error::Store(
            "DASH захищено DRM — не обходимо".to_owned(),
        ));
    }
    let mpd = dash_mpd::parse(xml)
        .map_err(|e| Error::Store(format!("не розібрати MPD: {e}")))?;
    if є_drm(&mpd) {
        return Err(Error::Store(
            "DASH захищено DRM — не обходимо".to_owned(),
        ));
    }
    let dynamic = mpd
        .mpdtype
        .as_deref()
        .is_some_and(|t| t.eq_ignore_ascii_case("dynamic"));
    let mut якості = зібрати_якості(&mpd, source)?;
    if якості.is_empty() {
        if dynamic {
            return Err(Error::Store(
                "DASH live / динамічний MPD без фіксованого списку сегментів — не качаємо"
                    .to_owned(),
            ));
        }
        // ⚠️ Найчастіша причина — SegmentTemplate. Помилка мусить її назвати:
        // «без representation із сегментами» звучить як зіпсований маніфест,
        // хоч насправді маніфест цілком справний, а межа — у нас.
        return Err(Error::Store(
            "цей MPD описує сегменти через SegmentTemplate, а модуль поки розуміє лише SegmentList"
                .to_owned(),
        ));
    }
    if dynamic && якості.iter().all(|я| я.сегменти.is_empty()) {
        return Err(Error::Store(
            "DASH live / динамічний MPD без фіксованого списку сегментів — не качаємо"
                .to_owned(),
        ));
    }
    якості.sort_by_key(|я| я.bandwidth);
    Ok(якості)
}

fn xml_схоже_на_drm(xml: &str) -> bool {
    let l = xml.to_ascii_lowercase();
    l.contains("<contentprotection")
        || l.contains("widevine")
        || l.contains("urn:mpeg:dash:mp4protection")
        || l.contains("edef8ba9-79d6-4ace-a3c8-27dcd51d21ed")
        || l.contains("sample-aes")
}

fn є_drm(mpd: &dash_mpd::MPD) -> bool {
    if захист_drm(&mpd.ContentProtection) {
        return true;
    }
    mpd.periods.iter().any(|p| {
        захист_drm(&p.ContentProtection)
            || p.adaptations.iter().any(|a| {
                захист_drm(&a.ContentProtection)
                    || a.representations
                        .iter()
                        .any(|r| захист_drm(&r.ContentProtection))
            })
    })
}

fn захист_drm(xs: &[dash_mpd::ContentProtection]) -> bool {
    !xs.is_empty()
}

fn зібрати_якості(mpd: &dash_mpd::MPD, source: &str) -> Result<Vec<Якість>> {
    let mut out = Vec::new();
    let mpd_base = база(source, mpd.base_url.first().map(|b| b.base.as_str()))?;
    for period in &mpd.periods {
        let period_base = база(&mpd_base, period.BaseURL.first().map(|b| b.base.as_str()))?;
        for adaptation in &period.adaptations {
            let aset_base = база(
                &period_base,
                adaptation.BaseURL.first().map(|b| b.base.as_str()),
            )?;
            for rep in &adaptation.representations {
                let rep_base = база(&aset_base, rep.BaseURL.first().map(|b| b.base.as_str()))?;
                let Some(list) = сегментний_список(rep, adaptation, period) else {
                    continue;
                };
                let сегменти = сегменти_списку(list, &rep_base)?;
                if сегменти.is_empty() {
                    continue;
                }
                let bandwidth = rep.bandwidth.unwrap_or(0);
                let height = rep.height.or(adaptation.height);
                let звук = це_звук(adaptation, rep);
                out.push(Якість {
                    name: імʼя_якості(height, bandwidth),
                    bandwidth,
                    height,
                    звук,
                    сегменти,
                });
            }
        }
    }
    Ok(out)
}

fn сегментний_список<'a>(
    r: &'a Representation,
    a: &'a AdaptationSet,
    p: &'a Period,
) -> Option<&'a SegmentList> {
    r.SegmentList
        .as_ref()
        .or(a.SegmentList.as_ref())
        .or(p.SegmentList.as_ref())
}

fn сегменти_списку(list: &SegmentList, base: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if let Some(init) = &list.Initialization
        && let Some(url) = &init.sourceURL
    {
        out.push(абсолютний(base, url)?);
    }
    for su in &list.segment_urls {
        if let Some(media) = &su.media {
            out.push(абсолютний(base, media)?);
        }
    }
    Ok(out)
}

fn база(поточна: &str, rel: Option<&str>) -> Result<String> {
    match rel {
        Some(r) if !r.is_empty() => абсолютний(поточна, r),
        _ => Ok(поточна.to_owned()),
    }
}

fn імʼя_якості(height: Option<u64>, bandwidth: u64) -> String {
    match height {
        Some(h) => format!("{h}p.mp4"),
        None => format!("{bandwidth}bps.mp4"),
    }
}

/// Якість з dest (`720p.mp4`, `720p (1).mp4`) або найвища.
/// Чи це доріжка звуку.
///
/// Дивимось `contentType`, потім `mimeType` — у різних MPD заповнене то
/// одне, то інше. Коли не сказано нічого, вважаємо відео: помилитись у
/// цей бік безпечніше, бо відео без звуку людина принаймні побачить.
fn це_звук(adaptation: &AdaptationSet, rep: &Representation) -> bool {
    let тип = adaptation
        .contentType
        .as_deref()
        .or(adaptation.mimeType.as_deref())
        .or(rep.mimeType.as_deref())
        .unwrap_or("");

    тип.to_ascii_lowercase().contains("audio")
}

/// Перелік якостей для людини: від найкращої до найгіршої.
///
/// ⚠️ Ідентифікатор — те саме ім'я, яким модуль назве файл (`720p.mp4`).
/// Одна річ має одну назву: інакше довелося б тримати ще одну відповідність
/// «якість ↔ файл» і стежити, щоб вона не розійшлася.
fn варіанти(якості: &[Якість]) -> Vec<Variant> {
    якості
        .iter()
        .rev()
        .filter(|я| !я.звук)
        .map(|я| Variant {
            id: я.name.clone(),
            label: match я.height {
                Some(h) => format!("{h}p"),
                None => мегабіти(я.bandwidth),
            },
            height: я.height.and_then(|h| u32::try_from(h).ok()),
            // MPD знає тривалість, але розмір сегментів — ні; чесніше
            // промовчати, ніж показати вигадане число.
            size: None,
            note: я.height.map(|_| мегабіти(я.bandwidth)),
        })
        .collect()
}

fn мегабіти(bandwidth: u64) -> String {
    format!("{:.1} Мбіт/с", bandwidth as f64 / 1_000_000.0)
}

/// Якість за явним вибором людини; `None` — вибору не було.
fn за_вибором<'a>(якості: &'a [Якість], вибір: Option<&str>) -> Option<&'a Якість> {
    let id = вибір?;
    якості.iter().find(|я| я.name == id)
}

fn обрати_якість<'a>(якості: &'a [Якість], dest: &Path) -> Result<&'a Якість> {
    let fallback = якості.last().ok_or_else(|| {
        Error::Store("MPD без representation".to_owned())
    })?;
    let stem = dest
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let основа = основа_імені(stem);
    if основа.is_empty() {
        return Ok(fallback);
    }
    let want = format!("{основа}.mp4");
    Ok(якості
        .iter()
        .find(|я| я.name == want)
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

fn абсолютний(base: &str, rel: &str) -> Result<String> {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return Ok(rel.to_owned());
    }
    let без_якоря = base.split(['?', '#']).next().unwrap_or(base);
    if rel.starts_with('/') {
        let scheme_end = без_якоря.find("://").map(|i| i + 3).ok_or_else(|| {
            Error::Store("базовий URL без схеми".to_owned())
        })?;
        let host_end = без_якоря[scheme_end..]
            .find('/')
            .map(|i| scheme_end + i)
            .unwrap_or(без_якоря.len());
        return Ok(format!("{}{rel}", &без_якоря[..host_end]));
    }
    let dir = match без_якоря.rfind('/') {
        Some(i) if i + 1 > без_якоря.find("://").map(|j| j + 3).unwrap_or(0) => {
            &без_якоря[..=i]
        }
        _ => без_якоря,
    };
    Ok(format!("{dir}{rel}"))
}

fn з_сесією(req: reqwest::RequestBuilder, session: &Session) -> reqwest::RequestBuilder {
    let mut req = req;
    if let Some(c) = session.cookies.as_deref() {
        req = req.header(reqwest::header::COOKIE, c);
    }
    if let Some(r) = session.referer.as_deref() {
        req = req.header(reqwest::header::REFERER, r);
    }
    req
}

async fn fetch_text(client: &Client, url: &str, session: &Session) -> Result<String> {
    let resp = з_сесією(client.get(url), session)
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

fn зшити(parts: &[PathBuf], dest: &Path) -> Result<()> {
    let mut out = std::fs::File::create(dest)?;
    for p in parts {
        let mut f = std::fs::File::open(p)?;
        std::io::copy(&mut f, &mut out)?;
    }
    Ok(())
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
    fn перелік_якостей_іде_від_найкращої() {
        let якості = vec![
            Якість { name: "360p.mp4".to_owned(), bandwidth: 400_000, height: Some(360), звук: false, сегменти: vec![] },
            Якість { name: "720p.mp4".to_owned(), bandwidth: 1_500_000, height: Some(720), звук: false, сегменти: vec![] },
            Якість { name: "96000bps.mp4".to_owned(), bandwidth: 96_000, height: None, звук: false, сегменти: vec![] },
            // Доріжка звуку: у переліку якостей їй не місце.
            Якість { name: "128000bps.mp4".to_owned(), bandwidth: 128_000, height: None, звук: true, сегменти: vec![] },
        ];

        // Модуль тримає якості за зростанням бітрейта — перелік має бути навпаки.
        let mut за_бітрейтом = якості;
        за_бітрейтом.sort_by_key(|я| я.bandwidth);

        let перелік = варіанти(&за_бітрейтом);
        assert_eq!(
            перелік.iter().map(|v| v.height).collect::<Vec<_>>(),
            vec![Some(720), Some(360), None]
        );

        // ⚠️ Без RESOLUTION у MPD висоти немає — лишається бітрейт.
        let останній = перелік.last().map(|v| v.label.clone()).unwrap_or_default();
        assert!(
            останній.contains("Мбіт/с"),
            "варіант без висоти має назвати бітрейт: {останній}"
        );

        // Ідентифікатор мусить збігатися з іменем файла, інакше вибір людини
        // не знайде свого варіанта.
        for v in &перелік {
            assert!(
                за_вибором(&за_бітрейтом, Some(&v.id)).is_some(),
                "варіант {} загубився",
                v.id
            );
        }
        assert!(за_вибором(&за_бітрейтом, Some("вигадка.mp4")).is_none());
        assert!(за_вибором(&за_бітрейтом, None).is_none());

        // ⚠️ Доріжка звуку — не «якість відео». Поки вона стояла в переліку
        // нарівні з відео, людина могла обрати «0.1 Мбіт/с» і отримати файл
        // без зображення.
        assert!(
            !перелік.iter().any(|v| v.id == "128000bps.mp4"),
            "звук потрапив у перелік якостей: {перелік:?}"
        );
    }

    #[test]
    fn впізнає_лише_маніфести() {
        let p = DashProtocol::new().unwrap();
        assert!(p.handles("https://cdn.example/a/manifest.mpd"));
        assert!(p.handles("http://127.0.0.1/x.mpd?token=1"));
        assert!(p.handles("https://cdn.example/list.MPD"));
        assert!(
            !p.handles("https://cdn.example/video.mp4"),
            "звичайний HTTP не має перехоплювати DASH"
        );
        assert!(
            !p.handles("https://cdn.example/a/master.m3u8"),
            "HLS не має перехоплювати DASH"
        );
    }

    #[tokio::test]
    async fn vod_склеює_init_і_сегменти() {
        let server = EvilServer::start().await.unwrap();
        let url = server.url("/dash/vod/manifest.mpd");
        let p = DashProtocol::new().unwrap();
        let probed = p.probe(&url).await.unwrap();
        assert_eq!(probed.files.len(), 2);
        assert!(
            probed.files[1].selected,
            "мав бути вибраний варіант із більшим bitrate"
        );
        assert!(probed.files[1].suggested_name.contains("720"));

        let dir = std::env::temp_dir().join(format!("dash-vod-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("out.mp4");
        p.run(
            RunContext {
                task_id: 1,
                source: url,
                targets: vec![dest.clone()],
                resume: None,
                cancel: Cancel::new(),
                session: Session::default(),
                variant: None,
            },
            &Німий,
        )
        .await
        .unwrap();

        let got = std::fs::read(&dest).unwrap();
        let mut expect = Vec::new();
        expect.extend_from_slice(b"INIT-PAYLOAD-DASH-AAAAAAAAAA");
        expect.extend_from_slice(b"SEG0-PAYLOAD-AAAAAAAAAAAAAAAA");
        expect.extend_from_slice(b"SEG1-PAYLOAD-BBBBBBBBBBBBBBBB");
        assert_eq!(got, expect, "склейка init+seg0+seg1 не збіглась");
        let digest = Sha256::digest(&got);
        assert_eq!(digest.len(), 32);
        let _ = std::fs::remove_dir_all(&dir);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn dest_обирає_якість_за_іменем() {
        let server = EvilServer::start().await.unwrap();
        let url = server.url("/dash/vod/manifest.mpd");
        let p = DashProtocol::new().unwrap();
        let probed = p.probe(&url).await.unwrap();
        assert_eq!(probed.files[0].suggested_name, "360p.mp4");
        assert!(!probed.files[0].selected);

        let dir = std::env::temp_dir().join(format!("dash-360-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("360p.mp4");
        p.run(
            RunContext {
                task_id: 2,
                source: url,
                targets: vec![dest.clone()],
                resume: None,
                cancel: Cancel::new(),
                session: Session::default(),
                variant: None,
            },
            &Німий,
        )
        .await
        .unwrap();
        let got = std::fs::read(&dest).unwrap();
        assert!(got.starts_with(b"INIT-PAYLOAD-DASH-AAAAAAAAAA"));
        let _ = std::fs::remove_dir_all(&dir);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn drm_чесно_відмовляє() {
        let server = EvilServer::start().await.unwrap();
        let url = server.url("/dash/vod/drm.mpd");
        let p = DashProtocol::new().unwrap();
        let err = p.probe(&url).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("DASH захищено DRM — не обходимо"),
            "очікували відмову DRM, маємо: {msg}"
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn live_чесно_відмовляє() {
        let server = EvilServer::start().await.unwrap();
        let url = server.url("/dash/vod/live.mpd");
        let p = DashProtocol::new().unwrap();
        let err = p.probe(&url).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("динамічний") || msg.contains("live"),
            "очікували відмову live, маємо: {msg}"
        );
        server.shutdown().await;
    }
}
