//! Ворожа відповідь перевіряється до запису будь-якого payload.
#![expect(clippy::unwrap_used, reason = "у тестах падіння є діагностикою")]
use downloader_core::protocol::Cancel;
use downloader_proto_http::{
    download::{DownloadError, Options, download_with_probe},
    headers::{RangeSupport, Validator},
    probe::{Probe, probe},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

struct Server {
    url: String,
    sent: std::sync::Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn server(status: u16, headers: &str, body: &[u8], stall: bool) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/file", listener.local_addr().unwrap());
    let response = format!("HTTP/1.1 {status} Test\r\nConnection: close\r\n{headers}\r\n");
    let body = body.to_vec();
    let sent = std::sync::Arc::new(tokio::sync::Notify::new());
    let notify = sent.clone();
    let task = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let response = response.clone();
            let body = body.clone();
            let notify = notify.clone();
            connections.spawn(async move {
                let mut request = Vec::new(); let mut buf = [0u8; 2048];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = socket.read(&mut buf).await.unwrap();
                    if n == 0 { return; }
                    request.extend_from_slice(&buf[..n]);
                    assert!(request.len() < 16 * 1024);
                }
                if request.starts_with(b"HEAD ") {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 256\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n").await.unwrap();
                } else {
                    socket.write_all(response.as_bytes()).await.unwrap();
                    socket.write_all(&body).await.unwrap();
                    notify.notify_one();
                    if stall { std::future::pending::<()>().await; }
                }
            });
        }
    });
    Server { url, sent, task }
}

struct Temp(std::path::PathBuf);
impl Temp {
    fn new() -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!("dl-integrity-{}-{unique}", std::process::id())))
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        for path in [self.0.clone(), downloader_core::state::state_path(&self.0)] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => eprintln!("не прибрано тестовий файл: {e}"),
            }
        }
    }
}

fn info(url: &str) -> Probe {
    Probe {
        final_url: url.into(),
        size: Some(256),
        resumable: true,
        declared: RangeSupport::Bytes,
        validator: Validator::None,
        disposition_name: None,
    }
}

#[tokio::test]
async fn неправильні_межі_розмір_і_відсутній_content_range_не_пишуться_на_диск() {
    for headers in [
        "Content-Length: 256\r\n",
        "Content-Range: bytes 1-256/257\r\nContent-Length: 256\r\n",
        "Content-Range: bytes 0-254/256\r\nContent-Length: 256\r\n",
        "Content-Range: bytes 0-255/257\r\nContent-Length: 256\r\n",
        "Content-Range: bytes 0-255/256\r\nContent-Length: 255\r\n",
    ] {
        let s = server(206, headers, &[b'X'; 256], false).await;
        let temp = Temp::new();
        let options = Options {
            parts: 1,
            ..Options::default()
        };
        let error = download_with_probe(&reqwest::Client::new(), &info(&s.url), &temp.0, &options)
            .await
            .unwrap_err();
        assert!(
            matches!(error, DownloadError::InvalidRange { .. }),
            "{error}"
        );
        assert!(std::fs::read(&temp.0).unwrap().iter().all(|b| *b == 0));
    }
}

#[tokio::test]
async fn повна_відповідь_замість_першого_сегмента_теж_відхиляється() {
    let s = server(200, "Content-Length: 256\r\n", &[b'X'; 256], false).await;
    let temp = Temp::new();
    let error = download_with_probe(
        &reqwest::Client::new(),
        &info(&s.url),
        &temp.0,
        &Options::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DownloadError::ResourceChanged { .. }));
    assert!(std::fs::read(&temp.0).unwrap().iter().all(|b| *b == 0));
}

#[tokio::test]
async fn підмінений_etag_у_206_відхиляється() {
    let s = server(
        206,
        "Content-Range: bytes 0-255/256\r\nContent-Length: 256\r\nETag: \"new\"\r\n",
        &[b'X'; 256],
        false,
    )
    .await;
    let temp = Temp::new();
    let mut probe = info(&s.url);
    probe.validator = Validator::Strong("\"old\"".into());
    let error = download_with_probe(
        &reqwest::Client::new(),
        &probe,
        &temp.0,
        &Options::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DownloadError::ResourceChanged { .. }));
}

