//! Self-тести злого сервера.
//!
//! Стенду, якому не можна вірити, гріш ціна: тут доводиться, що `/cut`
//! справді рве з'єднання, `/changing` справді міняє `ETag`, а `/liar-length`
//! справді бреше. Клієнт навмисно сирий (голий TCP): будь-яка HTTP-бібліотека
//! або сховала б обрив за своєю помилкою, або відмовилась би читати відповідь,
//! у якій `Content-Length` не сходиться з тілом.

use anyhow::{Context, Result, bail};
use downloader_testserver::{EvilServer, body_bytes, expected_sha256, expected_sha256_of_size};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use std::io::Read as _;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// ── Сирий клієнт ────────────────────────────────────────────────────────────

#[derive(Debug)]
struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == lower)
            .map(|(_, v)| v.as_str())
    }

    fn require(&self, name: &str) -> Result<&str> {
        self.header(name)
            .with_context(|| format!("у відповіді немає заголовка {name}: {:?}", self.headers))
    }

    /// Заявлена довжина тіла — не обов'язково справжня.
    fn claimed_len(&self) -> Option<u64> {
        self.header("content-length")?.parse().ok()
    }

    fn sha256(&self) -> String {
        let mut h = Sha256::new();
        h.update(&self.body);
        hex::encode(h.finalize())
    }
}

/// Один сирий запит. Читає до EOF — сервер завжди закриває з'єднання,
/// тож саме EOF, а не `Content-Length`, каже, де насправді кінець тіла.
async fn req(addr: SocketAddr, method: &str, target: &str, extra: &[(&str, &str)]) -> Result<Resp> {
    let mut stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("не під'єднатись до {addr}"))?;

    let mut head = format!("{method} {target} HTTP/1.1\r\nHost: {addr}\r\n");
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .await
        .context("надсилання запиту")?;
    stream.flush().await.context("flush запиту")?;

    let mut raw = Vec::new();
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            // Обрив — теж результат: віддаємо те, що встигли прочитати.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(e) => return Err(e).context("читання відповіді"),
        }
    }

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("у відповіді немає порожнього рядка після заголовків")?;
    let head_text = String::from_utf8_lossy(&raw[..split]).into_owned();
    let body = raw[split + 4..].to_vec();

    let mut lines = head_text.split("\r\n");
    let status_line = lines.next().context("порожня відповідь")?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .context("немає коду статусу")?
        .parse()
        .context("код статусу не число")?;

    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    Ok(Resp {
        status,
        headers,
        body,
    })
}

async fn get(addr: SocketAddr, target: &str) -> Result<Resp> {
    req(addr, "GET", target, &[]).await
}

// ── 1. /plain — еталон ──────────────────────────────────────────────────────

#[tokio::test]
async fn plain_віддає_ціле_тіло_і_206_на_range() -> Result<()> {
    let s = EvilServer::start().await?;
    let a = s.addr();

    let full = get(a, "/plain/4k").await?;
    assert_eq!(full.status, 200);
    assert_eq!(full.require("accept-ranges")?, "bytes");
    assert_eq!(full.body.len(), 4096);
    assert_eq!(full.claimed_len(), Some(4096));
    assert_eq!(full.sha256(), expected_sha256("/plain/4k")?);
    let etag = full.require("etag")?.to_string();
    full.require("last-modified")?;

    // ETag стабільний — саме цим /plain відрізняється від /changing.
    let again = get(a, "/plain/4k").await?;
    assert_eq!(
        again.require("etag")?,
        etag,
        "ETag чесного сервера не змінюється"
    );

    let part = req(a, "GET", "/plain/4k", &[("Range", "bytes=1000-1099")]).await?;
    assert_eq!(part.status, 206);
    assert_eq!(part.require("content-range")?, "bytes 1000-1099/4096");
    assert_eq!(part.body.len(), 100);
    assert_eq!(
        part.body,
        &body_bytes(4096)[1000..1100],
        "байти 206 мусять збігатися з тими самими байтами у 200"
    );

    let suffix = req(a, "GET", "/plain/4k", &[("Range", "bytes=-10")]).await?;
    assert_eq!(suffix.status, 206);
    assert_eq!(suffix.require("content-range")?, "bytes 4086-4095/4096");

    let bad = req(a, "GET", "/plain/4k", &[("Range", "bytes=99999-")]).await?;
    assert_eq!(bad.status, 416, "діапазон поза межами → 416");
    assert_eq!(bad.require("content-range")?, "bytes */4096");

    s.shutdown().await;
    Ok(())
}

