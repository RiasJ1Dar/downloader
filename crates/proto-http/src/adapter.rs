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

use std::sync::Arc;

use downloader_core::error::{Error, Result};
use downloader_core::protocol::{
    PlannedFile, Probed, Progress, ProgressSink, Protocol, RateLimitSupport, ResumeBlob,
    RunContext,
};
use reqwest::Client;

use crate::download::{Options, download_with_probe};
use crate::probe::probe;

/// Модуль завантаження по HTTP і HTTPS.
pub struct HttpProtocol {
    client: Client,
    /// Стеля швидкості. Змінюється ззовні, тому за м'ютексом.
    rate_limit: std::sync::Mutex<u64>,
    /// Скільки з'єднань відкривати на файл.
    parts: usize,
}

impl HttpProtocol {
    /// Створити модуль із власним HTTP-клієнтом.
    pub fn new(parts: usize) -> Result<Self> {
        let client = Client::builder()
            .build()
            .map_err(|e| Error::Store(format!("не вдалося створити HTTP-клієнт: {e}")))?;

        Ok(Self {
            client,
            rate_limit: std::sync::Mutex::new(0),
            parts,
        })
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
        let info = probe(&self.client, source)
            .await
            .map_err(|e| Error::Store(e.to_string()))?;

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
        })
    }

    async fn run(&self, ctx: RunContext, sink: &dyn ProgressSink) -> Result<Option<ResumeBlob>> {
        let Some(dest) = ctx.targets.first() else {
            return Err(Error::Store(
                "ядро не дало жодного шляху для запису".to_owned(),
            ));
        };

        let info = probe(&self.client, &ctx.source)
            .await
            .map_err(|e| Error::Store(e.to_string()))?;

        if let Some(total) = info.size {
            sink.report(Progress::TotalKnown { total });
        }

        // Місток між рушієм і контрактом: рушій знає про байти, sink — про
        // те, кому їх показати.
        //
        // Через канал, а не прямим викликом: `ProgressSink` приходить
        // посиланням із чужим часом життя, а колбек рушія має бути
        // `'static`. Канал розв'язує це без клонування слухача.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u64, usize)>();

        let опції = Options {
            parts: self.parts,
            rate_limit: self.rate_limit.lock().map(|g| *g).unwrap_or(0),
            cancel: Some(ctx.cancel.clone()),
            on_progress: Some(Arc::new(move |done, segments| {
                // Помилка надсилання означає лише, що слухач пішов, —
                // качання це не стосується.
                let _ = tx.send((done, segments));
            })),
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
            while let Some((done, segments)) = rx.recv().await {
                sink.report(Progress::Advanced { done });
                sink.report(Progress::Segments { count: segments });
            }
        };

        // Обидва живуть разом: доки качає — доти й пересилаємо.
        let (out, ()) = tokio::join!(качання, пересилання);
        let out = out.map_err(|e| Error::Store(e.to_string()))?;

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

        // Завдання завершене — стану відновлення не лишається.
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
