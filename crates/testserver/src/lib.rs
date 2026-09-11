//! «Злий» HTTP-сервер для тестів рушія завантажень.
//!
//! Сервер навмисно поводиться так, як поводяться реальні сервери у свій
//! найгірший день: ігнорує `Range`, рве з'єднання посеред тіла, віддає 503,
//! міняє `ETag` між запитами, бреше в `Content-Length`, віддає gzip, тягне
//! байти по краплині, водить ланцюгом редиректів і підсовує імена файлів,
//! на яких файлова система Windows ламається.
//!
//! ```no_run
//! # use downloader_testserver::EvilServer;
//! # async fn demo() -> anyhow::Result<()> {
//! let server = EvilServer::start().await?;
//! let url = server.url("/norange/1m/payload.bin");
//! // ... тест ...
//! server.shutdown().await;
//! # Ok(())
//! # }
//! ```
//!
//! # Чому не hyper/axum
//!
//! Уся цінність цього сервера — у праві брехати. Бібліотека, що поважає
//! протокол, не дасть ні написати неправдивий `Content-Length`, ні обірвати
//! тіло на середині. Тому тут голий [`tokio::net::TcpListener`] і відповіді,
//! складені руками.
//!
//! # Свідомі спрощення
//!
//! * Одне з'єднання — один запит, усі відповіді з `Connection: close`.
//! * Один діапазон у `Range`, без `multipart/byteranges`.
//! * `Date` і `Last-Modified` фіксовані — щоб тест не залежав від годинника.

#![forbid(unsafe_code)]

mod body;
mod handler;
mod request;

pub use body::{
    TINY_BODY, body_bytes, expected_sha256, expected_sha256_of_size, parse_size, path_segments,
};
pub use request::{RangeSpec, Request};

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Спільний стан інстансу: скільки разів звертались за кожним шляхом.
/// Потрібен `/flaky` (перші N запитів падають) і `/changing` (номер запиту
/// стає новим `ETag`).
#[derive(Default)]
pub(crate) struct ServerState {
    hits: Mutex<HashMap<String, u64>>,
}

impl ServerState {
    /// Порядковий номер цього запиту для шляху: 1 для першого.
    pub(crate) fn bump(&self, path: &str) -> u64 {
        // Отруєний м'ютекс не привід валити тестовий сервер: дані всередині
        // — лічильники, їх не зіпсує паніка в іншому потоці.
        let mut guard = self.hits.lock().unwrap_or_else(|p| p.into_inner());
        let counter = guard.entry(path.to_string()).or_insert(0);
        *counter += 1;
        *counter
    }

    /// Покоління live-плейлиста за кількістю запитів маніфесту, не за
    /// годинником: setup тесту не з'їдає вікно.
    ///
    /// 1–2 → лише seg0 (probe + перший fetch у `run` бачать те саме);
    /// 3 → лише seg1 (seg0 випав — хто чекав ENDLIST, той його втратив);
    /// 4+ → seg1 + ENDLIST.
    pub(crate) fn live_tick(&self) -> u8 {
        match self.bump("/hls/live/media") {
            1 | 2 => 1,
            3 => 2,
            _ => 3,
        }
    }
}

/// Запущений інстанс сервера. Слухає на `127.0.0.1` з випадковим портом, тож
/// кілька тестів можуть тримати свої інстанси одночасно.
///
/// Зупиняється явним [`EvilServer::shutdown`] або автоматично при `Drop`.
pub struct EvilServer {
    addr: SocketAddr,
    shutdown_tx: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl EvilServer {
    /// Піднімає сервер на `127.0.0.1:0` і повертає керунок ним.
    pub async fn start() -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("не вдалося зайняти порт на 127.0.0.1 — чи не зачинив брандмауер loopback?")?;
        let addr = listener
            .local_addr()
            .context("сокет піднявся, але не назвав свою адресу")?;

        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let state = Arc::new(ServerState::default());

        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, peer)) => {
                                let state = Arc::clone(&state);
                                tokio::spawn(async move {
                                    if let Err(err) = handler::handle_connection(stream, state).await {
                                        // Гучно: мовчазний збій у тестовому стенді
                                        // перетворює падіння тесту на загадку.
                                        tracing::warn!(
                                            %peer,
                                            error = %format!("{err:#}"),
                                            "злий сервер: з'єднання оброблено з помилкою"
                                        );
                                    }
                                });
                            }
                            Err(err) => {
                                tracing::error!(error = %err, "злий сервер: accept впав, акцептор зупиняється");
                                break;
                            }
                        }
                    }
                    changed = shutdown_rx.changed() => {
                        // Err — сендер дропнули разом з EvilServer, теж сигнал стоп.
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self {
            addr,
            shutdown_tx,
            task: Some(task),
        })
    }

    /// Адреса, на якій сервер слухає.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Порт інстансу.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// URL сценарію: `server.url("/plain/10m")`.
    pub fn url(&self, path: &str) -> String {
        if path.starts_with('/') {
            format!("http://{}{}", self.addr, path)
        } else {
            format!("http://{}/{}", self.addr, path)
        }
    }

    /// Зупиняє акцептор і чекає, доки він завершиться.
    ///
    /// Уже прийняті з'єднання не переривається чекати — вони дописують свої
    /// відповіді у власних задачах і згасають самі.
    pub async fn shutdown(mut self) {
        let _ = self.shutdown_tx.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for EvilServer {
    fn drop(&mut self) {
        // Сендер і так зараз дропнеться, але явний сигнал прибирає гонку:
        // акцептор може стояти на `changed()` і побачити `true` одразу.
        let _ = self.shutdown_tx.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