// ── 2. /norange — ігнорує Range ─────────────────────────────────────────────

#[tokio::test]
async fn norange_ігнорує_range_і_завжди_віддає_все() -> Result<()> {
    let s = EvilServer::start().await?;
    let a = s.addr();

    let r = req(
        a,
        "GET",
        "/norange/2k/payload.bin",
        &[("Range", "bytes=100-199")],
    )
    .await?;
    assert_eq!(
        r.status, 200,
        "Range має бути проігнорований, а не задоволений"
    );
    assert_eq!(r.require("accept-ranges")?, "none");
    assert!(r.header("content-range").is_none());
    assert_eq!(r.body.len(), 2048);
    assert_eq!(r.sha256(), expected_sha256("/norange/2k/payload.bin")?);

    s.shutdown().await;
    Ok(())
}

// ── 3. /cut — обрив посеред тіла ────────────────────────────────────────────

#[tokio::test]
async fn cut_рве_зʼєднання_і_повторює_це_щоразу() -> Result<()> {
    let s = EvilServer::start().await?;
    let a = s.addr();

    for sproba in 1..=2 {
        let r = get(a, "/cut/8k/1000").await?;
        assert_eq!(r.status, 200);
        assert_eq!(
            r.claimed_len(),
            Some(8192),
            "спроба {sproba}: сервер обіцяв повний розмір"
        );
        assert_eq!(
            r.body.len(),
            1000,
            "спроба {sproba}: сервер мав обірватись рівно на 1000 байтах"
        );
        assert_eq!(
            r.body,
            &body_bytes(8192)[..1000],
            "спроба {sproba}: віддані байти — це початок справжнього тіла"
        );
    }

    s.shutdown().await;
    Ok(())
}

// ── 4. /flaky — 503 із Retry-After ──────────────────────────────────────────

#[tokio::test]
async fn flaky_падає_перші_n_разів() -> Result<()> {
    let s = EvilServer::start().await?;
    let a = s.addr();

    for i in 1..=2 {
        let r = get(a, "/flaky/1k/2").await?;
        assert_eq!(r.status, 503, "запит {i} мав упасти");
        assert_eq!(r.require("retry-after")?, "1");
    }

    let ok = get(a, "/flaky/1k/2").await?;
    assert_eq!(ok.status, 200, "третій запит уже мав пройти");
    assert_eq!(ok.body.len(), 1024);
    assert_eq!(ok.sha256(), expected_sha256("/flaky/1k/2")?);

    // Лічильник — на кожен шлях свій.
    let inshyj = get(a, "/flaky/1k/1").await?;
    assert_eq!(inshyj.status, 503, "інший шлях має власний лічильник");

    s.shutdown().await;
    Ok(())
}

// ── 5. /changing — ETag міняється ───────────────────────────────────────────

#[tokio::test]
async fn changing_міняє_etag_і_відмовляє_if_range() -> Result<()> {
    let s = EvilServer::start().await?;
    let a = s.addr();

    let first = get(a, "/changing/4k").await?;
    let etag1 = first.require("etag")?.to_string();
    let second = get(a, "/changing/4k").await?;
    let etag2 = second.require("etag")?.to_string();
    assert_ne!(etag1, etag2, "ETag зобов'язаний змінитись між запитами");

    // Докачка з If-Range на старий ETag: сервер мусить віддати 200 і все тіло,
    // а не 206. Рушій має з цього зрозуміти, що склеювати нічого не можна.
    let resume = req(
        a,
        "GET",
        "/changing/4k",
        &[("Range", "bytes=2000-"), ("If-Range", etag1.as_str())],
    )
    .await?;
    assert_eq!(resume.status, 200, "застарілий If-Range → 200, а не 206");
    assert!(resume.header("content-range").is_none());
    assert_eq!(resume.body.len(), 4096, "прийшло все тіло з початку");
    assert_eq!(resume.sha256(), expected_sha256("/changing/4k")?);

    // Без If-Range сервер усе ще вміє 206 — інакше тест вище нічого не доводив би.
    let partial = req(a, "GET", "/changing/4k", &[("Range", "bytes=2000-2099")]).await?;
    assert_eq!(partial.status, 206);
    assert_eq!(partial.require("content-range")?, "bytes 2000-2099/4096");

    s.shutdown().await;
    Ok(())
}