#[tokio::test]
async fn брехлива_range_проба_не_вмикає_сегментацію() {
    let s = server(
        206,
        "Content-Range: bytes 5-5/256\r\nContent-Length: 1\r\n",
        b"X",
        false,
    )
    .await;
    let result = probe(&reqwest::Client::new(), &s.url).await.unwrap();
    assert!(!result.resumable);
    assert_eq!(result.size, Some(256));
}

#[tokio::test]
async fn пауза_перериває_очікування_завислого_тіла() {
    let s = server(
        206,
        "Content-Range: bytes 0-255/256\r\nContent-Length: 256\r\n",
        b"X",
        true,
    )
    .await;
    let temp = Temp::new();
    let cancel = Cancel::new();
    let signal = cancel.clone();
    let options = Options {
        parts: 1,
        cancel: Some(cancel),
        ..Options::default()
    };
    let sent = s.sent.clone();
    tokio::spawn(async move {
        sent.notified().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        signal.cancel();
    });
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        download_with_probe(&reqwest::Client::new(), &info(&s.url), &temp.0, &options),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(out.cancelled);
    assert!(out.bytes <= 1);
}

#[tokio::test]
async fn сервер_з_лімітом_зєднань_докачується_з_меншою_паралельністю() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/file", listener.local_addr().unwrap());
    let active = Arc::new(AtomicUsize::new(0));
    let rejected = Arc::new(AtomicUsize::new(0));
    let failures = rejected.clone();
    let task = tokio::spawn(async move {
        let mut children = tokio::task::JoinSet::new();
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let active = active.clone();
            let failures = failures.clone();
            children.spawn(async move {
                let mut request = Vec::new(); let mut buffer = [0u8; 2048];
                while !request.windows(4).any(|b| b == b"\r\n\r\n") {
                    let n = socket.read(&mut buffer).await.unwrap();
                    if n == 0 { return; } request.extend_from_slice(&buffer[..n]);
                }
                if active.fetch_add(1, Ordering::SeqCst) >= 2 {
                    failures.fetch_add(1, Ordering::SeqCst);
                    active.fetch_sub(1, Ordering::SeqCst);
                    socket.write_all(b"HTTP/1.1 503 Busy\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                    return;
                }
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                let range = request.lines().find_map(|line| line.strip_prefix("range: bytes=")).unwrap();
                let (from, to) = range.trim().split_once('-').unwrap();
                let from: u64 = from.parse().unwrap(); let to: u64 = to.parse().unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let header = format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {from}-{to}/256\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", to - from + 1);
                socket.write_all(header.as_bytes()).await.unwrap();
                socket.write_all(&(from..=to).map(|n| n as u8).collect::<Vec<_>>()).await.unwrap();
                active.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    let s = Server {
        url,
        sent: Arc::new(tokio::sync::Notify::new()),
        task,
    };
    let temp = Temp::new();
    let options = Options {
        parts: 8,
        min_chunk: 16,
        max_retries: 1,
        ..Options::default()
    };
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        download_with_probe(&reqwest::Client::new(), &info(&s.url), &temp.0, &options),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(out.bytes, 256);
    assert!(
        rejected.load(Ordering::SeqCst) > 0,
        "сервер мав відхилити надлишок з'єднань"
    );
    assert_eq!(
        std::fs::read(&temp.0).unwrap(),
        (0..=255).map(|n| n as u8).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn пауза_перериває_довге_очікування_лімітера() {
    let s = server(
        206,
        "Content-Range: bytes 0-255/256\r\nContent-Length: 256\r\n",
        &[b'X'; 256],
        false,
    )
    .await;
    let temp = Temp::new();
    let cancel = Cancel::new();
    let signal = cancel.clone();
    let options = Options {
        parts: 1,
        rate_limit: 1,
        cancel: Some(cancel),
        ..Options::default()
    };
    let sent = s.sent.clone();
    tokio::spawn(async move {
        sent.notified().await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        signal.cancel();
    });
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        download_with_probe(&reqwest::Client::new(), &info(&s.url), &temp.0, &options),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(out.cancelled);
    assert!(out.bytes < 256);
}
