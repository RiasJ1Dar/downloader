//! Наскрізна перевірка: справжнє ядро окремим процесом, справжній канал,
//! справжнє завантаження.
//!
//! Усе, що перевірялось досі, працювало всередині одного процесу. Тут уперше
//! зібрано весь ланцюг так, як він працюватиме в людини: ядро запущене
//! окремо, клієнт під'єднується каналом, завдання доїжджає до диска, а подія
//! про завершення повертається клієнту.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use downloader_ipc::frame::{read_frame, write_frame};
use downloader_ipc::protocol::{Event, PROTOCOL_VERSION, Request, Response};
use downloader_ipc::transport::{ClientStream, connect_to};
use downloader_testserver::{EvilServer, expected_sha256};
use sha2::{Digest, Sha256};

/// Запущене ядро, яке прибирає за собою.
struct Ядро {
    child: Child,
    pipe: String,
    data: PathBuf,
}

impl Ядро {
    fn запустити(tag: &str) -> anyhow::Result<Self> {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);

        let pipe = if cfg!(windows) {
            format!(r"\\.\pipe\downloader-e2e-{tag}-{unique}")
        } else {
            format!("/tmp/downloader-e2e-{tag}-{unique}.sock")
        };

        let data = std::env::temp_dir().join(format!("dl-e2e-{tag}-{unique}"));
        std::fs::create_dir_all(&data)?;

