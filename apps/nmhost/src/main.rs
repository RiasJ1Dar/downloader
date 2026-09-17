//! Native messaging host: Chrome/Edge/Firefox → ядро named pipe.
//!
//! Жодного TCP-порту. Кадр як у браузера: 4 байти LE + JSON. Далі той самий
//! IPC, що й у CLI (`Hello` client=`nmhost`, потім `Add`).

mod install;

use anyhow::{Context, Result, bail};
use downloader_ipc::frame::{read_frame, write_frame};
use downloader_ipc::protocol::{PROTOCOL_VERSION, Request, Response};
use downloader_ipc::transport::connect;
use serde::{Deserialize, Serialize};
use tokio::io::{self, AsyncReadExt, AsyncWriteExt, stdin, stdout};

/// Ім'я native host у маніфесті розширення.
pub const HOST_NAME: &str = "com.downloader.host";

#[derive(Debug, Deserialize)]
struct FromExt {
    url: String,
    #[serde(default)]
    cookies: Option<String>,
    #[serde(default)]
    referer: Option<String>,
}

#[derive(Debug, Serialize)]
struct ToExt {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter("warn")
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--install") {
        let exe = std::env::current_exe().context("current_exe")?;
        // Ідентифікатор розширення в Chrome Web Store призначає магазин, і
        // наперед він невідомий. Без цього ключа кожна публікація вимагала б
        // перезбірки програми: ID зашитий у код, а хост відхиляє все, чого в
        // списку немає — з боку людини це виглядає як «розширення не
        // працює», без жодної підказки чому.
        let додаткові: Vec<String> = args
            .windows(2)
            .filter(|w| w[0] == "--allow-extension")
            .map(|w| w[1].clone())
            .collect();
        let path = install::install_with(&exe, &додаткові)?;
        eprintln!("native host поставлено: {}", path.display());
        if !додаткові.is_empty() {
            eprintln!("додатково дозволено розширень: {}", додаткові.len());
        }
        return Ok(());
    }

    let mut stdin = stdin();
    let mut stdout = stdout();
    loop {
        let msg = match читати_з_браузера(&mut stdin).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let відповідь = обробити(msg).await;
        писати_в_браузер(&mut stdout, &відповідь).await?;
    }
}

async fn обробити(msg: FromExt) -> ToExt {
    match додати_в_ядро(msg).await {
        Ok(id) => ToExt {
            ok: true,
            id: Some(id),
            error: None,
        },
        Err(e) => ToExt {
            ok: false,
            id: None,
            error: Some(e.to_string()),
        },
    }
}

async fn додати_в_ядро(msg: FromExt) -> Result<i64> {
    if !(msg.url.starts_with("http://") || msg.url.starts_with("https://")) {
        bail!("розширення передало не http(s) адресу");
    }
    let mut stream = connect().await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!("ядро не запущене — запустіть downloader-core")
        } else {
            anyhow::anyhow!("не вдалося під'єднатись до ядра: {e}")
        }
    })?;
    write_frame(
        &mut stream,
        &Request::Hello {
            client: "nmhost".to_owned(),
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await
    .context("hello")?;
    match read_frame::<_, Response>(&mut stream).await? {
        Response::Hello { .. } => {}
        Response::Error { message, .. } => bail!("ядро відмовило: {message}"),
        other => bail!("не hello: {other:?}"),
    }
    write_frame(
        &mut stream,
        &Request::Add {
            url: msg.url,
            dest: None,
            parts: None,
            cookies: msg.cookies,
            referer: msg.referer,
            variant: None,
            queue: None,
            duration: None,
            rewind: false,
        },
    )
    .await?;
    match read_frame::<_, Response>(&mut stream).await? {
        Response::Added { id } => Ok(id),
        Response::Error { message, .. } => bail!(message),
        other => bail!("несподівана відповідь: {other:?}"),
    }
}

/// Chrome native messaging: u32 LE довжина, потім UTF-8 JSON.
async fn читати_з_браузера(r: &mut (impl AsyncReadExt + Unpin)) -> io::Result<FromExt> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let n = u32::from_le_bytes(len) as usize;
    if n == 0 || n > 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("кадр браузера {n} байтів"),
        ));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await?;
    serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

async fn писати_в_браузер(w: &mut (impl AsyncWriteExt + Unpin), msg: &ToExt) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    let n = u32::try_from(body.len()).context("кадр завеликий")?;
    w.write_all(&n.to_le_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "тест")]
mod tests {
    use super::*;

    #[test]
    fn розбір_повідомлення_розширення() {
        let m: FromExt = serde_json::from_str(
            r#"{"url":"https://cdn.example/a.zip","cookies":"a=b","referer":"https://ex.com/"}"#,
        )
        .unwrap();
        assert_eq!(m.url, "https://cdn.example/a.zip");
        assert_eq!(m.cookies.as_deref(), Some("a=b"));
    }

    #[test]
    fn без_необов_язкових_полів() {
        let m: FromExt = serde_json::from_str(r#"{"url":"https://a"}"#).unwrap();
        assert!(m.cookies.is_none());
        assert!(m.referer.is_none());
    }

    #[tokio::test]
    async fn читання_кадру_браузера_з_префіксом_довжини() {
        let body = br#"{"url":"https://site.com/video.m3u8"}"#;
        let len = (body.len() as u32).to_le_bytes();
        let mut input = Vec::new();
        input.extend_from_slice(&len);
        input.extend_from_slice(body);

        let mut cursor = std::io::Cursor::new(input);
        let msg = читати_з_браузера(&mut cursor).await.unwrap();
        assert_eq!(msg.url, "https://site.com/video.m3u8");
        assert!(msg.cookies.is_none());
    }

    #[tokio::test]
    async fn запис_кадру_браузера_з_префіксом_довжини() {
        let resp = ToExt {
            ok: true,
            id: Some(42),
            error: None,
        };
        let mut out = Vec::new();
        писати_в_браузер(&mut out, &resp).await.unwrap();

        assert!(out.len() >= 4);
        let len = u32::from_le_bytes(out[..4].try_into().unwrap()) as usize;
        assert_eq!(out.len() - 4, len);
        let parsed: serde_json::Value = serde_json::from_slice(&out[4..]).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["id"], 42);
    }

    #[tokio::test]
    async fn відхилення_нульового_або_завеликого_кадру() {
        // Нульовий кадр
        let mut zero_len = std::io::Cursor::new(0u32.to_le_bytes());
        let err = читати_з_браузера(&mut zero_len).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

        // Завеликий кадр (> 1 МБ)
        let mut big_len = std::io::Cursor::new((2 * 1024 * 1024u32).to_le_bytes());
        let err2 = читати_з_браузера(&mut big_len).await.unwrap_err();
        assert_eq!(err2.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn обробка_некоректної_url_повертає_помилку() {
        let msg = FromExt {
            url: "ftp://files.example/a.zip".to_string(),
            cookies: None,
            referer: None,
        };
        let res = обробити(msg).await;
        assert!(!res.ok);
        assert!(res.error.unwrap().contains("не http(s)"));
    }
}
