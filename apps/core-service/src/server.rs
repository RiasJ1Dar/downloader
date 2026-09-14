//! Обслуговування клієнтів: рукостискання, запити, потік подій.

use std::sync::Arc;

use downloader_ipc::frame::{FrameError, read_frame, write_frame};
use downloader_ipc::protocol::{ErrorCode, Event, PROTOCOL_VERSION, Request, Response};
use downloader_ipc::transport::{Listener, Stream};

use crate::engine::Engine;

/// Приймати клієнтів, доки ядро живе.
pub async fn serve(mut listener: Listener, engine: Arc<Engine>) -> anyhow::Result<()> {
    tracing::info!("ядро слухає канал");

    loop {
        let stream = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                // Збій прийому одного клієнта не привід зупиняти ядро:
                // завантаження тривають, а клієнт постукає ще раз.
                tracing::warn!(error = %e, "не вдалося прийняти клієнта");
                continue;
            }
        };

        let engine = Arc::clone(&engine);
        tokio::spawn(async move {
            if let Err(e) = handle(stream, engine).await {
                tracing::debug!(error = %e, "з'єднання завершилось помилкою");
            }
        });
    }
}

/// Одне з'єднання від початку до кінця.
async fn handle(mut stream: Stream, engine: Arc<Engine>) -> anyhow::Result<()> {
    // Рукостискання обов'язкове й має бути першим. Без нього неможливо
    // відрізнити старого клієнта від нового — і різниця вилізе не одразу,
    // а посеред роботи, у вигляді незрозумілого поля.
    let hello: Request = read_frame(&mut stream).await?;

    let client = match hello {
        Request::Hello {
            client,
            protocol_version,
        } => {
            if protocol_version != PROTOCOL_VERSION {
                write_frame(
                    &mut stream,
                    &Response::Error {
                        code: ErrorCode::VersionMismatch,
                        message: format!(
                            "клієнт говорить версією {protocol_version}, ядро — {PROTOCOL_VERSION}; \
                             оновіть програму цілком"
                        ),
                    },
                )
                .await?;
                return Ok(());
            }
            client
        }

        other => {
            write_frame(
                &mut stream,
                &Response::Error {
                    code: ErrorCode::HandshakeRequired,
                    message: format!("першим повідомленням має бути hello, а прийшло {other:?}"),
                },
            )
            .await?;
            return Ok(());
        }
    };

    tracing::info!(%client, "клієнт під'єднався");

    write_frame(
        &mut stream,
        &Response::Hello {
            server_version: downloader_core::VERSION.to_owned(),
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .await?;

    loop {
        let req: Request = match read_frame(&mut stream).await {
            Ok(r) => r,
            // Клієнт пішов — звичайне завершення розмови.
            Err(FrameError::Closed) => {
                tracing::info!(%client, "клієнт від'єднався");
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };

        // Підписка перетворює з'єднання на потік подій: далі клієнт уже
        // нічого не питає, лише слухає.
        if matches!(req, Request::Subscribe) {
            write_frame(&mut stream, &Response::Ok).await?;
            return stream_events(stream, engine).await;
        }

        let resp = dispatch(req, &engine).await;
        write_frame(&mut stream, &resp).await?;
    }
}

/// Виконати один запит.
async fn dispatch(req: Request, engine: &Arc<Engine>) -> Response {
    match req {
        Request::Ping => Response::Ok,

        Request::List => Response::Tasks {
            tasks: engine.list(),
        },

        // Невідоме завдання віддає порожню розкладку, а не помилку: вікно
        // питає про виділений рядок, і той міг зникнути між знімком і
        // запитом. Помилка тут була б блиманням на порожньому місці.
        Request::Details { id } => Response::Details {
            id,
            parts: engine.details(id),
        },

        Request::Variants { url } => match engine.variants(&url).await {
            Ok(variants) => Response::Variants { variants },
            Err(e) => Response::Error {
                code: ErrorCode::Internal,
                message: e.to_string(),
            },
        },

        Request::Add {
            url,
            dest,
            parts,
            cookies,
            referer,
            variant,
        } => {
            let session = downloader_core::protocol::Session::from_parts(cookies, referer);
            match engine.add(&url, dest.map(Into::into), parts, session, variant).await {
                Ok(id) => Response::Added { id },
                Err(e) => Response::Error {
                    code: ErrorCode::Internal,
                    message: e.to_string(),
                },
            }
        }

        Request::Remove { id, with_file } => match engine.remove(id, with_file) {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error {
                code: ErrorCode::NotFound,
                message: e.to_string(),
            },
        },

        Request::Pause { id } => match engine.pause(id) {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error {
                code: ErrorCode::InvalidState,
                message: e.to_string(),
            },
        },

        Request::Resume { id } => match engine.resume(id).await {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error {
                code: ErrorCode::NotFound,
                message: e.to_string(),
            },
        },

        Request::Settings => {
            let s = engine.settings();
            Response::Settings {
                max_concurrent: s.max_concurrent,
                rate_limit: s.rate_limit,
                post_action: s.post_action.as_str().to_owned(),
                schedule_from: s.schedule_from.map(downloader_core::format_hhmm),
                schedule_to: s.schedule_to.map(downloader_core::format_hhmm),
                quiet_from: s.quiet_from.map(downloader_core::format_hhmm),
                quiet_to: s.quiet_to.map(downloader_core::format_hhmm),
                quiet_rate: s.quiet_rate,
            }
        }

        Request::Configure {
            max_concurrent,
            rate_limit,
            post_action,
            schedule_from,
            schedule_to,
            quiet_from,
            quiet_to,
            quiet_rate,
        } => match engine.configure(downloader_core::SettingsPatch {
            max_concurrent,
            rate_limit,
            post_action,
            schedule_from,
            schedule_to,
            quiet_from,
            quiet_to,
            quiet_rate,
        }) {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error {
                code: ErrorCode::InvalidState,
                message: e.to_string(),
            },
        },

        Request::Hello { .. } => Response::Error {
            code: ErrorCode::InvalidState,
            message: "рукостискання вже відбулось".to_owned(),
        },

        Request::Subscribe => Response::Ok,
    }
}

/// Гнати клієнтові події, доки він слухає.
async fn stream_events(mut stream: Stream, engine: Arc<Engine>) -> anyhow::Result<()> {
    let mut rx = engine.subscribe();

    // Перший знімок — одразу, не чекаючи тіку: інакше щойно відкрите вікно
    // показувало б порожній список чверть секунди.
    write_frame(
        &mut stream,
        &Event::Snapshot {
            tasks: engine.list(),
        },
    )
    .await?;

    loop {
        match rx.recv().await {
            Ok(event) => {
                if write_frame(&mut stream, &event).await.is_err() {
                    // Клієнт закрився — нормальний кінець.
                    return Ok(());
                }
            }

            // Клієнт не встигав, і найстаріші знімки для нього загубились.
            // Це не біда: наступний знімок самодостатній і містить усе.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::debug!(skipped = n, "клієнт відстав, пропущено знімків");
            }

            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}
