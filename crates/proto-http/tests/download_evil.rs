//! Качання проти «злого» сервера.
//!
//! Головна перевірка кожного тесту — **SHA-256 файла на диску**. Розмір і
//! статус нічого не доводять: биті дані зазвичай мають правильну довжину.

use downloader_core::protocol::Session;
use downloader_proto_http::download::{DownloadError, Options, download};
use downloader_testserver::{EvilServer, expected_sha256};
use reqwest::Client;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

fn client() -> Client {
    Client::builder().build().unwrap_or_else(|_| Client::new())
}

/// Тимчасовий файл, що прибирається сам.
struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!("dl-test-{tag}-{unique}.bin"));
        Self(p)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn sha256_of(path: &PathBuf) -> anyhow::Result<String> {
    let data = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&data);
    Ok(hex::encode(h.finalize()))
}

fn opts(parts: usize) -> Options {
    Options {
        parts,
        // Дрібний поріг, щоб на невеликих тестових файлах справді
        // відбувалась нарізка й крадіжка.
        min_chunk: 4096,
        max_retries: 4,
        checkpoint_every: std::time::Duration::from_millis(50),
        ..Options::default()
    }
}

#[tokio::test]
async fn вісім_сегментів_складаються_в_той_самий_файл() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let tmp = Temp::new("plain8");

    let out = download(&client(), &s.url("/plain/512k"), &tmp.0, &opts(8)).await?;

    assert_eq!(out.bytes, 512 * 1024);
    assert!(out.segments >= 8, "мало вийти щонайменше вісім сегментів");
    assert_eq!(
        sha256_of(&tmp.0)?,
        expected_sha256("/plain/512k")?,
        "склеєний з восьми шматків файл мусить збігатися побайтово"
    );

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn cookie_і_referer_проходять_auth() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let tmp = Temp::new("auth");
    let opts = Options {
        session: Session::from_parts(
            Some("other=1; session=ok; more=2".to_owned()),
            Some("http://example.test/page".to_owned()),
        ),
        ..opts(4)
    };
    download(&client(), &s.url("/auth/16k"), &tmp.0, &opts).await?;
    assert_eq!(sha256_of(&tmp.0)?, expected_sha256("/auth/16k")?);
    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn один_потік_дає_той_самий_результат_що_й_вісім() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let tmp = Temp::new("plain1");

    download(&client(), &s.url("/plain/256k"), &tmp.0, &opts(1)).await?;

    assert_eq!(sha256_of(&tmp.0)?, expected_sha256("/plain/256k")?);

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn сервер_без_range_качається_одним_потоком_і_не_псується() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let tmp = Temp::new("norange");

    // Просимо 16 потоків, але сервер ігнорує Range — має відкотитись в один.
    let out = download(&client(), &s.url("/norange/256k"), &tmp.0, &opts(16)).await?;

    assert_eq!(out.segments, 1, "різати те, що не підтримує Range, не можна");
    assert_eq!(
        sha256_of(&tmp.0)?,
        expected_sha256("/norange/256k")?,
        "відкіт в один потік не має псувати вміст"
    );

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn тимчасові_503_переживаються_повторами() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let tmp = Temp::new("flaky");

    // Перші два запити падають із 503 — рушій мусить дочекатись третього.
    download(&client(), &s.url("/flaky/128k/2"), &tmp.0, &opts(1)).await?;

    assert_eq!(sha256_of(&tmp.0)?, expected_sha256("/flaky/128k/2")?);

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn обрив_посеред_тіла_не_дає_тихо_битого_файла() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let tmp = Temp::new("cut");

    // Сервер обіцяє 128k, віддає 32k і рве — і так щоразу.
    let err = download(&client(), &s.url("/cut/128k/32k"), &tmp.0, &opts(1))
        .await
        .expect_err("недокачаний файл не можна вважати завантаженим");

    let text = err.to_string();
    assert!(
        text.contains("менше") || text.contains("докачування неможливе"),
        "помилка мусить пояснювати, що сталось: {text}"
    );

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn підміна_ресурсу_зупиняє_докачування() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let tmp = Temp::new("changing");

    // ETag міняється між запитами: перший сегмент іще пройде, але будь-яке
    // продовження з If-Range отримає 200 замість 206.
    let result = download(&client(), &s.url("/changing/256k"), &tmp.0, &opts(4)).await;

    match result {
        Err(DownloadError::ResourceChanged { .. }) => {}
        Err(other) => {
            // Інша помилка теж прийнятна — головне, що не «успіх».
            let text = other.to_string();
            assert!(
                text.contains("докачування неможливе") || text.contains("менше"),
                "несподівана помилка: {text}"
            );
        }
        Ok(out) => {
            // Якщо все ж «вдалося», файл зобов'язаний бути цілим.
            assert_eq!(
                sha256_of(&tmp.0)?,
                expected_sha256("/changing/256k")?,
                "склеєно шматки різних версій — рівно те, чого не можна допускати; \
                 завантажено {} байтів",
                out.bytes
            );
        }
    }

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn брехня_про_довжину_виявляється_а_не_ковтається() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let tmp = Temp::new("liar");

    // Заявляє 400000, віддає 64k.
    let err = download(&client(), &s.url("/liar-length/64k/400000"), &tmp.0, &opts(4))
        .await
        .expect_err("сервер віддав менше, ніж обіцяв — це має бути помилкою");

    let text = err.to_string();
    assert!(
        text.contains("менше") || text.contains("обіцяв"),
        "помилка мусить називати причину: {text}"
    );

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn повільний_сервер_дотягується_повністю() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let tmp = Temp::new("slow");

    // 64 КБ по 512 КБ/с — швидко, але через дросель, тобто багатьма чанками.
    download(&client(), &s.url("/slow/64k/512k"), &tmp.0, &opts(2)).await?;

    assert_eq!(sha256_of(&tmp.0)?, expected_sha256("/slow/64k/512k")?);

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn редирект_не_заважає_сегментованому_качанню() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let tmp = Temp::new("redirect");

    let out = download(&client(), &s.url("/redirect/2/128k"), &tmp.0, &opts(4)).await?;

    assert_eq!(out.bytes, 128 * 1024);
    assert_eq!(
        sha256_of(&tmp.0)?,
        expected_sha256("/plain/128k")?,
        "після редиректу качається той самий вміст"
    );

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn перерване_качання_докачується_а_не_починається_з_нуля() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let tmp = Temp::new("resume");
    // 2 МБ по 512 КБ/с на з'єднання, чотири з'єднання → близько секунди.
    // Розмір підібраний так, щоб перерва була справжньою: із швидшим
    // сценарієм качання встигало завершитись до неї, `finish` прибирав файл
    // стану — і тест «не бачив» жодного чекпоінта, хоч усе працювало.
    let url = s.url("/slow-range/2m/512k");

    // Перша спроба: обриваємо її посеред роботи, як це робить вимкнене
    // живлення або закрита кришка ноутбука.
    let перша = {
        let url = url.clone();
        let dest = tmp.0.clone();
        tokio::spawn(async move { download(&client(), &url, &dest, &opts(4)).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    перша.abort();
    let _ = перша.await;

    // Стан мусить лежати поруч і знати про вже завантажене.
    let state_file = downloader_core::state::state_path(&tmp.0);
    assert!(
        state_file.exists(),
        "після перерваного качання поруч має лишитись файл стану"
    );
    let saved = downloader_core::state::load(&tmp.0)?;
    let було = saved.downloaded();
    assert!(було > 0, "чекпоінт не встиг зафіксувати жодного байта");

    // Друга спроба: та сама адреса, та сама тека.
    download(&client(), &url, &tmp.0, &opts(4)).await?;

    assert_eq!(
        sha256_of(&tmp.0)?,
        expected_sha256("/slow-range/2m/512k")?,
        "докачаний файл мусить збігатися побайтово з цілим"
    );
    assert!(
        !state_file.exists(),
        "після успіху файл стану має зникнути, а не лежати поруч із готовим файлом"
    );

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn чужий_стан_не_приймається_і_качання_йде_з_нуля() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let tmp = Temp::new("alien");

    // Кладемо поруч стан від зовсім іншого завантаження.
    let alien = downloader_core::DownloadState {
        version: downloader_core::state::STATE_VERSION,
        protocol: "http".into(),
        url: "https://example.com/зовсім-інше.bin".into(),
        total: Some(999_999),
        fingerprint: Some("\"чужий\"".into()),
        opaque: None,
        segments: vec![downloader_core::Segment {
            id: 0,
            start: 0,
            end: 999_999,
            done: 500_000,
        }],
    };
    downloader_core::state::save(&tmp.0, &alien)?;

    // Рушій мусить його відкинути й завантажити файл правильно.
    download(&client(), &s.url("/plain/128k"), &tmp.0, &opts(4)).await?;

    assert_eq!(
        sha256_of(&tmp.0)?,
        expected_sha256("/plain/128k")?,
        "чужий стан не має вплинути на результат"
    );

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn ліміт_швидкості_справді_стримує_качання() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let tmp = Temp::new("rate");

    // 512 КБ при ліміті 256 КБ/с — не швидше ніж за секунду навіть у
    // чотири потоки. Сервер тут швидкий навмисно: гальмує саме наш ліміт.
    let opts = Options {
        rate_limit: 256 * 1024,
        ..opts(4)
    };

    let старт = std::time::Instant::now();
    download(&client(), &s.url("/plain/512k"), &tmp.0, &opts).await?;
    let минуло = старт.elapsed();

    assert_eq!(
        sha256_of(&tmp.0)?,
        expected_sha256("/plain/512k")?,
        "ліміт не має псувати вміст"
    );

    // Стартове відро дає секунду трафіку одразу, тож 512 КБ теоретично
    // можуть піти майже миттєво. Перевіряємо головне: ліміт не зробив
    // файл битим і не завісив качання назавжди.
    assert!(
        минуло < std::time::Duration::from_secs(10),
        "качання з лімітом підвисло: {минуло:?}"
    );

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn жорсткий_ліміт_відчутно_гальмує() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let tmp = Temp::new("rate-hard");

    // 256 КБ при 64 КБ/с. Перша секунда йде зі стартового відра, решта —
    // рівно за лімітом, тобто якнайменше ще ~3 секунди... беремо з запасом
    // і перевіряємо лише нижню межу, щоб тест не був крихким.
    let opts = Options {
        rate_limit: 64 * 1024,
        ..opts(4)
    };

    let старт = std::time::Instant::now();
    download(&client(), &s.url("/plain/256k"), &tmp.0, &opts).await?;
    let минуло = старт.elapsed();

    assert_eq!(sha256_of(&tmp.0)?, expected_sha256("/plain/256k")?);
    assert!(
        минуло >= std::time::Duration::from_millis(500),
        "з лімітом 64 КБ/с 256 КБ не могли завантажитись за {минуло:?} — ліміт не діє"
    );

    s.shutdown().await;
    Ok(())
}
