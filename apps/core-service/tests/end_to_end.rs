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
    child: Option<Child>,
    pipe: String,
    data: PathBuf,
    прибрати_теку: bool,
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

        Ok(Self {
            child: Some(child),
            pipe,
            data,
            прибрати_теку: true,
        })
    }

    /// Вбити процес, лишивши теку з базою — для перевірки персисту.
    fn зупинити(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
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
        self.зупинити();
        if self.прибрати_теку {
            let _ = std::fs::remove_dir_all(&self.data);
        }
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
            variant: None,
            queue: None,
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
            variant: None,
            queue: None,
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
            variant: None,
            queue: None,
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
async fn звичайний_файл_не_має_чого_вибирати() -> anyhow::Result<()> {
    // ⚠️ Порожній перелік — це **відповідь**, а не помилка: звичайний файл
    // має один вигляд, і вибирати в ньому нема чого. Якби ядро відповідало
    // помилкою, вікно показувало б червоне там, де все гаразд.
    let server = EvilServer::start().await?;
    let ядро = Ядро::запустити("variants")?;
    let mut client = ядро.дочекатись().await?;
    привітатись(&mut client).await?;

    write_frame(
        &mut client,
        &Request::Variants {
            url: server.url("/plain/64k"),
        },
    )
    .await?;

    match read_frame::<_, Response>(&mut client).await? {
        Response::Variants { variants } => {
            assert!(
                variants.is_empty(),
                "у звичайного файла не буває варіантів якості: {variants:?}"
            );
        }
        other => anyhow::bail!("несподівана відповідь: {other:?}"),
    }

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn варіанти_невідомого_посилання_дають_зрозумілу_відмову() -> anyhow::Result<()> {
    let ядро = Ядро::запустити("variants-unknown")?;
    let mut client = ядро.дочекатись().await?;
    привітатись(&mut client).await?;

    write_frame(
        &mut client,
        &Request::Variants {
            url: "невідомо://щось".to_owned(),
        },
    )
    .await?;

    match read_frame::<_, Response>(&mut client).await? {
        Response::Error { message, .. } => {
            assert!(
                message.contains("невідомо://щось"),
                "помилка має називати посилання, а не лише факт відмови: {message}"
            );
        }
        other => anyhow::bail!("на невідоме посилання чекали відмову, а не {other:?}"),
    }

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

#[tokio::test]
async fn налаштування_переживають_рестарт_ядра() -> anyhow::Result<()> {
    let mut ядро = Ядро::запустити("cfg")?;
    ядро.прибрати_теку = false;
    let mut client = ядро.дочекатись().await?;
    привітатись(&mut client).await?;

    write_frame(
        &mut client,
        &Request::Configure {
            max_concurrent: Some(7),
            rate_limit: Some(50 * 1024),
            post_action: Some("sleep".into()),
            schedule_from: Some("22:00".into()),
            schedule_to: Some("07:00".into()),
            quiet_from: Some("00:00".into()),
            quiet_to: Some("06:00".into()),
            quiet_rate: Some(10 * 1024),
        },
    )
    .await?;
    match read_frame::<_, Response>(&mut client).await? {
        Response::Ok => {}
        other => anyhow::bail!("configure не прийнято: {other:?}"),
    }

    let db = ядро.data.join("tasks.db");
    let downloads = ядро.data.clone();
    ядро.зупинити();
    drop(client);

    // Дати першому процесу час відпустити файл бази.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pipe = if cfg!(windows) {
        format!(r"\\.\pipe\downloader-e2e-cfg2-{unique}")
    } else {
        format!("/tmp/downloader-e2e-cfg2-{unique}.sock")
    };

    let mut child = Command::new(env!("CARGO_BIN_EXE_downloader-core"))
        .arg("--pipe")
        .arg(&pipe)
        .arg("--db")
        .arg(&db)
        .arg("--downloads")
        .arg(&downloads)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let mut client2 = {
        let mut s = None;
        for _ in 0..100 {
            if let Ok(c) = connect_to(&pipe).await {
                s = Some(c);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        s.ok_or_else(|| anyhow::anyhow!("друге ядро не підняло канал"))?
    };
    привітатись(&mut client2).await?;
    write_frame(&mut client2, &Request::Settings).await?;
    match read_frame::<_, Response>(&mut client2).await? {
        Response::Settings {
            max_concurrent,
            rate_limit,
            post_action,
            schedule_from,
            schedule_to,
            quiet_from,
            quiet_to,
            quiet_rate,
        } => {
            assert_eq!(max_concurrent, 7);
            assert_eq!(rate_limit, 50 * 1024);
            assert_eq!(post_action, "sleep");
            assert_eq!(schedule_from.as_deref(), Some("22:00"));
            assert_eq!(schedule_to.as_deref(), Some("07:00"));
            assert_eq!(quiet_from.as_deref(), Some("00:00"));
            assert_eq!(quiet_to.as_deref(), Some("06:00"));
            assert_eq!(quiet_rate, 10 * 1024);
        }
        other => anyhow::bail!("settings: {other:?}"),
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&downloads);
    Ok(())
}

#[tokio::test]
async fn список_завдань_переживає_рестарт_ядра() -> anyhow::Result<()> {
    let server = EvilServer::start().await?;
    let mut ядро = Ядро::запустити("restore")?;
    ядро.прибрати_теку = false;
    let mut client = ядро.дочекатись().await?;
    привітатись(&mut client).await?;

    write_frame(
        &mut client,
        &Request::Add {
            url: server.url("/slow-range/1m/256k"),
            dest: Some(ядро.data.join("довге.bin").display().to_string()),
            parts: Some(2),
            cookies: None,
            referer: None,
            variant: None,
            queue: None,
        },
    )
    .await?;
    let id = match read_frame::<_, Response>(&mut client).await? {
        Response::Added { id } => id,
        other => anyhow::bail!("add: {other:?}"),
    };

    let db = ядро.data.join("tasks.db");
    let downloads = ядро.data.clone();
    ядро.зупинити();
    drop(client);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pipe = if cfg!(windows) {
        format!(r"\\.\pipe\downloader-e2e-restore2-{unique}")
    } else {
        format!("/tmp/downloader-e2e-restore2-{unique}.sock")
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_downloader-core"))
        .arg("--pipe")
        .arg(&pipe)
        .arg("--db")
        .arg(&db)
        .arg("--downloads")
        .arg(&downloads)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let mut client2 = {
        let mut s = None;
        for _ in 0..100 {
            if let Ok(c) = connect_to(&pipe).await {
                s = Some(c);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        s.ok_or_else(|| anyhow::anyhow!("друге ядро не підняло канал"))?
    };
    привітатись(&mut client2).await?;
    write_frame(&mut client2, &Request::List).await?;
    match read_frame::<_, Response>(&mut client2).await? {
        Response::Tasks { tasks } => {
            assert_eq!(tasks.len(), 1, "після рестарту список порожній");
            assert_eq!(tasks[0].id, id);
            assert_eq!(tasks[0].name, "довге.bin");
        }
        other => anyhow::bail!("list: {other:?}"),
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&downloads);
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn керування_іменованими_чергами_через_ipc() -> anyhow::Result<()> {
    let ядро = Ядро::запустити("queues_ipc")?;
    let mut client = ядро.дочекатись().await?;
    привітатись(&mut client).await?;

    // 1. Початковий список черг має містити "default"
    write_frame(&mut client, &Request::Queues).await?;
    match read_frame::<_, Response>(&mut client).await? {
        Response::Queues { queues } => {
            assert_eq!(queues.len(), 1);
            assert_eq!(queues[0].name, "default");
        }
        other => anyhow::bail!("очікували Queues, отримали {other:?}"),
    }

    // 2. Створення черги
    write_frame(
        &mut client,
        &Request::QueueCreate {
            name: "fast".to_owned(),
            max_concurrent: Some(5),
            rate_limit: None,
            schedule_from: None,
            schedule_to: None,
            post_action: None,
        },
    )
    .await?;
    match read_frame::<_, Response>(&mut client).await? {
        Response::Ok => {}
        other => anyhow::bail!("очікували Ok, отримали {other:?}"),
    }

    // 3. Зміна параметрів черги
    write_frame(
        &mut client,
        &Request::QueueConfigure {
            name: "fast".to_owned(),
            max_concurrent: Some(10),
            rate_limit: Some(500_000),
            schedule_from: None,
            schedule_to: None,
            post_action: Some("sleep".to_owned()),
        },
    )
    .await?;
    match read_frame::<_, Response>(&mut client).await? {
        Response::Ok => {}
        other => anyhow::bail!("очікували Ok, отримали {other:?}"),
    }

    // 4. Перевірка оновленого списку черг
    write_frame(&mut client, &Request::Queues).await?;
    match read_frame::<_, Response>(&mut client).await? {
        Response::Queues { queues } => {
            assert_eq!(queues.len(), 2);
            let Some(fast) = queues.iter().find(|q| q.name == "fast") else {
                anyhow::bail!("не знайдено fast");
            };
            assert_eq!(fast.max_concurrent, 10);
            assert_eq!(fast.rate_limit, 500_000);
            assert_eq!(fast.post_action, "sleep");
        }
        other => anyhow::bail!("очікували Queues, отримали {other:?}"),
    }

    // 5. Перейменування черги
    write_frame(
        &mut client,
        &Request::QueueRename {
            old_name: "fast".to_owned(),
            new_name: "turbo".to_owned(),
        },
    )
    .await?;
    match read_frame::<_, Response>(&mut client).await? {
        Response::Ok => {}
        other => anyhow::bail!("очікували Ok, отримали {other:?}"),
    }

    // 6. Пауза та відновлення черги
    write_frame(
        &mut client,
        &Request::QueuePause {
            name: "turbo".to_owned(),
        },
    )
    .await?;
    match read_frame::<_, Response>(&mut client).await? {
        Response::Ok => {}
        other => anyhow::bail!("очікували Ok, отримали {other:?}"),
    }

    write_frame(&mut client, &Request::Queues).await?;
    match read_frame::<_, Response>(&mut client).await? {
        Response::Queues { queues } => {
            let Some(turbo) = queues.iter().find(|q| q.name == "turbo") else {
                anyhow::bail!("не знайдено turbo");
            };
            assert!(turbo.paused);
        }
        other => anyhow::bail!("очікували Queues, отримали {other:?}"),
    }

    write_frame(
        &mut client,
        &Request::QueueResume {
            name: "turbo".to_owned(),
        },
    )
    .await?;
    match read_frame::<_, Response>(&mut client).await? {
        Response::Ok => {}
        other => anyhow::bail!("очікували Ok, отримали {other:?}"),
    }

    // 7. Видалення черги
    write_frame(
        &mut client,
        &Request::QueueDelete {
            name: "turbo".to_owned(),
        },
    )
    .await?;
    match read_frame::<_, Response>(&mut client).await? {
        Response::Ok => {}
        other => anyhow::bail!("очікували Ok, отримали {other:?}"),
    }

    write_frame(&mut client, &Request::Queues).await?;
    match read_frame::<_, Response>(&mut client).await? {
        Response::Queues { queues } => {
            assert_eq!(queues.len(), 1);
            assert_eq!(queues[0].name, "default");
        }
        other => anyhow::bail!("очікували Queues, отримали {other:?}"),
    }

    Ok(())
}

#[tokio::test]
async fn портативний_режим_створює_базу_поруч_із_exe() -> anyhow::Result<()> {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let port_dir = std::env::temp_dir().join(format!("dl-portable-test-{unique}"));
    std::fs::create_dir_all(&port_dir)?;

    let orig_exe = std::path::PathBuf::from(env!("CARGO_BIN_EXE_downloader-core"));
    let Some(exe_name) = orig_exe.file_name() else {
        anyhow::bail!("немає імені виконуваного файлу");
    };
    let port_exe = port_dir.join(exe_name);
    std::fs::copy(&orig_exe, &port_exe)?;

    // Створюємо маркерний файл portable.txt поруч із копією бінарника
    let marker = port_dir.join(downloader_winutil::PORTABLE_MARKER);
    std::fs::write(&marker, b"")?;

    let pipe = if cfg!(windows) {
        format!(r"\\.\pipe\downloader-port-{unique}")
    } else {
        format!("/tmp/downloader-port-{unique}.sock")
    };

    // Запускаємо ядро без аргументів --db та --downloads
    let mut child = Command::new(&port_exe)
        .arg("--pipe")
        .arg(&pipe)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let mut connected = false;
    for _ in 0..100 {
        if let Ok(mut client) = connect_to(&pipe).await
            && привітатись(&mut client).await.is_ok()
        {
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let _ = child.kill();
    let _ = child.wait();

    if !connected {
        anyhow::bail!("ядро у портативному режимі не відповіло на пінг");
    }

    let expected_db = port_dir.join("tasks.db");
    if !expected_db.is_file() {
        anyhow::bail!(
            "у портативному режимі tasks.db мав створитися у {}, але файл відсутній",
            port_dir.display()
        );
    }

    let _ = std::fs::remove_dir_all(&port_dir);

    Ok(())
}

#[tokio::test]
async fn звичайний_режим_створює_базу_у_localappdata() -> anyhow::Result<()> {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let appdata_dir = std::env::temp_dir().join(format!("dl-regular-appdata-{unique}"));
    std::fs::create_dir_all(&appdata_dir)?;

    let pipe = if cfg!(windows) {
        format!(r"\\.\pipe\downloader-reg-{unique}")
    } else {
        format!("/tmp/downloader-reg-{unique}.sock")
    };

    // Запускаємо ядро з підміненим LOCALAPPDATA і без аргументу --db
    let mut child = Command::new(env!("CARGO_BIN_EXE_downloader-core"))
        .arg("--pipe")
        .arg(&pipe)
        .env("LOCALAPPDATA", &appdata_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let mut connected = false;
    for _ in 0..100 {
        if let Ok(mut client) = connect_to(&pipe).await
            && привітатись(&mut client).await.is_ok()
        {
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }


    let _ = child.kill();
    let _ = child.wait();

    if !connected {
        anyhow::bail!("ядро у звичайному режимі не відповіло на пінг");
    }

    let expected_db = appdata_dir.join("Downloader").join("tasks.db");
    if !expected_db.is_file() {
        anyhow::bail!(
            "у звичайному режимі tasks.db мав створитися у {}, але файл відсутній",
            expected_db.display()
        );
    }

    let _ = std::fs::remove_dir_all(&appdata_dir);

    Ok(())
}