// ── 6. /liar-length — брехливий Content-Length ──────────────────────────────

#[tokio::test]
async fn liar_length_бреше_в_обидва_боки() -> Result<()> {
    let s = EvilServer::start().await?;
    let a = s.addr();

    // Заявив більше, ніж дав.
    let more = get(a, "/liar-length/1k/4k").await?;
    assert_eq!(more.claimed_len(), Some(4096));
    assert_eq!(more.body.len(), 1024, "реально прийшов 1 КіБ");
    assert_eq!(more.sha256(), expected_sha256("/liar-length/1k/4k")?);

    // Заявив менше, ніж дав.
    let less = get(a, "/liar-length/4k/1k").await?;
    assert_eq!(less.claimed_len(), Some(1024));
    assert_eq!(less.body.len(), 4096, "реально прийшло 4 КіБ");
    assert_eq!(less.sha256(), expected_sha256("/liar-length/4k/1k")?);

    s.shutdown().await;
    Ok(())
}

// ── 7. /gzip ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn gzip_content_length_про_стиснуте_а_на_диск_розпаковане() -> Result<()> {
    let s = EvilServer::start().await?;
    let a = s.addr();

    let r = get(a, "/gzip/16k").await?;
    assert_eq!(r.status, 200);
    assert_eq!(r.require("content-encoding")?, "gzip");
    assert_eq!(
        r.claimed_len(),
        Some(r.body.len() as u64),
        "Content-Length описує стиснене тіло — і збігається з ним"
    );
    // Тіло сценаріїв — псевдовипадкове, тобто нестисне: gzip на ньому не
    // економить, а додає свої ~20 байтів службових. Пастка від цього нікуди
    // не дівається — важливо саме те, що заявлена довжина НЕ дорівнює тому,
    // що ляже на диск.
    assert_ne!(
        r.body.len() as u64,
        16 * 1024,
        "Content-Length мусить розходитись із розміром розпакованого тіла"
    );

    let mut rozpakovane = Vec::new();
    GzDecoder::new(&r.body[..])
        .read_to_end(&mut rozpakovane)
        .context("розпакування gzip")?;
    assert_eq!(rozpakovane.len(), 16 * 1024);
    assert_eq!(rozpakovane, body_bytes(16 * 1024));

    let mut h = Sha256::new();
    h.update(&rozpakovane);
    assert_eq!(
        hex::encode(h.finalize()),
        expected_sha256("/gzip/16k")?,
        "expected_sha256 для /gzip — це хеш РОЗПАКОВАНОГО"
    );

    s.shutdown().await;
    Ok(())
}

// ── 8. /slow ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn slow_не_швидше_заданого() -> Result<()> {
    let s = EvilServer::start().await?;
    let a = s.addr();

    let start = std::time::Instant::now();
    let r = get(a, "/slow/8192/8192").await?;
    let tryvalist = start.elapsed();

    assert_eq!(r.status, 200);
    assert_eq!(r.body.len(), 8192);
    assert_eq!(r.sha256(), expected_sha256_of_size(8192));
    assert!(
        tryvalist >= Duration::from_millis(700),
        "8 КіБ на 8192 Б/с мали тягтись близько секунди, а зайняли {tryvalist:?}"
    );

    s.shutdown().await;
    Ok(())
}

// ── 9. /redirect ────────────────────────────────────────────────────────────

