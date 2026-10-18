//! `downloader-core` — ядро завантажень окремим процесом.
//!
//! Живе в треї, тримає завдання і качає. Вікно, CLI та native messaging host
//! під'єднуються до нього локальним каналом і можуть приходити й зникати
//! скільки завгодно разів — завантаження це не чіпає.
//!
//! # Чому окремий процес
//!
//! * **Пам'ять.** Закрите вікно вивантажує WebView2 цілком; у треї лишається
//!   ядро на кілька десятків мегабайтів, а не гроно процесів браузера.
//! * **Живучість.** Крах або оновлення UI не перериває качання.
//! * **Один контракт.** Зовнішні плагіни-протоколи говоритимуть тим самим
//!   протоколом, що й вікно, — а не другим, окремо вигаданим.

// Ядро — фоновий процес. Підсистема GUI не створює console window навіть
// при прямому запуску ярлика автозапуску; діагностика лишається в журналі.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod after;
mod clipboard_watch;
mod engine;
mod server;
#[cfg(windows)]
mod tray;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use downloader_i18n as i18n;
use downloader_ipc::transport::Listener;

#[derive(Parser)]
#[command(
    name = "downloader-core",
    version,
    about = "Ядро менеджера завантажень: тримає завдання й качає"
)]
struct Cli {
    /// Файл бази завдань.
    #[arg(long)]
    db: Option<PathBuf>,

    /// Тека, куди класти файли без явного шляху.
    #[arg(long)]
    downloads: Option<PathBuf>,

    /// Ім'я каналу. Потрібне тестам і кільком незалежним екземплярам;
    /// у звичайній роботі не задається.
    #[arg(long)]
    pipe: Option<String>,

    /// Мова: `uk` або `en`. Російська ОС → українська.
    #[arg(long)]
    lang: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Помилка старту мусить лишити слід у журналі, а не лише в stderr.
    //
    // Ядро запускається з автозапуску, без консолі: коли воно не піднімається
    // — скажімо, канал уже зайнятий іншим екземпляром, — людина бачить, що
    // програми просто немає. Текст `Error: не вдалося зайняти канал…` іде в
    // нікуди, а в журналі лишається самий лише рядок про те, що журнал
    // ведеться. Саме так виглядала ця вада до виправлення.
    match запустити().await {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::error!(помилка = %format!("{e:#}"), "ядро не запустилося");
            Err(e)
        }
    }
}

