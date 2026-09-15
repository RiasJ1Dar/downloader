//! Командний рядок менеджера завантажень.
//!
//! Перша оболонка ядра й постійний тестовий стенд. Усе, що вміє вікно,
//! спершу має вміти CLI — інакше логіка непомітно заповзає в UI, і потім
//! її не витягнути.
//!
//! Згодом командний рядок перейшов на IPC і став клієнтом ядра-сервісу,
//! рівно таким самим, яким є вікно.

mod client;
mod expand;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use downloader_core::protocol::{
    Cancel, PlannedFile, Progress, ProgressSink, Protocol, RateLimitSupport, RunContext,
    Session,
};
use downloader_ipc::protocol::{Event, Request, Response};
use downloader_proto_dash::DashProtocol;
use downloader_proto_hls::HlsProtocol;
use downloader_proto_ytdlp::YtdlpProtocol;
use downloader_proto_http::download::{Options, download_with_probe};
use downloader_proto_http::probe::{probe, probe_with_session};
use downloader_i18n::{self as i18n, t, t_pairs};
use downloader_winutil::{motw, names, paths, текст_буфера};

#[derive(Parser, Debug)]
#[command(
    name = "dl",
    version,
    about = "Менеджер завантажень: сегментоване качання з докачуванням"
)]
struct Cli {
    /// Мова: `uk` або `en`. Без прапорця — мова ОС; російська ОС → українська.
    #[arg(long, global = true)]
    lang: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
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

        /// Cookie-заголовок: `n=v; n2=v2`.
        #[arg(long)]
        cookie: Option<String>,

        /// Заголовок Referer.
        #[arg(long)]
        referer: Option<String>,

        /// Варіант якості зі `dl variants <url>`: наприклад `720`.
        #[arg(long)]
        variant: Option<String>,
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
        /// Посилання. Можна шаблон `file[001-010].zip`.
        url: Option<String>,

        /// Файл зі списком адрес (рядки, `#` — коментар).
        #[arg(long, conflicts_with = "clipboard")]
        list: Option<PathBuf>,

        /// Взяти адреси з буфера обміну.
        #[arg(long)]
        clipboard: bool,

        /// Куди зберегти. Для кількох адрес ігнорується (ядро саме іменує).
        #[arg(short, long)]
        out: Option<PathBuf>,

        /// Скільки з'єднань.
        #[arg(short = 'n', long)]
        parts: Option<usize>,

        /// Cookie-заголовок: `n=v; n2=v2`.
        #[arg(long)]
        cookie: Option<String>,

        /// Заголовок Referer.
        #[arg(long)]
        referer: Option<String>,

        /// Черга завантаження (типово "default").
        #[arg(short = 'q', long)]
        queue: Option<String>,
    },

    /// Показати завдання ядра.
    List,

    /// Поставити ffmpeg поруч із програмою.
    ///
    /// Потрібен, щоб зводити відео зі звуком: YouTube роздає їх окремо.
    FfmpegInstall,

    /// Які варіанти якості пропонує це посилання.
    ///
    /// Проба, не завантаження: нічого не качається, ядро не потрібне.
    Variants {
        /// Посилання.
        url: String,

        /// Cookie-заголовок: `n=v; n2=v2`.
        #[arg(long)]
        cookie: Option<String>,

        /// Заголовок Referer.
        #[arg(long)]
        referer: Option<String>,
    },

    /// Показати, як завдання поділене на частини.
    ///
    /// Те саме, що показує смужка сегментів у вікні. Правило проєкту: усе,
    /// що вміє вікно, спершу вміє CLI — інакше перша ж річ, зроблена «тільки
    /// для вікна», лишиться без перевірки.
    Parts {
        /// Ідентифікатор зі `dl list`.
        id: i64,
    },

    /// Зупинити завдання ядра.
    Pause {
        /// Ідентифікатор зі `dl list`.
        id: i64,
    },

    /// Продовжити зупинене завдання.
    Resume {
        /// Ідентифікатор зі `dl list`.
        id: i64,
    },

    /// Прибрати завдання зі списку ядра.
    Rm {
        /// Ідентифікатор зі `dl list`.
        id: i64,

        /// Видалити вже завантажені байти з диска.
        #[arg(long)]
        with_file: bool,
    },

    /// Стежити за прогресом, доки не натиснуто Ctrl+C.
    Watch,

    /// Показати правила ядра: черга, ліміт, розклад, післядія.
    Settings,

    /// Змінити правила ядра. Порожній прапорець — не чіпати те поле.
    Configure {
        /// Скільки завдань качати одночасно.
        #[arg(long)]
        max: Option<u32>,
        /// Стеля швидкості в кілобайтах за секунду. 0 — без обмеження.
        #[arg(long)]
        rate_kb: Option<u64>,
        /// Після порожньої черги: `none`, `sleep` або `shutdown`.
        #[arg(long)]
        after: Option<String>,
        /// Початок вікна старту, `ГГ:ХХ`. Порожньо разом із `--to` прибирає розклад.
        #[arg(long)]
        from: Option<String>,
        /// Кінець вікна старту.
        #[arg(long)]
        to: Option<String>,
        /// Початок нічного ліміту.
        #[arg(long)]
        quiet_from: Option<String>,
        /// Кінець нічного ліміту.
        #[arg(long)]
        quiet_to: Option<String>,
        /// Нічний ліміт у КБ/с. 0 — вимкнути нічний профіль.
        #[arg(long)]
        quiet_kb: Option<u64>,
        /// Прибрати розклад (качати завжди).
        #[arg(long)]
        clear_schedule: bool,
        /// Прибрати нічний профіль.
        #[arg(long)]
        clear_quiet: bool,
    },

    /// Оновити зовнішній yt-dlp (`yt-dlp -U`).
    YtdlpUpdate,

    /// Перемістити завдання в іншу чергу.
    Move {
        /// Ідентифікатор завдання зі `dl list`.
        id: i64,

        /// Назва черги призначення.
        #[arg(short = 'q', long)]
        queue: String,
    },

    /// Керування іменованими чергами завантажень.
    Queue {
        #[command(subcommand)]
        sub: Option<QueueCommand>,
    },
}

