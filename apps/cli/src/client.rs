//! Клієнт ядра: те саме, що потім робитиме вікно.
//!
//! CLI навмисно ходить до ядра тим самим шляхом, що й UI, — через
//! [`downloader_ipc`]. Якби CLI мав власну коротку дорогу до рушія, різниця
//! між ними накопичувалась би непомітно, і перша ж функція, зроблена «тільки
//! для вікна», лишилася б неперевіреною.

use anyhow::{Context, Result};
use downloader_ipc::frame::{read_frame, write_frame};
use downloader_ipc::protocol::{Event, PROTOCOL_VERSION, Request, Response};
use downloader_ipc::transport::{ClientStream, connect};

/// З'єднання з ядром після рукостискання.
pub struct Client {
    stream: ClientStream,
}

impl Client {
    /// Під'єднатись і привітатись.
    ///
    /// Якщо ядра немає, повідомлення каже, що робити, а не лише те, що
    /// сталося: «файл не знайдено» на named pipe нікому нічого не пояснює.
    pub async fn connect() -> Result<Self> {
        let stream = connect().await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::anyhow!(
                    "ядро не запущене — запустіть `downloader-core` \
                     (воно живе у треї й тримає завантаження)"
                )
            } else {
                anyhow::anyhow!("не вдалося під'єднатись до ядра: {e}")
            }
        })?;

        let mut client = Self { stream };
        client.handshake().await?;
        Ok(client)
    }

    async fn handshake(&mut self) -> Result<()> {
        write_frame(
            &mut self.stream,
            &Request::Hello {
                client: "cli".to_owned(),
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .await
        .context("не вдалося привітатись із ядром")?;

        match read_frame::<_, Response>(&mut self.stream).await? {
            Response::Hello {
                server_version,
                protocol_version,
            } => {
                tracing::debug!(%server_version, protocol_version, "з'єднано з ядром");
                Ok(())
            }
            Response::Error { message, .. } => anyhow::bail!("ядро відмовило: {message}"),
            other => anyhow::bail!("ядро відповіло не рукостисканням: {other:?}"),
        }
    }

    /// Надіслати запит і дочекатись відповіді.
    pub async fn call(&mut self, req: &Request) -> Result<Response> {
        write_frame(&mut self.stream, req).await?;
        Ok(read_frame(&mut self.stream).await?)
    }

    /// Перетворити з'єднання на потік подій.
    ///
    /// Після цього запитів слати не можна — з'єднання належить подіям.
    pub async fn subscribe(mut self) -> Result<Events> {
        write_frame(&mut self.stream, &Request::Subscribe).await?;
        let _: Response = read_frame(&mut self.stream).await?;
        Ok(Events {
            stream: self.stream,
        })
    }
}

/// Потік подій від ядра.
pub struct Events {
    stream: ClientStream,
}

impl Events {
    /// Наступна подія. `None` — ядро закрило з'єднання.
    pub async fn next(&mut self) -> Result<Option<Event>> {
        match read_frame::<_, Event>(&mut self.stream).await {
            Ok(e) => Ok(Some(e)),
            Err(downloader_ipc::frame::FrameError::Closed) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}
