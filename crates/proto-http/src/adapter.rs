//! HTTP як звичайний модуль ядра.
//!
//! Тут рушій із [`crate::download`] загортається в контракт
//! [`downloader_core::protocol::Protocol`] — той самий, яким колись
//! під'єднається торент і яким уже зараз говорить протокол-пустушка в тестах
//! ядра.
//!
//! # Чому це не «зайвий шар»
//!
//! Спокуса викликати рушій прямо з ядра велика: він же поруч, і сигнатура
//! зручніша. Але саме так межа й ламається — тихо, і все продовжує
//! працювати. Через місяць у планувальнику з'являється `if url.ends_with(
//! ".m3u8")`, і обіцянка «торент доточиться новим модулем» перестає бути
//! правдою.
//!
//! Тому HTTP не має жодних привілеїв: ядро знає його рівно настільки, як
//! знатиме будь-який зовнішній плагін.

use std::sync::{Arc, Mutex};

use downloader_core::error::{Error, Result};
use downloader_core::protocol::{
    PlannedFile, Probed, Progress, ProgressSink, Protocol, RateLimitSupport, ResumeBlob,
    RunContext, Session,
};
use downloader_core::RateLimiter;
use reqwest::Client;

use crate::download::{DownloadError, Options, download_with_probe};
use crate::probe::{ProbeError, probe_with_session};

fn з_проби(e: ProbeError) -> Error {
    match e {
        ProbeError::BadStatus { url, status } if matches!(status, 401 | 403) => {
            Error::AuthRequired { url, status }
        }
        other => Error::Store(other.to_string()),
    }
}

fn з_качання(url: &str, e: DownloadError) -> Error {
    match e {
        DownloadError::BadStatus { status, .. } if matches!(status, 401 | 403) => {
            Error::AuthRequired {
                url: url.to_owned(),
                status,
            }
        }
        other => Error::Store(other.to_string()),
    }
}

/// Модуль завантаження по HTTP і HTTPS.
pub struct HttpProtocol {
    client: Client,
    /// Стеля швидкості. Змінюється ззовні, тому за м'ютексом.
    rate_limit: Mutex<u64>,
    /// Обмежувач швидкості для активних завантажень.
    limiter: Arc<RateLimiter>,
    /// Cookies / Referer останнього `set_session`. Для `probe`, де немає
    /// [`RunContext`]. `run` бере сесію з контексту, щоб паралельні
    /// завдання не перетирали одне одному.
    session: Mutex<Session>,
    /// Скільки з'єднань відкривати на файл.
    parts: usize,
}

impl HttpProtocol {
    /// Створити модуль із власним HTTP-клієнтом.
    pub fn new(parts: usize) -> Result<Self> {
        let client = crate::зібрати_клієнт()?;

        Ok(Self {
            client,
            rate_limit: Mutex::new(0),
            limiter: Arc::new(RateLimiter::unlimited()),
            session: Mutex::new(Session::default()),
            parts,
        })
    }

    fn поточна_сесія(&self, ctx: Option<&Session>) -> Session {
        match ctx {
            Some(s) if !s.is_empty() => s.clone(),
            _ => self.session.lock().map(|g| g.clone()).unwrap_or_default(),
        }
    }
}