async fn запустити() -> anyhow::Result<()> {
    let cli = Cli::parse();
    i18n::init(cli.lang.as_deref());
    let ставити_nmhost = cli.pipe.is_none();

    let exe = std::env::current_exe().ok();
    let is_portable = downloader_winutil::is_portable();
    if is_portable {
        tracing::info!("активовано портативний режим (знайдено portable.txt)");
    }

    let data_dir = cli
        .db
        .clone()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| downloader_winutil::data_dir_for_exe(exe.as_deref()));
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("не вдалося створити теку даних {}", data_dir.display()))?;

    // Журнал вмикається аж тут, бо до розбору аргументів невідомо, де тека
    // даних: у портативному режимі вона поруч із програмою, у звичайному — у
    // профілі. Кілька перших рядків (розбір аргументів) через це в файл не
    // потрапляють — зате файл завжди лежить там, де його шукатиме людина.
    let шлях_журналу = downloader_winutil::zhurnal::файл_журналу(&data_dir);
    let фільтр =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    match downloader_winutil::zhurnal::Журнал::відкрити(&шлях_журналу) {
        Ok(журнал) => {
            use tracing_subscriber::fmt::writer::MakeWriterExt as _;
            // Стелю лишаємо: консоль потрібна тому, хто запустив ядро руками.
            let пише = std::io::stderr.and(move || журнал.clone());
            tracing_subscriber::fmt()
                .with_env_filter(фільтр)
                .with_ansi(false)
                .with_writer(пише)
                .init();
            tracing::info!(файл = %шлях_журналу.display(), "журнал ведеться");
        }
        Err(e) => {
            // Диск лише для читання, немає прав, зайнятий файл — програма має
            // працювати далі, але людина мусить знати, що слідів не буде.
            tracing_subscriber::fmt().with_env_filter(фільтр).init();
            tracing::warn!(
                файл = %шлях_журналу.display(),
                помилка = %e,
                "журнал у файл не ведеться — лишається тільки консоль"
            );
        }
    }

    let db = cli.db.unwrap_or_else(|| data_dir.join("tasks.db"));
    let downloads = cli
        .downloads
        .unwrap_or_else(|| downloader_winutil::downloads_dir_for_exe(exe.as_deref()));
    std::fs::create_dir_all(&downloads).with_context(|| {
        format!(
            "не вдалося створити теку завантажень {}",
            downloads.display()
        )
    })?;

    // ⚠️ Канал займається **до** відкриття бази й навмисно першим.
    //
    // Друге ядро має впасти саме тут, поки воно ще нічого не чіпало. Два
    // ядра на одну базу — це два планувальники, які качають ті самі
    // завдання в той самий файл; помітити це потім було б важко, а наслідки
    // незворотні.
    let pipe = cli
        .pipe
        .unwrap_or_else(|| downloader_ipc::protocol::PIPE_NAME.to_owned());

    let listener = Listener::bind_named(&pipe).map_err(|e| {
        anyhow::anyhow!(
            "не вдалося зайняти канал {pipe}: {e}. \
             Найімовірніше, ядро вже запущене — друге не потрібне"
        )
    })?;

    // ⚠️ Реєстр заповнюється **тут**, а не в ядрі. Ядро не має способу
    // створити модуль самостійно — саме це й тримає межу, про яку йдеться
    // в `crates/core/src/protocol.rs`.
    //
    // Порядок важливий: спеціалізовані модулі (HLS, DASH, торент) реєструються
    // перед загальним HTTP, інакше HTTP забирав би собі все, що починається
    // з `https://`, включно з посиланнями на маніфести.
    let mut registry = downloader_core::protocol::Registry::new();
    registry.register(Box::new(downloader_proto_hls::HlsProtocol::new()?));
    registry.register(Box::new(downloader_proto_dash::DashProtocol::new()?));
    registry.register(Box::new(downloader_proto_ytdlp::YtdlpProtocol::new()));
    registry.register(Box::new(downloader_proto_http::HttpProtocol::new(8)?));

    let engine = engine::Engine::new(&db, downloads.clone(), registry)?;

    let clip = std::env::var("DOWNLOADER_WATCH_CLIPBOARD").unwrap_or_default();
    if clip != "0" {
        tokio::spawn(clipboard_watch::run());
    }

    if ставити_nmhost && let Ok(mut host) = std::env::current_exe() {
        host.set_file_name(if cfg!(windows) {
            "downloader-nmhost.exe"
        } else {
            "downloader-nmhost"
        });
        if host.is_file() {
            let mut command = std::process::Command::new(&host);
            command.arg("--install");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt as _;
                // GUI-ядро не повинне породжувати консоль дочірнього host.
                command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            match command.status() {
                Ok(st) if st.success() => {
                    tracing::info!("native host прописано для браузера");
                }
                Ok(st) => tracing::warn!(code = ?st.code(), "native host --install не вдався"),
                Err(e) => tracing::warn!(error = %e, "не запустити downloader-nmhost --install"),
            }
        }
    }

    tracing::info!(
        база = %db.display(),
        завантаження = %downloads.display(),
        "ядро запущено"
    );

    #[cfg(windows)]
    let mut з_трею = if ставити_nmhost {
        match tray::старт(Arc::clone(&engine), downloads.clone()) {
            Ok(rx) => Some(rx),
            Err(e) => {
                tracing::warn!(error = %e, "трей не стартував");
                None
            }
        }
    } else {
        None
    };

    tokio::select! {
        res = server::serve(listener, engine) => res?,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("отримано сигнал зупинки, ядро завершується");
        }
        _ = async {
            #[cfg(windows)]
            if let Some(rx) = з_трею.as_mut() {
                while !*rx.borrow() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            } else {
                std::future::pending::<()>().await;
            }
            #[cfg(not(windows))]
            std::future::pending::<()>().await;
        } => {
            tracing::info!("вихід з меню трею");
        }
    }

    Ok(())
}
