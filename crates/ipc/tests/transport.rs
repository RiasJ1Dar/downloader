//! Перевірка локального каналу на справжніх named pipe / сокетах.

use downloader_ipc::frame::{read_frame, write_frame};
use downloader_ipc::protocol::{PROTOCOL_VERSION, Request, Response};
use downloader_ipc::transport::{Listener, connect_to};

/// Унікальне ім'я каналу для одного тесту.
fn канал(tag: &str) -> String {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    if cfg!(windows) {
        format!(r"\\.\pipe\downloader-test-{tag}-{unique}")
    } else {
        format!("/tmp/downloader-test-{tag}-{unique}.sock")
    }
}

#[tokio::test]
async fn клієнт_і_ядро_обмінюються_повідомленнями() -> anyhow::Result<()> {
    let name = канал("hello");
    let mut listener = Listener::bind_named(&name)?;

    let серверна = tokio::spawn(async move {
        let mut stream = listener.accept().await?;
        let req: Request = read_frame(&mut stream).await?;

        let відповідь = match req {
            Request::Hello {
                protocol_version, ..
            } if protocol_version == PROTOCOL_VERSION => Response::Hello {
                server_version: "тест".into(),
                protocol_version: PROTOCOL_VERSION,
            },
            _ => Response::Ok,
        };

        write_frame(&mut stream, &відповідь).await?;
        Ok::<_, anyhow::Error>(())
    });

    let mut client = connect_to(&name).await?;
    write_frame(
        &mut client,
        &Request::Hello {
            client: "cli".into(),
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await?;

    let resp: Response = read_frame(&mut client).await?;
    match resp {
        Response::Hello {
            protocol_version, ..
        } => assert_eq!(protocol_version, PROTOCOL_VERSION),
        other => anyhow::bail!("очікували рукостискання, отримали {other:?}"),
    }

    серверна.await??;
    Ok(())
}

#[tokio::test]
async fn канал_приймає_клієнтів_один_за_одним() -> anyhow::Result<()> {
    // ⚠️ Саме тут ловиться помилка з екземплярами named pipe: якщо створювати
    // наступний екземпляр після того, як попередній зайняли, другий клієнт
    // натрапить на мить, коли каналу не існує.
    let name = канал("serial");
    let mut listener = Listener::bind_named(&name)?;

    let серверна = tokio::spawn(async move {
        for _ in 0..3 {
            let mut stream = listener.accept().await?;
            let _req: Request = read_frame(&mut stream).await?;
            write_frame(&mut stream, &Response::Ok).await?;
        }
        Ok::<_, anyhow::Error>(())
    });

    for i in 0..3 {
        let mut client = connect_to(&name)
            .await
            .map_err(|e| anyhow::anyhow!("клієнт {i} не під'єднався: {e}"))?;
        write_frame(&mut client, &Request::Ping).await?;
        let _resp: Response = read_frame(&mut client).await?;
    }

    серверна.await??;
    Ok(())
}

#[tokio::test]
async fn підʼєднання_без_ядра_дає_зрозумілу_відмову() {
    let name = канал("absent");
    let err = connect_to(&name)
        .await
        .expect_err("каналу немає — під'єднання не може вдатись");

    // Клієнт мусить відрізняти «ядро не запущене» від справжньої поломки:
    // у першому випадку його треба запустити, а не сипати помилками.
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::NotFound,
        "несподіваний вид помилки: {err:?}"
    );
}

#[tokio::test]
async fn друге_ядро_не_займає_той_самий_канал() -> anyhow::Result<()> {
    let name = канал("single");
    let _перше = Listener::bind_named(&name)?;

    // Два ядра на одну базу — це два планувальники, які качають ті самі
    // завдання в той самий файл. Друге має впасти одразу.
    let друге = Listener::bind_named(&name);
    assert!(
        друге.is_err(),
        "другий екземпляр ядра зайняв канал замість того, щоб відмовитись"
    );

    Ok(())
}

#[tokio::test]
async fn обрив_клієнта_не_валить_серверну_сторону() -> anyhow::Result<()> {
    let name = канал("drop");
    let mut listener = Listener::bind_named(&name)?;

    let серверна = tokio::spawn(async move {
        let mut stream = listener.accept().await?;
        // Клієнт піде, не сказавши нічого — читання має повернути «закрито»,
        // а не помилку й не паніку.
        let res: Result<Request, _> = read_frame(&mut stream).await;
        Ok::<_, anyhow::Error>(res.is_err())
    });

    {
        let _client = connect_to(&name).await?;
        // одразу дропаємо
    }

    assert!(серверна.await??, "сервер мав побачити закриття з'єднання");
    Ok(())
}
