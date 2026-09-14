//! Стрес-тест на гонки: багато потоків, дрібні сегменти, багато крадіжок.
//!
//! # Навіщо окремо від решти
//!
//! Гонка не падає щоразу — вона падає раз на кілька прогонів, коли збіглися
//! таймінги. Саме так у цьому проєкті й виявилась неатомарна пара «звірити
//! межу / записати»: звичайні тести були зелені, а один тест падав раз на
//! три-чотири прогони, і легко було списати це на «моргнуло».
//!
//! Тому тут навмисно створюються найгірші умови:
//!
//! * багато воркерів на маленький файл — черга завдань вичерпується
//!   майже одразу, і далі всі живуть **крадіжками** одне в одного;
//! * крихітний `min_chunk` — крадіжки відбуваються постійно, а не раз;
//! * ліміт швидкості — додає паузи, тобто розширює вікна між діями;
//! * повторення — одного проходу мало, щоб зловити рідкісний збіг.
//!
//! Головна перевірка — **SHA-256**. Гонка за сегментами дає файл правильної
//! довжини з переплутаними або втраченими шматками всередині, і тільки хеш
//! це показує.

// Це тест: падіння тут і є повідомленням про помилку. Заборона на
// `expect` і проковтнуті помилки призначена бойовому коду, а не перевіркам.
#![expect(
    clippy::let_underscore_must_use,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]

use downloader_proto_http::download::{Options, download};
use downloader_testserver::{EvilServer, expected_sha256};
use reqwest::Client;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn client() -> Client {
    Client::builder().build().unwrap_or_else(|_| Client::new())
}

struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!("dl-stress-{tag}-{unique}.bin"));
        Self(p)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(downloader_core::state::state_path(&self.0));
    }
}

fn sha256_of(path: &Path) -> anyhow::Result<String> {
    let data = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&data);
    Ok(hex::encode(h.finalize()))
}

/// Скільки разів повторювати кожен сценарій.
///
/// Двадцять — компроміс: досить, щоб рідкісний збіг таймінгів вилазив
/// регулярно, і достатньо швидко, щоб тест лишався в звичайному прогоні, а
/// не в окремому нічному.
const ПОВТОРІВ: usize = 20;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn багато_воркерів_на_дрібних_сегментах_не_псують_файл() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let scenario = "/plain/256k";
    let очікуваний = expected_sha256(scenario)?;

    for спроба in 0..ПОВТОРІВ {
        let tmp = Temp::new(&format!("many-{спроба}"));

        let opts = Options {
            // Воркерів більше, ніж початкових сегментів: майже всі почнуть
            // із крадіжки.
            parts: 16,
            // Дрібно: 256 КБ на шматки по 2 КБ — крадіжок будуть десятки.
            min_chunk: 2048,
            max_retries: 3,
            checkpoint_every: std::time::Duration::from_millis(20),
            ..Options::default()
        };

        let out = download(&client(), &s.url(scenario), &tmp.0, &opts).await?;

        assert_eq!(
            sha256_of(&tmp.0)?,
            очікуваний,
            "спроба {спроба}: файл зібрано неправильно при {} сегментах — \
             це гонка за межами сегментів, а не «моргнуло»",
            out.segments
        );
    }

    s.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ліміт_швидкості_не_відкриває_вікон_для_гонки() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let scenario = "/plain/128k";
    let очікуваний = expected_sha256(scenario)?;

    for спроба in 0..ПОВТОРІВ {
        let tmp = Temp::new(&format!("rate-{спроба}"));

        let opts = Options {
            parts: 12,
            min_chunk: 2048,
            max_retries: 3,
            // Саме ліміт свого часу й розбудив приспану гонку: пауза між
            // звіркою межі й записом розширює вікно, у яке встигає чужа
            // крадіжка.
            rate_limit: 512 * 1024,
            checkpoint_every: std::time::Duration::from_millis(20),
            ..Options::default()
        };

        let _ = download(&client(), &s.url(scenario), &tmp.0, &opts).await?;

        assert_eq!(
            sha256_of(&tmp.0)?,
            очікуваний,
            "спроба {спроба}: ліміт швидкості дав гонці шанс"
        );
    }

    s.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn повільний_сервер_із_багатьма_воркерами_тримає_цілісність() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    // Дросель на боці сервера дає багато дрібних чанків замість кількох
    // великих — тобто багато проходів через звірку межі.
    let scenario = "/slow-range/128k/1m";
    let очікуваний = expected_sha256(scenario)?;

    for спроба in 0..(ПОВТОРІВ / 2) {
        let tmp = Temp::new(&format!("slow-{спроба}"));

        let opts = Options {
            parts: 16,
            min_chunk: 2048,
            max_retries: 3,
            checkpoint_every: std::time::Duration::from_millis(10),
            ..Options::default()
        };

        download(&client(), &s.url(scenario), &tmp.0, &opts).await?;

        assert_eq!(
            sha256_of(&tmp.0)?,
            очікуваний,
            "спроба {спроба}: дрібні чанки виявили гонку"
        );
    }

    s.shutdown().await;
    Ok(())
}