#[async_trait::async_trait]
impl Protocol for HttpProtocol {
    fn name(&self) -> &'static str {
        "http"
    }

    fn handles(&self, source: &str) -> bool {
        // Знання «що таке http-посилання» живе тут, а не в ядрі.
        let s = source.trim();
        s.starts_with("http://") || s.starts_with("https://")
    }

    async fn probe(&self, source: &str) -> Result<Probed> {
        let session = self.поточна_сесія(None);
        let info = probe_with_session(&self.client, source, &session)
            .await
            .map_err(з_проби)?;

        Ok(Probed {
            total_size: info.size,
            resumable: info.resumable,
            fingerprint: info.validator.if_range_value().map(str::to_owned),
            // HTTP завжди дає рівно один файл. Контракт при цьому
            // багатофайловий — саме щоб торент і DASH не ламали модель.
            files: vec![PlannedFile {
                suggested_name: info
                    .filename()
                    .unwrap_or_else(|| "downloaded.bin".to_owned()),
                size: info.size,
                selected: true,
            }],
            final_url: info.final_url,
            variants: Vec::new(),
        })
    }

    async fn run(&self, ctx: RunContext, sink: &dyn ProgressSink) -> Result<Option<ResumeBlob>> {
        let Some(dest) = ctx.targets.first() else {
            return Err(Error::Store(
                "ядро не дало жодного шляху для запису".to_owned(),
            ));
        };

        let session = self.поточна_сесія(Some(&ctx.session));
        let info = probe_with_session(&self.client, &ctx.source, &session)
            .await
            .map_err(з_проби)?;

        if let Some(total) = info.size {
            sink.report(Progress::TotalKnown { total });
        }

        // Місток між рушієм і контрактом: рушій знає про байти, sink — про
        // те, кому їх показати.
        //
        // Через канал, а не прямим викликом: `ProgressSink` приходить
        // посиланням із чужим часом життя, а колбек рушія має бути
        // `'static`. Канал розв'язує це без клонування слухача.
        type Поступ = (u64, usize, Vec<downloader_core::protocol::PartProgress>);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Поступ>();

        let опції = Options {
            parts: self.parts,
            rate_limit: self.rate_limit.lock().map(|g| *g).unwrap_or(0),
            limiter: Some(self.limiter.clone()),
            cancel: Some(ctx.cancel.clone()),
            on_progress: Some(Arc::new(move |done, segments, parts| {
                // Помилка надсилання означає лише, що слухач пішов, —
                // качання це не стосується.
                if tx.send((done, segments, parts)).is_err() {
                    tracing::trace!("слухач прогресу пішов");
                }
            })),
            session,
            ..Options::default()
        };

        // ⚠️ `опції` мусять дропнутись **одразу після** качання, всередині
        // цього блоку.
        //
        // У них живе `tx`. Поки вони в області видимості, канал відкритий, і
        // `пересилання` нижче чекає на нього вічно — `join!` не завершується
        // ніколи. Так і сталось: наскрізний тест висів до таймауту, а причина
        // була не в мережі й не в ядрі, а в часі життя однієї змінної.
        let качання = async move {
            let out = download_with_probe(&self.client, &info, dest, &опції).await;
            drop(опції);
            out
        };

        let пересилання = async {
            while let Some((done, segments, parts)) = rx.recv().await {
                sink.report(Progress::Advanced { done });
                sink.report(Progress::Segments { count: segments });
                if !parts.is_empty() {
                    sink.report(Progress::Layout { parts });
                }
            }
        };

        // Обидва живуть разом: доки качає — доти й пересилаємо.
        let (out, ()) = tokio::join!(качання, пересилання);
        let out = out.map_err(|e| з_качання(&ctx.source, e))?;

        sink.report(Progress::Advanced { done: out.bytes });
        sink.report(Progress::Segments {
            count: out.segments,
        });

        if out.cancelled {
            // Зупинено на прохання — не помилка. Стан лежить у sidecar поруч
            // із файлом, тож ядру досить знати сам факт: непорожній blob є
            // сигналом «продовжити можна».
            return Ok(Some(Vec::new()));
        }

        // Лічильник у RAM не доказ: після truncate файл на диску має бути
        // рівно `out.bytes`. Інакше віддамо людині дірку правильної «довжини».
        downloader_core::verify::length(dest, out.bytes)?;

        // Завдання завершене — стану відновлення не лишається.
        Ok(None)
    }

    fn set_rate_limit(&self, bytes_per_sec: u64) -> RateLimitSupport {
        match self.rate_limit.lock() {
            Ok(mut g) => {
                *g = bytes_per_sec;
                self.limiter.set_limit(bytes_per_sec);
                RateLimitSupport::Applied
            }
            Err(_) => RateLimitSupport::Unsupported,
        }
    }

    fn set_session(&self, session: Session) {
        match self.session.lock() {
            Ok(mut g) => *g = session,
            Err(_) => tracing::error!("сесія HTTP отруєна, cookie не застосовано"),
        }
    }

    async fn verify(&self, ctx: &RunContext) -> Result<()> {
        // Рушій уже звірив довжину з `Content-Length` і впав би на
        // розбіжності. Тут лишається хіба переконатись, що файл на місці:
        // між завершенням і перевіркою його могли прибрати.
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
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]
mod tests {
    use super::*;
    use downloader_core::protocol::{Cancel, Progress};
    use downloader_testserver::{EvilServer, expected_sha256};
    use sha2::{Digest, Sha256};

    fn sha256_hex(data: &[u8]) -> String {
        format!("{:x}", Sha256::digest(data))
    }

    struct Німий;

    impl ProgressSink for Німий {
        fn report(&self, _: Progress) {}
    }

    #[tokio::test]
    async fn без_cookie_auth_це_authrequired() {
        let server = EvilServer::start().await.unwrap();
        let p = HttpProtocol::new(4).unwrap();
        let url = server.url("/auth/16k");
        let err = p.probe(&url).await.expect_err("без cookie має бути 403");
        match err {
            Error::AuthRequired { status: 403, .. } => {}
            other => panic!("очікували AuthRequired 403, маємо {other}"),
        }
        server.shutdown().await;
    }

    #[tokio::test]
    async fn cookie_і_referer_проходять_auth() {
        let server = EvilServer::start().await.unwrap();
        let p = HttpProtocol::new(4).unwrap();
        let session = Session::from_parts(
            Some("other=1; session=ok; more=2".to_owned()),
            Some("http://example.test/page".to_owned()),
        );
        p.set_session(session.clone());

        let url = server.url("/auth/16k");
        let probed = p.probe(&url).await.unwrap();
        assert_eq!(probed.total_size, Some(16 * 1024));

        let dir = std::env::temp_dir().join(format!(
            "http-auth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("auth.bin");
        p.run(
            RunContext {
                task_id: 1,
                source: url,
                targets: vec![dest.clone()],
                resume: None,
                cancel: Cancel::new(),
                session,
                variant: None,
            },
            &Німий,
        )
        .await
        .unwrap();

        let got = std::fs::read(&dest).unwrap();
        assert_eq!(got.len(), 16 * 1024);
        assert_eq!(sha256_hex(&got), expected_sha256("/auth/16k").unwrap());
        server.shutdown().await;
        drop(std::fs::remove_dir_all(&dir));
    }
}
