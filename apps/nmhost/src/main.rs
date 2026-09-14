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

    if std::env::args().any(|a| a == "--install") {
        let exe = std::env::current_exe().context("current_exe")?;
        let path = install::install(&exe)?;
        eprintln!("native host поставлено: {}", path.display());
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
}