#[derive(Subcommand, Debug)]
enum QueueCommand {
    /// Показати список черг.
    List,

    /// Створити нову чергу.
    Add {
        /// Назва черги.
        name: String,

        /// Одночасних завантажень у черзі.
        #[arg(long)]
        max: Option<u32>,

        /// Ліміт швидкості черги у КБ/с. 0 — без обмежень.
        #[arg(long)]
        rate_kb: Option<u64>,

        /// Початок вікна старту завантажень (ГГ:ХХ).
        #[arg(long)]
        from: Option<String>,

        /// Кінець вікна старту завантажень (ГГ:ХХ).
        #[arg(long)]
        to: Option<String>,

        /// Післядія черги: `none`, `sleep` або `shutdown`.
        #[arg(long)]
        after: Option<String>,
    },

    /// Змінити налаштування черги.
    Set {
        /// Назва черги.
        name: String,

        /// Одночасних завантажень у черзі.
        #[arg(long)]
        max: Option<u32>,

        /// Ліміт швидкості черги у КБ/с. 0 — без обмежень.
        #[arg(long)]
        rate_kb: Option<u64>,

        /// Початок вікна старту завантажень (ГГ:ХХ).
        #[arg(long)]
        from: Option<String>,

        /// Кінець вікна старту завантажень (ГГ:ХХ).
        #[arg(long)]
        to: Option<String>,

        /// Післядія черги: `none`, `sleep` або `shutdown`.
        #[arg(long)]
        after: Option<String>,

        /// Прибрати розклад черги.
        #[arg(long)]
        clear_schedule: bool,
    },

    /// Зупинити всі завантаження черги.
    Pause {
        /// Назва черги.
        name: String,
    },

    /// Продовжити завантаження черги.
    Resume {
        /// Назва черги.
        name: String,
    },

    /// Перейменувати чергу.
    Rename {
        /// Стара назва черги.
        old: String,

        /// Нова назва черги.
        new: String,
    },

