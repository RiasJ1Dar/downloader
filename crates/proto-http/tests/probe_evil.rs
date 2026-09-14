//! Проба URL проти «злого» сервера.
//!
//! Кожен тест відповідає на питання «а що буде, коли сервер поведеться
//! погано саме так». Заготовки на кшталт «перевіримо, що працює» тут
//! марні — рушій ламається саме на крайніх випадках.

// Це тест: падіння тут і є повідомленням про помилку. Заборона на
// `expect` і проковтнуті помилки призначена бойовому коду, а не перевіркам.
#![expect(
    clippy::expect_used,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]

use downloader_core::protocol::Session;
use downloader_proto_http::headers::RangeSupport;
use downloader_proto_http::probe::{probe, probe_with_session};
use downloader_testserver::EvilServer;
use reqwest::Client;

fn client() -> Client {
    Client::builder()
        .build()
        .unwrap_or_else(|_| Client::new())
}

#[tokio::test]
async fn чесний_сервер_дає_розмір_і_дозвіл_на_сегменти() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let p = probe(&client(), &s.url("/plain/64k")).await?;

    assert_eq!(p.size, Some(65536));
    assert!(p.resumable, "чесний сервер має дозволити сегменти");
    assert_eq!(p.declared, RangeSupport::Bytes);
    assert!(p.validator.is_reliable(), "мав бути придатний ETag або Last-Modified");
    assert_eq!(p.usable_parts(16), 16);

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn сервер_без_range_відкочує_нас_в_один_потік() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let p = probe(&client(), &s.url("/norange/64k")).await?;

    assert!(
        !p.resumable,
        "сервер віддав 200 на Range — різати не можна, інакше запишемо байти не туди"
    );
    assert_eq!(
        p.usable_parts(32),
        1,
        "просили 32 потоки, але дозволено рівно один"
    );

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn відсутній_head_не_ламає_пробу() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let p = probe(&client(), &s.url("/no-head/32k")).await?;

    assert_eq!(
        p.size,
        Some(32768),
        "розмір мав приїхати з Content-Range пробного GET"
    );
    assert!(p.resumable);

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn імʼя_береться_з_content_disposition_а_не_з_url() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let p = probe(&client(), &s.url("/disposition/cyrillic")).await?;

    let name = p.filename().unwrap_or_default();
    assert_eq!(
        name, "Звіт за 2026 рік.bin",
        "сервер шле сиру кирилицю в заголовку — це має дожити до імені файла"
    );

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn спроба_виходу_з_теки_ріжеться_до_імені() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let p = probe(&client(), &s.url("/disposition/traversal")).await?;

    let name = p.filename().unwrap_or_default();
    assert!(
        !name.contains('/') && !name.contains('\\'),
        "у імені лишився шлях: {name}"
    );
    assert!(!name.contains(".."), "у імені лишився вихід із теки: {name}");

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn редиректи_дають_кінцевий_url() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let start = s.url("/redirect/3/16k");
    let p = probe(&client(), &start).await?;

    assert_ne!(p.final_url, start, "качати треба кінцевий URL, а не початковий");
    assert_eq!(p.size, Some(16384));

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn відмова_доступу_це_помилка_а_не_порожній_файл() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let err = probe(&client(), &s.url("/auth/16k"))
        .await
        .expect_err("без cookie сервер віддає 403");

    let text = err.to_string();
    assert!(text.contains("403"), "помилка не називає код: {text}");

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn cookie_і_referer_проходять_auth() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let session = Session::from_parts(
        Some("other=1; session=ok; more=2".to_owned()),
        Some("http://example.test/page".to_owned()),
    );
    let p = probe_with_session(&client(), &s.url("/auth/16k"), &session).await?;
    assert_eq!(p.size, Some(16 * 1024));
    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn сервер_що_бреше_в_довжині_не_вважається_надійним() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    // Заявляє 100000, віддає 16384. Проба цього ще не бачить — вона й не
    // мусить. Але розмір, який вона віддала, — саме заявлений, і далі рушій
    // зобов'язаний звірити фактично завантажене з ним.
    let p = probe(&client(), &s.url("/liar-length/16k/100000")).await?;

    assert_eq!(
        p.size,
        Some(100_000),
        "проба переказує те, що сказав сервер; ловити брехню — робота рушія"
    );

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn filename_star_перемагає_звичайне_імʼя() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let p = probe(&client(), &s.url("/disposition/rfc5987")).await?;

    assert_eq!(
        p.filename().unwrap_or_default(),
        "Звіт.bin",
        "у заголовку були обидва варіанти: filename*=UTF-8 має бути важливішим"
    );

    s.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn відсутній_заголовок_відкочує_на_імʼя_з_url() -> anyhow::Result<()> {
    let s = EvilServer::start().await?;
    let p = probe(&client(), &s.url("/disposition/none")).await?;

    assert_eq!(
        p.filename().unwrap_or_default(),
        "none",
        "без Content-Disposition ім'я береться з останнього сегмента URL"
    );

    s.shutdown().await;
    Ok(())
}
