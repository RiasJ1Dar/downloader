//! Командний рядок менеджера завантажень.
//!
//! Перша оболонка ядра й постійний тестовий стенд. Усе, що вміє вікно,
//! спершу має вміти CLI — інакше логіка непомітно заповзає в UI, і потім
//! її не витягнути.
//!
//! На Ф3 CLI перейде на IPC і стане клієнтом ядра-сервісу, рівно таким
//! самим, яким потім буде вікно.

mod client;
mod expand;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use downloader_core::protocol::{
    Cancel, PlannedFile, Progress, ProgressSink, Protocol, RateLimitSupport, RunContext,
};
use downloader_ipc::protocol::{Event, Request, Response};
use downloader_proto_hls::HlsProtocol;
use downloader_proto_http::download::{Options, download_with_probe};
use downloader_proto_http::probe::probe;
use downloader_winutil::{motw, names, paths, текст_буфера};

#[derive(Parser, Debug)]
#[command(
    name = "dl",
    version,
    about = "Менеджер завантажень: сегментоване качання з докачуванням"
)]
struct Cli {
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
    },

    /// Показати завдання ядра.
    List,

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
        } => {
            let urls = зібрати_адреси(url, list, clipboard)?;
            let mut core = client::Client::connect().await?;
            let dest = if urls.len() == 1 {
                out.map(|p| p.display().to_string())
            } else {
                None
            };
            for url in urls {
                let resp = core
                    .call(&Request::Add {
                        url: url.clone(),
                        dest: dest.clone(),
                        parts,
                    })
                    .await?;
                match resp {
                    Response::Added { id } => println!("завдання {id} прийнято ядром: {url}"),
                    Response::Error { message, .. } => anyhow::bail!(message),
                    other => anyhow::bail!("несподівана відповідь ядра: {other:?}"),
                }
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

        Command::Pause { id } => {
            let mut core = client::Client::connect().await?;
            core.call_ok(&Request::Pause { id }).await?;
            println!("завдання {id} зупиняється");
        }

        Command::Resume { id } => {
            let mut core = client::Client::connect().await?;
            core.call_ok(&Request::Resume { id }).await?;
            println!("завдання {id} продовжено");
        }

        Command::Rm { id, with_file } => {
            let mut core = client::Client::connect().await?;
            core.call_ok(&Request::Remove { id, with_file }).await?;
            if with_file {
                println!("завдання {id} прибрано разом із файлом");
            } else {
                println!("завдання {id} прибрано зі списку");
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
            let hls = HlsProtocol::new()?;
            if hls.handles(&url) {
                показати_hls(&hls, &url).await?;
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

        Command::Get {
            url,
            out,
            parts,
            min_chunk,
            limit_kb,
            checkpoint_ms,
        } => {
            let urls = expand::розгорнути_шаблон(&url)?;
            if urls.len() != 1 {
                anyhow::bail!(
                    "шаблон дає {} адрес — для пакета скористайтесь `dl add`",
                    urls.len()
                );
            }
            let url = &urls[0];
            let hls = HlsProtocol::new()?;
            if hls.handles(url) {
                качати_hls(&hls, url, out, limit_kb).await?;
                return Ok(());
            }
            let info = probe(&client, url).await?;

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
                Err(e) => println!("⚠ не вдалося позначити файл як завантажений з мережі: {e}"),
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
        bail!("вкажіть посилання, --list або --clipboard");
    }
    Ok(expand::унікальні_порядком(out))
}

/// Прогрес у консолі не малюємо: HTTP і так друкує підсумок, HLS — список файлів.
struct НімийПрогрес;

impl ProgressSink for НімийПрогрес {
    fn report(&self, _progress: Progress) {}
}

/// `dl probe` для маніфеста HLS: файли й що обрано, без качання.
async fn показати_hls(hls: &HlsProtocol, url: &str) -> Result<()> {
    let probed = hls.probe(url).await?;
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

/// `dl get` для HLS: probe → шляхи для selected → `run` → MotW на записане.
async fn качати_hls(
    hls: &HlsProtocol,
    url: &str,
    out: Option<PathBuf>,
    limit_kb: u64,
) -> Result<()> {
    if limit_kb > 0 {
        match hls.set_rate_limit(limit_kb.saturating_mul(1024)) {
            RateLimitSupport::Applied => {}
            RateLimitSupport::Unsupported => {
                println!("⚠ ліміт швидкості цей протокол не вміє застосувати");
            }
        }
    }

    let probed = hls.probe(url).await?;
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
    };
    if hls.run(ctx, &НімийПрогрес).await?.is_some() {
        println!("завантаження HLS зупинено до завершення");
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
                        println!("⚠ не вдалося позначити файл як завантажений з мережі: {e}");
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
        bail!("HLS-проба не дала жодного обраного файла");
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
            } => assert_eq!(id, 7),
            other => panic!("не pause: {other:?}"),
        }

        match Cli::try_parse_from(["dl", "resume", "7"]).expect("resume") {
            Cli {
                command: Command::Resume { id },
            } => assert_eq!(id, 7),
            other => panic!("не resume: {other:?}"),
        }

        match Cli::try_parse_from(["dl", "rm", "7", "--with-file"]).expect("rm") {
            Cli {
                command: Command::Rm { id, with_file },
            } => {
                assert_eq!(id, 7);
                assert!(with_file);
            }
            other => panic!("не rm: {other:?}"),
        }

        match Cli::try_parse_from(["dl", "rm", "3"]).expect("rm без файла") {
            Cli {
                command: Command::Rm { id, with_file },
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
            } => assert!(u.contains("[001-002]")),
            other => panic!("не add шаблон: {other:?}"),
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
        качати_hls(&hls, &url, Some(dest.clone()), 0)
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
}
