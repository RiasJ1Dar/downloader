//! Командний рядок менеджера завантажень.
//!
//! Перша оболонка ядра й постійний тестовий стенд. Усе, що вміє вікно,
//! спершу має вміти CLI — інакше логіка непомітно заповзає в UI, і потім
//! її не витягнути.
//!
//! На Ф3 CLI перейде на IPC і стане клієнтом ядра-сервісу, рівно таким
//! самим, яким потім буде вікно.

mod client;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use downloader_proto_http::download::{Options, download_with_probe};
use downloader_proto_http::probe::probe;
use downloader_ipc::protocol::{Event, Request, Response};
use downloader_winutil::{motw, names, paths};

#[derive(Parser)]
#[command(
    name = "dl",
    version,
    about = "Менеджер завантажень: сегментоване качання з докачуванням"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Завантажити файл.
    Get {
        /// Посилання.
        url: String,

        /// Куди зберегти. Без цього ім'я береться з `Content-Disposition`
        /// або з URL.
        #[arg(short, long)]
        out: Option<PathBuf>,

        /// Скільки з'єднань відкривати.
        #[arg(short = 'n', long, default_value_t = 8)]
        parts: usize,

        /// Найменший шматок на з'єднання, у байтах.
        #[arg(long, default_value_t = 1 << 20)]
        min_chunk: u64,

        /// Стеля швидкості в кілобайтах за секунду. 0 — без обмежень.
        #[arg(long, default_value_t = 0)]
        limit_kb: u64,

        /// Як часто скидати стан на диск, у мілісекундах.
        #[arg(long, default_value_t = 2000)]
        checkpoint_ms: u64,
    },

    /// Показати, що відомо про посилання, нічого не качаючи.
    Probe {
        /// Посилання.
        url: String,
    },

    /// Передати завантаження ядру, яке живе у треї.
    ///
    /// На відміну від `get`, завдання переживе закриття цього вікна консолі:
    /// качає ядро, а не ми.
    Add {
        /// Посилання.
        url: String,

        /// Куди зберегти.
        #[arg(short, long)]
        out: Option<PathBuf>,

        /// Скільки з'єднань.
        #[arg(short = 'n', long)]
        parts: Option<usize>,
    },

    /// Показати завдання ядра.
    List,

    /// Стежити за прогресом, доки не натиснуто Ctrl+C.
    Watch,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let client = reqwest::Client::builder()
        .build()
        .context("не вдалося створити HTTP-клієнт")?;

    match cli.command {
        Command::Add { url, out, parts } => {
            let mut core = client::Client::connect().await?;

            let resp = core
                .call(&Request::Add {
                    url: url.clone(),
                    dest: out.map(|p| p.display().to_string()),
                    parts,
                })
                .await?;

            match resp {
                Response::Added { id } => println!("завдання {id} прийнято ядром"),
                Response::Error { message, .. } => anyhow::bail!(message),
                other => anyhow::bail!("несподівана відповідь ядра: {other:?}"),
            }
        }

        Command::List => {
            let mut core = client::Client::connect().await?;

            match core.call(&Request::List).await? {
                Response::Tasks { tasks } if tasks.is_empty() => {
                    println!("завдань немає");
                }
                Response::Tasks { tasks } => {
                    for t in tasks {
                        let прогрес = t.progress().map_or_else(
                            || format_size(t.done),
                            |p| format!("{:.0}% ({})", p * 100.0, format_size(t.done)),
                        );
                        println!(
                            "{:>4}  {:<10} {:>18}  {:>10}/с  {}",
                            t.id,
                            t.status,
                            прогрес,
                            format_size(t.speed),
                            t.name
                        );
                        if let Some(e) = t.error {
                            println!("        ⚠ {e}");
                        }
                    }
                }
                Response::Error { message, .. } => anyhow::bail!(message),
                other => anyhow::bail!("несподівана відповідь ядра: {other:?}"),
            }
        }

        Command::Watch => {
            let core = client::Client::connect().await?;
            let mut events = core.subscribe().await?;

            println!("стежу за ядром, Ctrl+C щоб вийти");

            while let Some(event) = events.next().await? {
                match event {
                    Event::Snapshot { tasks } => {
                        let активні: Vec<_> =
                            tasks.iter().filter(|t| t.status == "running").collect();
                        if активні.is_empty() {
                            continue;
                        }
                        for t in активні {
                            let прогрес = t
                                .progress()
                                .map_or_else(|| "?".to_owned(), |p| format!("{:.0}%", p * 100.0));
                            println!(
                                "  {:>4}  {:>5}  {:>10}/с  {}",
                                t.id,
                                прогрес,
                                format_size(t.speed),
                                t.name
                            );
                        }
                    }
                    Event::Finished { id, path, bytes } => {
                        println!("✓ завдання {id} готове: {} — {path}", format_size(bytes));
                    }
                    Event::Failed { id, message } => {
                        println!("✗ завдання {id} впало: {message}");
                    }
                }
            }

            println!("ядро закрило з'єднання");
        }

        Command::Probe { url } => {
            let info = probe(&client, &url).await?;

            println!("адреса:      {}", info.final_url);
            println!(
                "розмір:      {}",
                info.size.map_or_else(|| "невідомий".to_owned(), format_size)
            );
            println!(
                "сегменти:    {}",
                if info.resumable {
                    "можна різати й докачувати"
                } else {
                    "тільки один потік, докачування недоступне"
                }
            );
            println!("Accept-Ranges: {:?}", info.declared);
            println!(
                "ім'я:        {}",
                info.filename().unwrap_or_else(|| "невідоме".to_owned())
            );
        }

        Command::Get {
            url,
            out,
            parts,
            min_chunk,
            limit_kb,
            checkpoint_ms,
        } => {
            let info = probe(&client, &url).await?;

            // Ім'я з мережі складала стороння людина: спершу знешкодити,
            // потім переконатись, що не затираємо чужий файл.
            let dest = match out {
                Some(p) => p,
                None => {
                    let raw = info.filename().unwrap_or_else(|| names::ЗАПАСНЕ_ІМʼЯ.to_owned());
                    let safe = names::sanitize(&raw);
                    if safe != raw {
                        println!("ім'я з сервера знешкоджено: {raw:?} → {safe:?}");
                    }
                    paths::unique_path(&PathBuf::from(safe))
                }
            };

            let opts = Options {
                parts,
                min_chunk,
                rate_limit: limit_kb.saturating_mul(1024),
                checkpoint_every: Duration::from_millis(checkpoint_ms),
                ..Options::default()
            };

            println!(
                "качаю {} → {} ({}, {} потоків)",
                info.final_url,
                dest.display(),
                info.size.map_or_else(|| "розмір невідомий".to_owned(), format_size),
                info.usable_parts(parts)
            );

            let started = std::time::Instant::now();
            let out = download_with_probe(&client, &info, &dest, &opts).await?;

            let secs = started.elapsed().as_secs_f64().max(0.001);
            println!(
                "готово: {} за {:.1} с ({}/с), сегментів {}",
                format_size(out.bytes),
                secs,
                format_size((out.bytes as f64 / secs) as u64),
                out.segments
            );

            // Мовчазний `download` теж придатний, але людині корисно бачити
            // шлях: `-o` могло не бути, і файл ліг за іменем із заголовка.
            println!("файл:   {}", out.path.display());

            // Mark-of-the-Web. Без неї SmartScreen не попередить людину про
            // завантажений з мережі виконуваний файл — тобто ми стаємо
            // засобом обходу захисту, і антивіруси це помічають.
            //
            // Невдача тут не скасовує завантаження (файл цілий і на диску),
            // але й мовчати про неї не можна: на FAT32 чи мережевому диску
            // мітка просто не запишеться, і людина має про це знати.
            match motw::mark(&out.path, Some(&info.final_url), None) {
                Ok(()) => {}
                Err(e) => println!("⚠ не вдалося позначити файл як завантажений з мережі: {e}"),
            }
        }
    }

    Ok(())
}

/// Розмір у зрозумілому вигляді.
fn format_size(bytes: u64) -> String {
    const ОДИНИЦІ: [&str; 5] = ["Б", "КБ", "МБ", "ГБ", "ТБ"];
    let mut value = bytes as f64;
    let mut unit = 0;

    while value >= 1024.0 && unit + 1 < ОДИНИЦІ.len() {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{bytes} {}", ОДИНИЦІ[unit])
    } else {
        format!("{value:.1} {}", ОДИНИЦІ[unit])
    }
}