#[tokio::test]
async fn redirect_веде_ланцюгом_і_закінчується_файлом() -> Result<()> {
    let s = EvilServer::start().await?;
    let a = s.addr();

    let mut target = "/redirect/3/2k".to_string();
    let mut krokiv = 0;
    let tilo = loop {
        let r = get(a, &target).await?;
        if r.status == 302 {
            krokiv += 1;
            if krokiv > 10 {
                bail!("ланцюг редиректів не закінчується");
            }
            let loc = r.require("location")?.to_string();
            // Location має бути абсолютним, бо клієнт прислав Host.
            assert!(
                loc.starts_with(&format!("http://{a}/")),
                "очікувався абсолютний Location на цей самий сервер, а прийшов {loc:?}"
            );
            target = loc
                .strip_prefix(&format!("http://{a}"))
                .context("Location не на цьому сервері")?
                .to_string();
            continue;
        }
        assert_eq!(r.status, 200);
        break r;
    };

    assert_eq!(krokiv, 3, "мало бути рівно 3 редиректи");
    assert_eq!(
        target, "/plain/2k",
        "останній крок мусить вести на інший шлях того самого сервера"
    );
    assert_eq!(tilo.body.len(), 2048);
    assert_eq!(tilo.sha256(), expected_sha256("/redirect/3/2k")?);

    s.shutdown().await;
    Ok(())
}

// ── 10. /auth ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn auth_вимагає_cookie_і_referer() -> Result<()> {
    let s = EvilServer::start().await?;
    let a = s.addr();

    assert_eq!(get(a, "/auth/1k").await?.status, 403, "без нічого — 403");

    let lyshe_cookie = req(a, "GET", "/auth/1k", &[("Cookie", "session=ok")]).await?;
    assert_eq!(lyshe_cookie.status, 403, "без Referer — теж 403");

    let lyshe_referer = req(a, "GET", "/auth/1k", &[("Referer", "http://example.test/")]).await?;
    assert_eq!(lyshe_referer.status, 403, "без Cookie — теж 403");

    let chuzha_cookie = req(
        a,
        "GET",
        "/auth/1k",
        &[
            ("Cookie", "session=bad"),
            ("Referer", "http://example.test/"),
        ],
    )
    .await?;
    assert_eq!(chuzha_cookie.status, 403, "чужа сесія — 403");

    let ok = req(
        a,
        "GET",
        "/auth/1k",
        &[
            ("Cookie", "other=1; session=ok; more=2"),
            ("Referer", "http://example.test/page"),
        ],
    )
    .await?;
    assert_eq!(ok.status, 200);
    assert_eq!(ok.body.len(), 1024);
    assert_eq!(ok.sha256(), expected_sha256("/auth/1k")?);

    s.shutdown().await;
    Ok(())
}

// ── 11. /disposition ────────────────────────────────────────────────────────

#[tokio::test]
async fn disposition_підсовує_небезпечні_імена() -> Result<()> {
    let s = EvilServer::start().await?;
    let a = s.addr();

    let ochikuvannja: &[(&str, &str)] = &[
        ("cyrillic", "Звіт за 2026 рік.bin"),
        ("spaces", "my report file.bin"),
        ("reserved", "CON.txt"),
        ("trailing-dot", "report.bin."),
        ("traversal", "..\\..\\evil.exe"),
        ("rfc5987", "filename*=UTF-8''%D0%97%D0%B2%D1%96%D1%82.bin"),
    ];

    for (case, pidrjadok) in ochikuvannja {
        let r = get(a, &format!("/disposition/{case}")).await?;
        assert_eq!(r.status, 200, "кейс {case}");
        let cd = r.require("content-disposition")?;
        assert!(
            cd.contains(pidrjadok),
            "кейс {case}: у Content-Disposition {cd:?} немає {pidrjadok:?}"
        );
        assert_eq!(r.body.len(), 64, "кейс {case}: тіло маленьке");
        assert_eq!(r.sha256(), expected_sha256("/disposition/cyrillic")?);
    }

    let dovge = get(a, "/disposition/long").await?;
    assert!(
        dovge.require("content-disposition")?.len() > 300,
        "кейс long мав дати дуже довге ім'я"
    );

    let bez = get(a, "/disposition/none").await?;
    assert_eq!(bez.status, 200);
    assert!(
        bez.header("content-disposition").is_none(),
        "кейс none — заголовка не має бути взагалі"
    );

    let nevidomyj = get(a, "/disposition/nema-takogo").await?;
    assert_eq!(nevidomyj.status, 404, "невідомий кейс має падати гучно");

    s.shutdown().await;
    Ok(())
}