        let child = Command::new(env!("CARGO_BIN_EXE_downloader-core"))
            .arg("--pipe")
            .arg(&pipe)
            .arg("--db")
            .arg(data.join("tasks.db"))
            .arg("--downloads")
            .arg(&data)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        Ok(Self { child, pipe, data })
    }

    /// Дочекатись, поки ядро підніме канал.
    ///
    /// Опитування тут доречне саме тому, що ми чекаємо на **інший процес**:
    /// сигналу від нього ще немає — канал і є той сигнал.
    async fn дочекатись(&self) -> anyhow::Result<ClientStream> {
        for _ in 0..100 {
            if let Ok(s) = connect_to(&self.pipe).await {
                return Ok(s);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::bail!("ядро не підняло канал за п'ять секунд")
    }
}

impl Drop for Ядро {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

/// Привітатись і переконатись, що версії збігаються.
async fn привітатись(stream: &mut ClientStream) -> anyhow::Result<()> {
    write_frame(
        stream,
        &Request::Hello {
            client: "тест".into(),
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await?;

    match read_frame::<_, Response>(stream).await? {
        Response::Hello {
            protocol_version, ..
        } => {
            anyhow::ensure!(protocol_version == PROTOCOL_VERSION, "версії розійшлись");
            Ok(())
        }
        other => anyhow::bail!("очікували рукостискання, отримали {other:?}"),
    }
}

fn sha256_of(path: &PathBuf) -> anyhow::Result<String> {
    let data = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&data);
    Ok(hex::encode(h.finalize()))
}

#[tokio::test]
async fn ядро_качає_файл_на_запит_клієнта() -> anyhow::Result<()> {
    let server = EvilServer::start().await?;
    let ядро = Ядро::запустити("download")?;
    let mut client = ядро.дочекатись().await?;
    привітатись(&mut client).await?;

    // Окреме з'єднання слухає події: у справжньому житті це вікно, яке
    // показує прогрес, поки інший клієнт додає завдання.
    let mut слухач = connect_to(&ядро.pipe).await?;
    привітатись(&mut слухач).await?;
    write_frame(&mut слухач, &Request::Subscribe).await?;
    let _: Response = read_frame(&mut слухач).await?;

    let scenario = "/plain/128k";
    let dest = ядро.data.join("готове.bin");

    write_frame(
        &mut client,
        &Request::Add {
            url: server.url(scenario),
            dest: Some(dest.display().to_string()),
            parts: Some(4),
            cookies: None,
            referer: None,
        },
    )
    .await?;

    let id = match read_frame::<_, Response>(&mut client).await? {
        Response::Added { id } => id,
        other => anyhow::bail!("завдання не прийнято: {other:?}"),
    };

    // Чекаємо саме подію про завершення, а не опитуємо список: клієнт не
    // мусить крутити цикл, щоб дізнатись очевидне.
    let finished = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match read_frame::<_, Event>(&mut слухач).await {
                Ok(Event::Finished { id: done, bytes, .. }) if done == id => return Ok(bytes),
                Ok(Event::Failed { message, .. }) => {
                    anyhow::bail!("завдання впало: {message}")
                }
                Ok(_) => {}
                Err(e) => anyhow::bail!("потік подій обірвався: {e}"),
            }
        }
    })
    .await??;

    assert_eq!(finished, 128 * 1024);
    assert_eq!(
        sha256_of(&dest)?,
        expected_sha256(scenario)?,
        "ядро зібрало файл неправильно"
    );

    // Файл, завантажений із мережі, мусить нести мітку — інакше SmartScreen
    // не попередить людину.
    assert!(
        downloader_winutil::is_marked_internet(&dest),
        "ядро не позначило файл як отриманий з мережі"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn ядро_качає_hls_vod() -> anyhow::Result<()> {
    let server = EvilServer::start().await?;
    let ядро = Ядро::запустити("hls")?;
    let mut client = ядро.дочекатись().await?;
    привітатись(&mut client).await?;

    let mut слухач = connect_to(&ядро.pipe).await?;
    привітатись(&mut слухач).await?;
    write_frame(&mut слухач, &Request::Subscribe).await?;
    let _: Response = read_frame(&mut слухач).await?;

    let dest = ядро.data.join("stream.ts");
    write_frame(
        &mut client,
        &Request::Add {
            url: server.url("/hls/vod/media.m3u8"),
            dest: Some(dest.display().to_string()),
            parts: None,
            cookies: None,
            referer: None,
        },
    )
    .await?;

    let id = match read_frame::<_, Response>(&mut client).await? {
        Response::Added { id } => id,
        other => anyhow::bail!("HLS завдання не прийнято: {other:?}"),
    };

    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match read_frame::<_, Event>(&mut слухач).await {
                Ok(Event::Finished { id: done, .. }) if done == id => return Ok(()),
                Ok(Event::Failed { message, .. }) => {
                    anyhow::bail!("HLS завдання впало: {message}")
                }
                Ok(_) => {}
                Err(e) => anyhow::bail!("потік подій обірвався: {e}"),
            }
        }
    })
    .await??;

    let got = std::fs::read(&dest)?;
    let mut expect = Vec::new();
    expect.extend_from_slice(b"SEG0-PAYLOAD-AAAAAAAAAAAAAAAA");
    expect.extend_from_slice(b"SEG1-PAYLOAD-BBBBBBBBBBBBBBBB");
    assert_eq!(got, expect, "ядро мало склеїти HLS VOD");

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn клієнт_бачить_завдання_у_списку() -> anyhow::Result<()> {
    let server = EvilServer::start().await?;
    let ядро = Ядро::запустити("list")?;
    let mut client = ядро.дочекатись().await?;
    привітатись(&mut client).await?;

    write_frame(
        &mut client,
        &Request::Add {
            url: server.url("/slow-range/1m/256k"),
            dest: Some(ядро.data.join("повільне.bin").display().to_string()),
            parts: Some(2),
            cookies: None,
            referer: None,
        },
    )
    .await?;
    let _: Response = read_frame(&mut client).await?;

    write_frame(&mut client, &Request::List).await?;
    match read_frame::<_, Response>(&mut client).await? {
        Response::Tasks { tasks } => {
            assert_eq!(tasks.len(), 1, "завдання не потрапило в список");
            assert_eq!(tasks[0].name, "повільне.bin");
            assert_eq!(tasks[0].total, Some(1024 * 1024));
        }
        other => anyhow::bail!("несподівана відповідь: {other:?}"),
    }

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn без_рукостискання_ядро_не_розмовляє() -> anyhow::Result<()> {
    let ядро = Ядро::запустити("handshake")?;
    let mut client = ядро.дочекатись().await?;

    // Одразу запит, без `hello`.
    write_frame(&mut client, &Request::List).await?;

    match read_frame::<_, Response>(&mut client).await? {
        Response::Error { code, message } => {
            assert_eq!(code, downloader_ipc::protocol::ErrorCode::HandshakeRequired);
            assert!(message.contains("hello"), "{message}");
        }
        other => anyhow::bail!("ядро відповіло без рукостискання: {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn чужа_версія_протоколу_відхиляється_зрозуміло() -> anyhow::Result<()> {
    let ядро = Ядро::запустити("version")?;
    let mut client = ядро.дочекатись().await?;

    write_frame(
        &mut client,
        &Request::Hello {
            client: "старий".into(),
            protocol_version: PROTOCOL_VERSION + 100,
        },
    )
    .await?;

    match read_frame::<_, Response>(&mut client).await? {
        Response::Error { code, message } => {
            assert_eq!(code, downloader_ipc::protocol::ErrorCode::VersionMismatch);
            // Повідомлення має пояснити людині, що робити, а не лише
            // констатувати розбіжність.
            assert!(message.contains("оновіть"), "{message}");
        }
        other => anyhow::bail!("несумісний клієнт не відхилено: {other:?}"),
    }

    Ok(())
}