    /// Видалити чергу (завдання перейдуть у чергу "default").
    Rm {
        /// Назва черги.
        name: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    i18n::init(cli.lang.as_deref());
    let client = reqwest::Client::builder()
        .build()
        .context("не вдалося створити HTTP-клієнт")?;

    match cli.command {
        Command::Add {
            url,
            list,
            clipboard,
            out,
            parts,
            cookie,
            referer,
            queue,
        } => {
            let urls = зібрати_адреси(url, list, clipboard)?;
            let mut core = client::Client::connect().await?;
            let dest = if urls.len() == 1 {
                out.map(|p| p.display().to_string())
            } else {
                None
            };
            let session = Session::from_parts(cookie, referer);
            if session.cookies.is_some() {
                tracing::info!("cookie задано");
            }
            if session.referer.is_some() {
                tracing::info!("referer задано");
            }
            for url in urls {
                let resp = core
                    .call(&Request::Add {
                        url: url.clone(),
                        dest: dest.clone(),
                        parts,
                        cookies: session.cookies.clone(),
                        referer: session.referer.clone(),
                        variant: None,
                        queue: queue.clone(),
                    })
                    .await?;
                match resp {
                    Response::Added { id } => println!(
                        "{}",
                        t_pairs(
                            "task-accepted",
                            &[
                                ("id", id.to_string()),
                                ("url", url.clone()),
                            ]
                        )
                    ),
                    Response::Error { message, .. } => anyhow::bail!(message),
                    other => anyhow::bail!("несподівана відповідь ядра: {other:?}"),
                }
            }
        }

        Command::List => {
            let mut core = client::Client::connect().await?;

            match core.call(&Request::List).await? {
                Response::Tasks { tasks } if tasks.is_empty() => {
                    println!("{}", t("no-tasks"));
                }
                Response::Tasks { tasks } => {
                    for t in tasks {
                        let прогрес = t.progress().map_or_else(
                            || format_size(t.done),
                            |p| format!("{:.0}% ({})", p * 100.0, format_size(t.done)),
                        );
                        let q_display = if t.queue == "default" {
                            "default (типова)".to_owned()
                        } else {
                            t.queue
                        };
                        println!(
                            "{:>4}  {:<10} {:<16} {:>18}  {:>10}/с  {}",
                            t.id,
                            t.status,
                            q_display,
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

        Command::FfmpegInstall => {
            let куди = поставити_ffmpeg(&client).await?;
            println!("{}", t_pairs("ffmpeg-installed", &[("path", куди.display().to_string())]));
        }

        Command::Variants {
            url,
            cookie,
            referer,
        } => {
            let session = Session::from_parts(cookie, referer);
            let варіанти = варіанти_посилання(&url, session).await?;

            if варіанти.is_empty() {
                println!("{}", t("no-variants"));
            } else {
                for v in варіанти {
                    println!(
                        "{:>6}  {:<12} {:>10}  {}",
                        v.id,
                        v.label,
                        v.size.map_or_else(|| "—".to_owned(), format_size),
                        v.note.unwrap_or_default()
                    );
                }
            }
        }

        Command::Parts { id } => {
            let mut core = client::Client::connect().await?;

            match core.call(&Request::Details { id }).await? {
                Response::Details { parts, .. } if parts.is_empty() => {
                    println!("{}", t("no-layout"));
                }
                Response::Details { parts, .. } => {
                    for (i, p) in parts.iter().enumerate() {
                        let довжина = p.end.saturating_sub(p.start);
                        let частка = if довжина > 0 {
                            p.done as f64 / довжина as f64 * 100.0
                        } else {
                            100.0
                        };

                        println!(
                            "{:>3}  {:>14} … {:<14} {:>10} з {:>10}  {:>5.1}%  {}",
                            i,
                            p.start,
                            p.end,
                            format_size(p.done),
                            format_size(довжина),
                            частка,
                            смужка(частка),
                        );
                    }
                }
                Response::Error { message, .. } => anyhow::bail!(message),
                other => anyhow::bail!("несподівана відповідь ядра: {other:?}"),
            }
        }

        Command::Pause { id } => {
            let mut core = client::Client::connect().await?;
            core.call_ok(&Request::Pause { id }).await?;
            println!("{}", t_pairs("task-paused", &[("id", id.to_string())]));
        }

        Command::Resume { id } => {
            let mut core = client::Client::connect().await?;
            core.call_ok(&Request::Resume { id }).await?;
            println!("{}", t_pairs("task-resumed", &[("id", id.to_string())]));
        }

        Command::Rm { id, with_file } => {
            let mut core = client::Client::connect().await?;
            core.call_ok(&Request::Remove { id, with_file }).await?;
            if with_file {
                println!(
                    "{}",
                    t_pairs("task-removed-file", &[("id", id.to_string())])
                );
            } else {
                println!("{}", t_pairs("task-removed", &[("id", id.to_string())]));
            }
        }

        Command::Watch => {
            let core = client::Client::connect().await?;
            let mut events = core.subscribe().await?;

            println!("{}", t("watching"));

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
                        println!(
                            "✓ {}",
                            t_pairs(
                                "task-done",
                                &[
                                    ("id", id.to_string()),
                                    ("size", format_size(bytes)),
                                    ("path", path),
                                ]
                            )
                        );
                    }
                    Event::Failed { id, message } => {
                        println!(
                            "✗ {}",
                            t_pairs(
                                "task-failed",
                                &[
                                    ("id", id.to_string()),
                                    ("message", message),
                                ]
                            )
                        );
                    }
                }
            }

            println!("{}", t("core-closed"));
        }

        Command::Settings => {
            let mut core = client::Client::connect().await?;
            match core.call(&Request::Settings).await? {
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
                    println!(
                        "{}: {max_concurrent}",
                        t("set-max")
                    );
                    if rate_limit == 0 {
                        println!("{}: {}", t("set-rate"), t("set-unlimited"));
                    } else {
                        println!("{}: {} {}/с", t("set-rate"), rate_limit / 1024, t("kb"));
                    }
                    println!("{}: {}", t("set-after"), післядія_текст(&post_action));
                    match (schedule_from, schedule_to) {
                        (Some(a), Some(b)) => {
                            println!("{}: {a}–{b}", t("set-schedule"))
                        }
                        _ => println!("{}: {}", t("set-schedule"), t("set-always")),
                    }
                    match (quiet_from, quiet_to) {
                        (Some(a), Some(b)) if quiet_rate > 0 => println!(
                            "{}: {} {}/с {a}–{b}",
                            t("set-quiet"),
                            quiet_rate / 1024,
                            t("kb")
                        ),
                        _ => println!("{}: {}", t("set-quiet"), t("none")),
                    }
                }
                Response::Error { message, .. } => anyhow::bail!(message),
                other => anyhow::bail!("несподівана відповідь ядра: {other:?}"),
            }
        }

        Command::Configure {
            max,
            rate_kb,
            after,
            from,
            to,
            quiet_from,
            quiet_to,
            quiet_kb,
            clear_schedule,
            clear_quiet,
        } => {
            let mut core = client::Client::connect().await?;
            let schedule_from = if clear_schedule {
                Some(String::new())
            } else {
                from
            };
            let schedule_to = if clear_schedule {
                Some(String::new())
            } else {
                to
            };
            let quiet_from = if clear_quiet {
                Some(String::new())
            } else {
                quiet_from
            };
            let quiet_to = if clear_quiet {
                Some(String::new())
            } else {
                quiet_to
            };
            let quiet_rate = if clear_quiet {
                Some(0)
            } else {
                quiet_kb.map(|kb| kb.saturating_mul(1024))
            };
            core.call_ok(&Request::Configure {
                max_concurrent: max,
                rate_limit: rate_kb.map(|kb| kb.saturating_mul(1024)),
                post_action: after,
                schedule_from,
                schedule_to,
                quiet_from,
                quiet_to,
                quiet_rate,
            })
            .await?;
            println!("{}", t("set-applied"));
        }

        Command::YtdlpUpdate => {
            let yt = YtdlpProtocol::new();
            let text = yt.self_update().await?;
            let trimmed = text.trim();
            if trimmed.is_empty() {
                println!("{}", t("ytdlp-updated"));
            } else {
                println!("{trimmed}");
            }
        }

        Command::Move { id, queue } => {
            let mut core = client::Client::connect().await?;
            core.call_ok(&Request::MoveToQueue {
                id,
                queue: queue.clone(),
            })
            .await?;
            println!(
                "{}",
                t_pairs(
                    "task-moved",
                    &[("id", id.to_string()), ("queue", queue)]
                )
            );
        }

        Command::Queue { sub } => {
            let mut core = client::Client::connect().await?;
            match sub.unwrap_or(QueueCommand::List) {
                QueueCommand::List => {
                    match core.call(&Request::Queues).await? {
                        Response::Queues { queues } if queues.is_empty() => {
                            println!("{}", t("no-queues"));
                        }
                        Response::Queues { queues } => {
                            println!(
                                "{:>4}  {:<16} {:>6}  {:>14}  {:<14} {:<10} {:>6} {:>8}",
                                "ID",
                                "НАЗВА",
                                "СЛОТИ",
                                "ШВИДКІСТЬ",
                                "РОЗКЛАД",
                                "ПІСЛЯДІЯ",
                                "РАЗОМ",
                                "АКТИВНІ"
                            );
                            for q in queues {
                                let name_display = if q.paused {
                                    format!("{} [пауза]", q.name)
                                } else {
                                    q.name
                                };
                                let rate_display = if q.rate_limit == 0 {
                                    t("set-unlimited")
                                } else {
                                    format!("{} {}/с", q.rate_limit / 1024, t("kb"))
                                };
                                let sched_display = match (q.schedule_from, q.schedule_to) {
                                    (Some(a), Some(b)) => format!("{a}–{b}"),
                                    _ => t("set-always"),
                                };
                                println!(
                                    "{:>4}  {:<16} {:>6}  {:>14}  {:<14} {:<10} {:>6} {:>8}",
                                    q.id,
                                    name_display,
                                    q.max_concurrent,
                                    rate_display,
                                    sched_display,
                                    післядія_текст(&q.post_action),
                                    q.total_tasks,
                                    q.running_tasks,
                                );
                            }
                        }
                        Response::Error { message, .. } => anyhow::bail!(message),
                        other => anyhow::bail!("несподівана відповідь ядра: {other:?}"),
                    }
                }
                QueueCommand::Add {
                    name,
                    max,
                    rate_kb,
                    from,
                    to,
                    after,
                } => {
                    core.call_ok(&Request::QueueCreate {
                        name: name.clone(),
                        max_concurrent: max,
                        rate_limit: rate_kb.map(|kb| kb.saturating_mul(1024)),
                        schedule_from: from,
                        schedule_to: to,
                        post_action: after,
                    })
                    .await?;
                    println!("{}", t_pairs("queue-created", &[("name", name)]));
                }
                QueueCommand::Set {
                    name,
                    max,
                    rate_kb,
                    from,
                    to,
                    after,
                    clear_schedule,
                } => {
                    let schedule_from = if clear_schedule {
                        Some(String::new())
                    } else {
                        from
                    };
                    let schedule_to = if clear_schedule {
                        Some(String::new())
                    } else {
                        to
                    };
                    core.call_ok(&Request::QueueConfigure {
                        name: name.clone(),
                        max_concurrent: max,
                        rate_limit: rate_kb.map(|kb| kb.saturating_mul(1024)),
                        schedule_from,
                        schedule_to,
                        post_action: after,
                    })
                    .await?;
                    println!("{}", t_pairs("queue-configured", &[("name", name)]));
                }
                QueueCommand::Pause { name } => {
                    core.call_ok(&Request::QueuePause { name: name.clone() })
                        .await?;
                    println!("{}", t_pairs("queue-paused", &[("name", name)]));
                }
                QueueCommand::Resume { name } => {
                    core.call_ok(&Request::QueueResume { name: name.clone() })
                        .await?;
                    println!("{}", t_pairs("queue-resumed", &[("name", name)]));
                }
                QueueCommand::Rename { old, new } => {
                    core.call_ok(&Request::QueueRename {
                        old_name: old.clone(),
                        new_name: new.clone(),
                    })
                    .await?;
                    println!(
                        "{}",
                        t_pairs("queue-renamed", &[("old", old), ("new", new)])
                    );
                }
                QueueCommand::Rm { name } => {
                    core.call_ok(&Request::QueueDelete { name: name.clone() })
                        .await?;
                    println!("{}", t_pairs("queue-removed", &[("name", name)]));
                }
            }
        }

        Command::Probe { url } => {
            let hls = HlsProtocol::new()?;
            if hls.handles(&url) {
                показати_модуль(&hls, &url).await?;
            } else {
                let dash = DashProtocol::new()?;
                if dash.handles(&url) {
                    показати_модуль(&dash, &url).await?;
                } else {
                    let yt = YtdlpProtocol::new();
                    if yt.handles(&url) {
                        показати_модуль(&yt, &url).await?;
                    } else {
                let info = probe(&client, &url).await?;

                println!("адреса:      {}", info.final_url);
                println!(
                    "розмір:      {}",
                    info.size
                        .map_or_else(|| "невідомий".to_owned(), format_size)
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
                }
            }
        }

        Command::Get {
            url,
            out,
            parts,
            min_chunk,
            limit_kb,
            checkpoint_ms,
            cookie,
            referer,
            variant,
        } => {
            let urls = expand::розгорнути_шаблон(&url)?;
            if urls.len() != 1 {
                anyhow::bail!(
                    "шаблон дає {} адрес — для пакета скористайтесь `dl add`",
                    urls.len()
                );
            }
            let url = &urls[0];
            let session = Session::from_parts(cookie, referer);
            if session.cookies.is_some() {
                tracing::info!("cookie задано");
            }
            if session.referer.is_some() {
                tracing::info!("referer задано");
            }
            let hls = HlsProtocol::new()?;
            if hls.handles(url) {
                качати_модулем(&hls, url, out, limit_kb, session, variant).await?;
                return Ok(());
            }
            let dash = DashProtocol::new()?;
            if dash.handles(url) {
                качати_модулем(&dash, url, out, limit_kb, session, variant).await?;
                return Ok(());
            }
            let yt = YtdlpProtocol::new();
            if yt.handles(url) {
                качати_модулем(&yt, url, out, limit_kb, session, variant).await?;
                return Ok(());
            }
            let info = probe_with_session(&client, url, &session).await?;

            // Ім'я з мережі складала стороння людина: спершу знешкодити,
            // потім переконатись, що не затираємо чужий файл.
            let dest = match out {
                Some(p) => p,
                None => {
                    let raw = info
                        .filename()
                        .unwrap_or_else(|| names::ЗАПАСНЕ_ІМʼЯ.to_owned());
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
                session,
                ..Options::default()
            };

            println!(
                "качаю {} → {} ({}, {} потоків)",
                info.final_url,
                dest.display(),
                info.size
                    .map_or_else(|| "розмір невідомий".to_owned(), format_size),
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
                Err(e) => println!(
                    "⚠ {}",
                    t_pairs("motw-fail", &[("error", e.to_string())])
                ),
            }
        }
    }

    Ok(())
}

/// Зібрати адреси з аргумента, файла-списку або буфера обміну й розгорнути шаблони.
fn зібрати_адреси(
    url: Option<String>,
    list: Option<PathBuf>,
    clipboard: bool,
) -> Result<Vec<String>> {
    let mut з_пакета = Vec::new();
    if clipboard {
        з_пакета.push(текст_буфера()?);
    }
    if let Some(path) = list {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("не прочитати список {}", path.display()))?;
        з_пакета.push(text);
    }
    let mut out = Vec::new();
    for блок in з_пакета {
        for line in блок.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if !expand::з_масового_джерела(line) {
                continue;
            }
            out.extend(expand::розгорнути_шаблон(line)?);
        }
    }
    if let Some(u) = url {
        out.extend(expand::розгорнути_шаблон(u.trim())?);
    }
    if out.is_empty() {
        bail!("{}", t("need-url"));
    }
    Ok(expand::унікальні_порядком(out))
}

/// Прогрес у консолі не малюємо: HTTP і так друкує підсумок, HLS — список файлів.
struct НімийПрогрес;

impl ProgressSink for НімийПрогрес {
    fn report(&self, _progress: Progress) {}
}

/// `dl probe` для HLS/DASH: файли й що обрано, без качання.
async fn показати_модуль(p: &dyn Protocol, url: &str) -> Result<()> {
    let probed = p.probe(url).await?;
    println!("адреса:      {}", probed.final_url);
    println!(
        "розмір:      {}",
        probed
            .total_size
            .map_or_else(|| "невідомий".to_owned(), format_size)
    );
    println!(
        "сегменти:    {}",
        if probed.resumable {
            "можна різати й докачувати"
        } else {
            "тільки один потік, докачування недоступне"
        }
    );
    if probed.files.is_empty() {
        println!("файли:       немає");
        return Ok(());
    }
    for f in &probed.files {
        let стан = if f.selected { "обрано" } else { "пропущено" };
        let розмір = f
            .size
            .map_or_else(|| "розмір невідомий".to_owned(), format_size);
        println!("файл:        {} ({стан}, {розмір})", f.suggested_name);
    }
    Ok(())
}

/// `dl get` для HLS/DASH: probe → шляхи для selected → `run` → MotW.
async fn качати_модулем(
    p: &dyn Protocol,
    url: &str,
    out: Option<PathBuf>,
    limit_kb: u64,
    session: Session,
    варіант: Option<String>,
) -> Result<()> {
    if limit_kb > 0 {
        match p.set_rate_limit(limit_kb.saturating_mul(1024)) {
            RateLimitSupport::Applied => {}
            RateLimitSupport::Unsupported => {
                println!("⚠ {}", t("rate-unsupported"));
            }
        }
    }
    p.set_session(session.clone());

    // Проба з урахуванням вибору: 720p важить не стільки, скільки 360p, а
    // «лише аудіо» дає інше розширення — склад файлів залежить від варіанта.
    let probed = p.probe_variant(url, варіант.as_deref()).await?;
    for f in probed.files.iter().filter(|f| f.selected) {
        let safe = names::sanitize(&f.suggested_name);
        if safe != f.suggested_name {
            println!(
                "ім'я з сервера знешкоджено: {:?} → {safe:?}",
                f.suggested_name
            );
        }
    }

    let dests = шляхи_для_обраних(out, &probed.files)?;
    println!(
        "качаю {} → {} ({} файл{})",
        probed.final_url,
        dests
            .first()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "—".to_owned()),
        dests.len(),
        if dests.len() == 1 { "" } else { "ів" }
    );
    for dest in dests.iter().skip(1) {
        println!("           → {}", dest.display());
    }

    let started = std::time::Instant::now();
    let ctx = RunContext {
        task_id: 0,
        source: url.to_owned(),
        targets: dests.clone(),
        resume: None,
        cancel: Cancel::new(),
        session,
        variant: варіант,
        limiter: None,
    };
    if p.run(ctx, &НімийПрогрес).await?.is_some() {
        println!("{}", t("stopped-early"));
    }

    let secs = started.elapsed().as_secs_f64().max(0.001);
    let mut bytes = 0u64;
    for dest in &dests {
        match std::fs::metadata(dest) {
            Ok(m) => {
                bytes += m.len();
                match motw::mark(dest, Some(url), None) {
                    Ok(()) => {}
                    Err(e) => {
                        println!(
                            "⚠ {}",
                            t_pairs("motw-fail", &[("error", e.to_string())])
                        );
                    }
                }
                println!("файл:   {}", dest.display());
            }
            Err(e) => println!("⚠ немає файла {}: {e}", dest.display()),
        }
    }
    println!(
        "готово: {} за {:.1} с ({}/с)",
        format_size(bytes),
        secs,
        format_size((bytes as f64 / secs) as u64)
    );
    Ok(())
}

/// Шляхи запису для обраних файлів probe.
///
/// Один обраний і `-o` — це він. Кілька: `-o` тека (якщо існує) або ім'я
/// першого, решта `suggested_name` поруч.
fn шляхи_для_обраних(out: Option<PathBuf>, files: &[PlannedFile]) -> Result<Vec<PathBuf>> {
    let обрані: Vec<&PlannedFile> = files.iter().filter(|f| f.selected).collect();
    if обрані.is_empty() {
        bail!("{}", t("need-selected"));
    }

    match out {
        Some(p) if обрані.len() == 1 => Ok(vec![p]),
        Some(p) if p.is_dir() => Ok(обрані
            .iter()
            .map(|f| paths::unique_path(&p.join(names::sanitize(&f.suggested_name))))
            .collect()),
        Some(p) => {
            let тека = match p.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
                _ => PathBuf::from("."),
            };
            let mut dests = Vec::with_capacity(обрані.len());
            dests.push(p);
            for f in обрані.iter().skip(1) {
                dests.push(paths::unique_path(
                    &тека.join(names::sanitize(&f.suggested_name)),
                ));
            }
            Ok(dests)
        }
        None => Ok(обрані
            .iter()
            .map(|f| paths::unique_path(&PathBuf::from(names::sanitize(&f.suggested_name))))
            .collect()),
    }
}

/// Розмір у зрозумілому вигляді.
/// Звідки беремо ffmpeg.
///
/// ⚠️ Саме **LGPL**-збірка, без `enable-gpl`. GPL-варіант зобов'язав би нас
/// відкрити власний код — а тут ми поширюємо ffmpeg разом із програмою.
const FFMPEG_URL: &str = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip";

/// Завантажити й поставити ffmpeg поруч із програмою.
///
/// Качаємо **власним рушієм**: сегментовано, з докачуванням. Менеджеру
/// завантажень личить користуватися собою, та й перевірка виходить
/// безкоштовна — якщо тут щось не працює, то не працює й головна функція.
async fn поставити_ffmpeg(client: &reqwest::Client) -> Result<PathBuf> {
    let поруч = std::env::current_exe()?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("не визначити теку програми"))?
        .to_path_buf();

