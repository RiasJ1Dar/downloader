//! Обмеження браузерних заголовків діє також для запитів після probe.
#![expect(clippy::unwrap_used, reason = "у тестах падіння є діагностикою")]
use downloader_core::protocol::{BrowserRequestContext, Session};
use downloader_proto_http::probe::apply_session;

fn session() -> Session {
    let mut session = Session::from_parts(
        Some("session=secret".into()),
        Some("https://page.test/watch?token=private".into()),
    );
    session.context = Some(BrowserRequestContext {
        version: 1,
        source_url: "https://cdn.test/private/file".into(),
        page_url: Some("https://page.test/watch".into()),
        method: "GET".into(),
        user_agent: Some("Browser-UA/1".into()),
        headers: vec![
            ("Accept-Language".into(), "uk,en;q=0.8".into()),
            ("Origin".into(), "https://page.test".into()),
        ],
    });
    session
}

#[test]
fn браузерний_контекст_доходить_до_мережевого_запиту() {
    let session = session();
    let request = apply_session(
        reqwest::Client::new().get("https://cdn.test/private/file"),
        &session,
        "https://cdn.test/private/file",
    )
    .build()
    .unwrap();
    let headers = request.headers();
    assert_eq!(headers["user-agent"], "Browser-UA/1");
    assert_eq!(headers["accept-language"], "uk,en;q=0.8");
    assert_eq!(headers["origin"], "https://page.test");
    assert_eq!(headers["cookie"], "session=secret");
}

#[test]
fn новий_origin_не_одержує_cookie_чи_origin_попереднього_ресурсу() {
    let session = session();
    for target in [
        "https://other.test/file",
        "http://cdn.test/file",
        "https://cdn.test:444/file",
    ] {
        let request = apply_session(reqwest::Client::new().get(target), &session, target)
            .build()
            .unwrap();
        let headers = request.headers();
        assert!(!headers.contains_key("cookie"));
        assert!(!headers.contains_key("origin"));
        assert_eq!(headers["referer"], "https://page.test/");
        assert_eq!(headers["user-agent"], "Browser-UA/1");
    }
}

#[test]
fn службові_заголовки_не_можна_підмінити_навіть_поза_ipc() {
    let mut session = session();
    session.context.as_mut().unwrap().headers = vec![
        ("Range".into(), "bytes=999-".into()),
        ("Authorization".into(), "secret".into()),
    ];
    let target = "https://cdn.test/private/file";
    let request = apply_session(reqwest::Client::new().get(target), &session, target)
        .build()
        .unwrap();
    assert!(!request.headers().contains_key("range"));
    assert!(!request.headers().contains_key("authorization"));
}