// ── 12. /head-only і /no-head ───────────────────────────────────────────────

#[tokio::test]
async fn head_only_відповідає_на_head() -> Result<()> {
    let s = EvilServer::start().await?;
    let a = s.addr();

    let h = req(a, "HEAD", "/head-only/4k", &[]).await?;
    assert_eq!(h.status, 200);
    assert_eq!(h.claimed_len(), Some(4096));
    assert_eq!(h.require("accept-ranges")?, "bytes");
    assert!(h.body.is_empty(), "на HEAD тіла бути не має");

    let g = get(a, "/head-only/4k").await?;
    assert_eq!(g.status, 200);
    assert_eq!(g.body.len(), 4096);
    assert_eq!(g.sha256(), expected_sha256("/head-only/4k")?);

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn no_head_змушує_пробувати_get_range() -> Result<()> {
    let s = EvilServer::start().await?;
    let a = s.addr();

    let h = req(a, "HEAD", "/no-head/4k", &[]).await?;
    assert_eq!(h.status, 405, "HEAD тут заборонений");
    assert_eq!(h.require("allow")?, "GET");

    // Обхідний шлях рушія: один байт через Range, щоб дізнатись розмір.
    let probe = req(a, "GET", "/no-head/4k", &[("Range", "bytes=0-0")]).await?;
    assert_eq!(probe.status, 206);
    assert_eq!(probe.require("content-range")?, "bytes 0-0/4096");
    assert_eq!(probe.body.len(), 1);

    let g = get(a, "/no-head/4k").await?;
    assert_eq!(g.body.len(), 4096);
    assert_eq!(g.sha256(), expected_sha256("/no-head/4k")?);

    s.shutdown().await;
    Ok(())
}

// ── Загальні властивості стенду ─────────────────────────────────────────────

#[tokio::test]
async fn кілька_серверів_живуть_паралельно() -> Result<()> {
    let a = EvilServer::start().await?;
    let b = EvilServer::start().await?;
    assert_ne!(a.port(), b.port(), "кожен інстанс бере власний порт");

    // Лічильники теж окремі: обидва сервери «падають» перший раз незалежно.
    assert_eq!(get(a.addr(), "/flaky/1k/1").await?.status, 503);
    assert_eq!(get(b.addr(), "/flaky/1k/1").await?.status, 503);
    assert_eq!(get(a.addr(), "/flaky/1k/1").await?.status, 200);
    assert_eq!(get(b.addr(), "/flaky/1k/1").await?.status, 200);

    a.shutdown().await;
    b.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn url_та_невідомий_сценарій() -> Result<()> {
    let s = EvilServer::start().await?;
    let url = s.url("/plain/1k");
    assert_eq!(url, format!("http://{}/plain/1k", s.addr()));
    assert_eq!(s.url("plain/1k"), url, "провідний / необов'язковий");

    let r = get(s.addr(), "/takogo-nema/1k").await?;
    assert_eq!(r.status, 404);
    let tekst = String::from_utf8_lossy(&r.body);
    assert!(
        tekst.contains("невідомий сценарій")
            && tekst.contains("/plain")
            && tekst.contains("/hls/media")
            && tekst.contains("/hls/drm"),
        "404 має гучно перелічити доступні сценарії, а не мовчати: {tekst}"
    );

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn після_shutdown_порт_не_відповідає() -> Result<()> {
    let s = EvilServer::start().await?;
    let a = s.addr();
    assert_eq!(get(a, "/plain/1k").await?.status, 200);
    s.shutdown().await;

    // Даємо ядру мить на закриття слухача.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        get(a, "/plain/1k").await.is_err(),
        "після shutdown сервер не має відповідати"
    );
    Ok(())
}

#[tokio::test]
async fn великий_файл_та_докачка_складаються_в_оригінал() -> Result<()> {
    let s = EvilServer::start().await?;
    let a = s.addr();

    // Дві половини через Range мають дати той самий SHA-256, що й цілий файл.
    let persha = req(a, "GET", "/plain/1m", &[("Range", "bytes=0-524287")]).await?;
    let druha = req(a, "GET", "/plain/1m", &[("Range", "bytes=524288-")]).await?;
    assert_eq!(persha.status, 206);
    assert_eq!(druha.status, 206);

    let mut sklejene = persha.body.clone();
    sklejene.extend_from_slice(&druha.body);
    assert_eq!(sklejene.len(), 1024 * 1024);

    let mut h = Sha256::new();
    h.update(&sklejene);
    assert_eq!(
        hex::encode(h.finalize()),
        expected_sha256("/plain/1m")?,
        "склеєні діапазони мусять дати той самий файл, що й одним потоком"
    );

    s.shutdown().await;
    Ok(())
}

// ── HLS: окремі AUDIO/SUBTITLES і FairPlay SAMPLE-AES ───────────────────────

#[tokio::test]
async fn hls_media_master_віддає_audio_і_subtitles() -> Result<()> {
    let s = EvilServer::start().await?;
    let a = s.addr();

    let master = get(a, "/hls/media/master.m3u8").await?;
    assert_eq!(master.status, 200);
    let tekst = String::from_utf8_lossy(&master.body);
    assert!(
        tekst.contains("#EXT-X-MEDIA:TYPE=AUDIO") && tekst.contains("audio.m3u8"),
        "master має оголосити AUDIO: {tekst}"
    );
    assert!(
        tekst.contains("#EXT-X-MEDIA:TYPE=SUBTITLES") && tekst.contains("subs.vtt"),
        "master має оголосити SUBTITLES: {tekst}"
    );

    let audio = get(a, "/hls/media/audio.m3u8").await?;
    assert_eq!(audio.status, 200);
    assert!(
        String::from_utf8_lossy(&audio.body).contains("audio.ts"),
        "аудіо-плейлист має вказувати на audio.ts"
    );
    let audio_seg = get(a, "/hls/media/audio.ts").await?;
    assert_eq!(audio_seg.status, 200);
    assert_eq!(audio_seg.body, b"AUDIO-TRACK-PAYLOAD");

    let subs = get(a, "/hls/media/subs.vtt").await?;
    assert_eq!(subs.status, 200);
    assert!(
        String::from_utf8_lossy(&subs.body).contains("WEBVTT"),
        "субтитри мають бути WebVTT"
    );

    let nevidomyj = get(a, "/hls/media/nope.bin").await?;
    assert_eq!(nevidomyj.status, 404);
    assert!(
        String::from_utf8_lossy(&nevidomyj.body).contains("невідомий HLS media файл"),
        "невідомий файл — 404 з текстом як у інших hls_*"
    );

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn hls_drm_містить_keyformat_і_sample_aes() -> Result<()> {
    let s = EvilServer::start().await?;
    let a = s.addr();

    let r = get(a, "/hls/drm/media.m3u8").await?;
    assert_eq!(r.status, 200);
    let tekst = String::from_utf8_lossy(&r.body);
    assert!(
        tekst.contains("SAMPLE-AES") && tekst.contains("KEYFORMAT"),
        "drm-плейлист має містити SAMPLE-AES і KEYFORMAT: {tekst}"
    );
    assert!(
        tekst.contains("com.apple.streamingkeydelivery"),
        "KEYFORMAT має бути FairPlay: {tekst}"
    );
    assert!(
        tekst.contains("skd://fairplay"),
        "ключ — skd://, не AES-файл: {tekst}"
    );

    let key = get(a, "/hls/drm/key.bin").await?;
    assert_eq!(key.status, 404, "ключ файлом не віддаємо");
    assert!(
        String::from_utf8_lossy(&key.body).contains("невідомий HLS drm файл"),
        "невідомий файл — 404 з текстом як у інших hls_*"
    );

    s.shutdown().await;
    Ok(())
}