    let тимчасова = std::env::temp_dir().join("downloader-ffmpeg");
    std::fs::create_dir_all(&тимчасова)?;
    let архів = тимчасова.join("ffmpeg.zip");

    println!("{}", t("ffmpeg-downloading"));

    let info = probe(client, FFMPEG_URL).await?;
    let opts = Options {
        parts: 8,
        ..Options::default()
    };
    download_with_probe(client, &info, &архів, &opts).await?;

    println!("{}", t("ffmpeg-unpacking"));
    розпакувати(&архів, &тимчасова)?;

    // Усередині архіву тека з версією, а в ній `bin`.
    let bin = знайти_bin(&тимчасова)
        .ok_or_else(|| anyhow::anyhow!("в архіві немає теки bin: {}", тимчасова.display()))?;

    for запис in std::fs::read_dir(&bin)? {
        let запис = запис?;
        let імʼя = запис.file_name();
        let імʼя = імʼя.to_string_lossy();

        // ffplay — програвач; нам потрібне лише зведення доріжок.
        if імʼя.eq_ignore_ascii_case("ffplay.exe") {
            continue;
        }

        std::fs::copy(запис.path(), поруч.join(запис.file_name()))?;
    }

    // Ліцензію кладемо поруч обов'язково: LGPL цього вимагає.
    if let Some(ліцензія) = знайти_ліцензію(&тимчасова) {
        std::fs::copy(ліцензія, поруч.join("LICENSE-ffmpeg.txt"))?;
    }

    let _ = std::fs::remove_dir_all(&тимчасова);

    let exe = поруч.join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" });
    if !exe.is_file() {
        anyhow::bail!("після розпакування ffmpeg не з'явився: {}", exe.display());
    }

    Ok(exe)
}

/// Розпакувати zip системним архіватором.
///
/// Windows 10+ несе bsdtar у System32, і він розуміє zip. Своя реалізація
/// zip заради одного розпакування на все життя програми — зайва вага й
/// зайвий код, який доведеться супроводжувати.
fn розпакувати(архів: &Path, куди: &Path) -> Result<()> {
    let tar = if cfg!(windows) {
        std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:\\Windows"))
            .join("System32")
            .join("tar.exe")
    } else {
        PathBuf::from("tar")
    };

    let out = std::process::Command::new(&tar)
        .current_dir(куди)
        .arg("-xf")
        .arg(архів)
        .output()
        .map_err(|e| anyhow::anyhow!("не запустити {}: {e}", tar.display()))?;

    if !out.status.success() {
        anyhow::bail!(
            "розпакування не вдалося: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    Ok(())
}

fn знайти_bin(корінь: &Path) -> Option<PathBuf> {
    for запис in std::fs::read_dir(корінь).ok()? {
        let шлях = запис.ok()?.path();
        if !шлях.is_dir() {
            continue;
        }
        let bin = шлях.join("bin");
        if bin.is_dir() {
            return Some(bin);
        }
    }
    None
}

fn знайти_ліцензію(корінь: &Path) -> Option<PathBuf> {
    for запис in std::fs::read_dir(корінь).ok()? {
        let шлях = запис.ok()?.path();
        if !шлях.is_dir() {
            continue;
        }
        let l = шлях.join("LICENSE.txt");
        if l.is_file() {
            return Some(l);
        }
    }
    None
}

/// Варіанти якості для посилання — тим самим модулем, який його й качатиме.
///
/// Ядро тут не потрібне: проба нічого не змінює, і людина має бачити перелік
/// ще до того, як щось додала.
async fn варіанти_посилання(
    url: &str,
    session: Session,
) -> Result<Vec<downloader_core::protocol::Variant>> {
    let hls = HlsProtocol::new()?;
    if hls.handles(url) {
        hls.set_session(session);
        return Ok(hls.probe(url).await?.variants);
    }

    let dash = DashProtocol::new()?;
    if dash.handles(url) {
        dash.set_session(session);
        return Ok(dash.probe(url).await?.variants);
    }

    let yt = YtdlpProtocol::new();
    if yt.handles(url) {
        yt.set_session(session);
        return Ok(yt.probe(url).await?.variants);
    }

    // Звичайний файл має один вигляд — і це не помилка, а відповідь.
    Ok(Vec::new())
}

/// Смужка виконаного для консолі.
///
/// У консолі немає кольору, на який можна покластися, тож частка малюється
/// знаками: інакше двадцять рядків чисел не читаються з першого погляду.
fn смужка(частка: f64) -> String {
    const ШИРИНА: usize = 20;
    let повних = ((частка / 100.0).clamp(0.0, 1.0) * ШИРИНА as f64).round() as usize;
    format!("[{}{}]", "█".repeat(повних), "·".repeat(ШИРИНА - повних))
}

fn післядія_текст(raw: &str) -> String {
    match raw {
        "sleep" => t("set-sleep"),
        "shutdown" => t("set-shutdown"),
        _ => t("none"),
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn clap_розбирає_pause_resume_rm() {
        Cli::command().debug_assert();

        match Cli::try_parse_from(["dl", "pause", "7"]).expect("pause") {
            Cli {
                command: Command::Pause { id },
                ..
            } => assert_eq!(id, 7),
            other => panic!("не pause: {other:?}"),
        }

        match Cli::try_parse_from(["dl", "resume", "7"]).expect("resume") {
            Cli {
                command: Command::Resume { id },
                ..
            } => assert_eq!(id, 7),
            other => panic!("не resume: {other:?}"),
        }

        match Cli::try_parse_from(["dl", "rm", "7", "--with-file"]).expect("rm") {
            Cli {
                command: Command::Rm { id, with_file },
                ..
            } => {
                assert_eq!(id, 7);
                assert!(with_file);
            }
            other => panic!("не rm: {other:?}"),
        }

        match Cli::try_parse_from(["dl", "rm", "3"]).expect("rm без файла") {
            Cli {
                command: Command::Rm { id, with_file },
                ..
            } => {
                assert_eq!(id, 3);
                assert!(!with_file);
            }
            other => panic!("не rm: {other:?}"),
        }

        match Cli::try_parse_from(["dl", "add", "--clipboard"]).expect("clipboard") {
            Cli {
                command: Command::Add {
                    clipboard: true,
                    url: None,
                    list: None,
                    ..
                },
                ..
            } => {}
            other => panic!("не add --clipboard: {other:?}"),
        }

        match Cli::try_parse_from(["dl", "add", "http://ex.com/a[001-002].zip"]).expect("шаблон") {
            Cli {
                command: Command::Add {
                    url: Some(u),
                    clipboard: false,
                    ..
                },
                ..
            } => assert!(u.contains("[001-002]")),
            other => panic!("не add шаблон: {other:?}"),
        }

        match Cli::try_parse_from([
            "dl",
            "add",
            "http://ex.com/a.bin",
            "--cookie",
            "n=v; n2=v2",
            "--referer",
            "http://ex.com/",
        ])
        .expect("add cookie")
        {
            Cli {
                command: Command::Add {
                    cookie,
                    referer,
                    ..
                },
                ..
            } => {
                assert_eq!(cookie.as_deref(), Some("n=v; n2=v2"));
                assert_eq!(referer.as_deref(), Some("http://ex.com/"));
            }
            other => panic!("не add --cookie: {other:?}"),
        }

        match Cli::try_parse_from([
            "dl",
            "get",
            "http://ex.com/a.bin",
            "--cookie",
            "session=ok",
            "--referer",
            "http://ex.com/page",
        ])
        .expect("get cookie")
        {
            Cli {
                command: Command::Get {
                    cookie,
                    referer,
                    ..
                },
                ..
            } => {
                assert_eq!(cookie.as_deref(), Some("session=ok"));
                assert_eq!(referer.as_deref(), Some("http://ex.com/page"));
            }
            other => panic!("не get --cookie: {other:?}"),
        }

        match Cli::try_parse_from(["dl", "settings"]).expect("settings") {
            Cli {
                command: Command::Settings,
                ..
            } => {}
            other => panic!("не settings: {other:?}"),
        }

        match Cli::try_parse_from([
            "dl",
            "configure",
            "--max",
            "5",
            "--after",
            "sleep",
            "--from",
            "22:00",
            "--to",
            "07:00",
        ])
        .expect("configure")
        {
            Cli {
                command: Command::Configure {
                    max,
                    after,
                    from,
                    to,
                    clear_schedule,
                    ..
                },
                ..
            } => {
                assert_eq!(max, Some(5));
                assert_eq!(after.as_deref(), Some("sleep"));
                assert_eq!(from.as_deref(), Some("22:00"));
                assert_eq!(to.as_deref(), Some("07:00"));
                assert!(!clear_schedule);
            }
            other => panic!("не configure: {other:?}"),
        }

        match Cli::try_parse_from(["dl", "ytdlp-update"]).expect("ytdlp-update") {
            Cli {
                command: Command::YtdlpUpdate,
                ..
            } => {}
            other => panic!("не ytdlp-update: {other:?}"),
        }

        match Cli::try_parse_from(["dl", "add", "http://ex.com/a.bin", "-q", "nightly"]).expect("add queue") {
            Cli {
                command: Command::Add { queue, .. },
                ..
            } => assert_eq!(queue.as_deref(), Some("nightly")),
            other => panic!("не add -q: {other:?}"),
        }

        match Cli::try_parse_from(["dl", "move", "42", "-q", "fast"]).expect("move") {
            Cli {
                command: Command::Move { id, queue },
                ..
            } => {
                assert_eq!(id, 42);
                assert_eq!(queue, "fast");
            }
            other => panic!("не move: {other:?}"),
        }

        match Cli::try_parse_from(["dl", "queue", "add", "fast", "--max", "3", "--rate-kb", "1024"]).expect("queue add") {
            Cli {
                command: Command::Queue { sub: Some(QueueCommand::Add { name, max, rate_kb, .. }) },
                ..
            } => {
                assert_eq!(name, "fast");
                assert_eq!(max, Some(3));
                assert_eq!(rate_kb, Some(1024));
            }
            other => panic!("не queue add: {other:?}"),
        }

        match Cli::try_parse_from(["dl", "queue", "pause", "fast"]).expect("queue pause") {
            Cli {
                command: Command::Queue { sub: Some(QueueCommand::Pause { name }) },
                ..
            } => assert_eq!(name, "fast"),
            other => panic!("не queue pause: {other:?}"),
        }
    }

    fn planned(name: &str, selected: bool) -> PlannedFile {
        PlannedFile {
            suggested_name: name.to_owned(),
            size: None,
            selected,
        }
    }

    #[test]
    fn youtube_впізнає_ytdlp_не_http() {
        let p = YtdlpProtocol::new();
        assert!(p.handles("https://www.youtube.com/watch?v=abc"));
        assert!(p.handles("https://youtu.be/abc"));
        assert!(!p.handles("https://cdn.example/video.mp4"));
    }

    #[test]
    fn hls_впізнає_маніфест_а_не_звичайний_файл() {
        let p = HlsProtocol::new().expect("hls");
        assert!(p.handles("https://cdn.example/a/master.m3u8"));
        assert!(p.handles("http://127.0.0.1/x.m3u8?token=1"));
        assert!(
            !p.handles("https://cdn.example/video.mp4"),
            "звичайний HTTP не має перехоплювати HLS"
        );
    }

    #[test]
    fn шляхи_один_selected_бере_out() {
        let files = [planned("a.ts", true), planned("b.ts", false)];
        let out = PathBuf::from("movie.ts");
        let got = шляхи_для_обраних(Some(out.clone()), &files).expect("один");
        assert_eq!(got, vec![out]);
    }

    #[test]
    fn шляхи_кілька_selected_out_це_імʼя_першого() {
        let dir = std::env::temp_dir().join(format!("dl-e11-first-{}", std::process::id()));
        let files = [planned("a.ts", true), planned("b.ts", true)];
        let out = dir.join("movie.ts");
        let got = шляхи_для_обраних(Some(out.clone()), &files).expect("кілька");
        assert_eq!(got[0], out);
        assert_eq!(got[1], dir.join("b.ts"));
    }

    #[test]
    fn шляхи_кілька_selected_out_тека() {
        let dir = std::env::temp_dir().join(format!(
            "dl-e11-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("тека");
        let files = [planned("a.ts", true), planned("b.ts", true)];
        let got = шляхи_для_обраних(Some(dir.clone()), &files).expect("тека");
        assert_eq!(got[0], dir.join("a.ts"));
        assert_eq!(got[1], dir.join("b.ts"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn шляхи_без_обраних_це_помилка() {
        let files = [planned("a.ts", false)];
        let err = шляхи_для_обраних(None, &files).expect_err("порожнє");
        assert!(err.to_string().contains("обраного"));
    }

    #[tokio::test]
    async fn hls_get_vod_склеює_сегменти() {
        let server = downloader_testserver::EvilServer::start()
            .await
            .expect("стенд");
        let url = server.url("/hls/vod/media.m3u8");
        let hls = HlsProtocol::new().expect("hls");
        assert!(hls.handles(&url), "media.m3u8 має йти в HLS, не в HTTP");

        let dir = std::env::temp_dir().join(format!(
            "dl-e11-vod-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("тека");
        let dest = dir.join("out.ts");
        качати_модулем(&hls, &url, Some(dest.clone()), 0, Session::default(), None)
            .await
            .expect("get");

        let got = std::fs::read(&dest).expect("прочитати");
        let mut expect = Vec::new();
        expect.extend_from_slice(b"SEG0-PAYLOAD-AAAAAAAAAAAAAAAA");
        expect.extend_from_slice(b"SEG1-PAYLOAD-BBBBBBBBBBBBBBBB");
        assert_eq!(got, expect, "склейка сегментів не збіглась");
        server.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dash_get_vod_склеює_сегменти() {
        let server = downloader_testserver::EvilServer::start()
            .await
            .expect("стенд");
        let url = server.url("/dash/vod/manifest.mpd");
        let dash = DashProtocol::new().expect("dash");
        assert!(dash.handles(&url), "mpd має йти в DASH, не в HTTP");

        let dir = std::env::temp_dir().join(format!(
            "dl-e17-dash-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("тека");
        let dest = dir.join("out.mp4");
        качати_модулем(&dash, &url, Some(dest.clone()), 0, Session::default(), None)
            .await
            .expect("get");

        let got = std::fs::read(&dest).expect("прочитати");
        assert!(
            got.starts_with(b"INIT-PAYLOAD-DASH-AAAAAAAAAA"),
            "мав початись з init"
        );
        server.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
