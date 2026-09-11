//! Іконка в системному треї. Це ядро, не вікно: закрити «вікно» тут
//! неможливо — є лише меню. Тести з `--pipe` трей не піднімають.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use downloader_core::protocol::Session;
use downloader_winutil::текст_буфера;
use tokio::sync::watch;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};

use crate::clipboard_watch::витягти_http;
use crate::engine::Engine;

/// Поставити іконку в окремому потоці. Повертає канал «вийти».
pub fn старт(
    engine: Arc<Engine>,
    downloads: PathBuf,
) -> anyhow::Result<watch::Receiver<bool>> {
    let (tx, rx) = watch::channel(false);
    let handle = tokio::runtime::Handle::current();

    thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            if let Err(e) = цикл(engine, downloads, tx, handle) {
                tracing::warn!(error = %e, "трей не піднявся");
            }
        })?;

    Ok(rx)
}

fn цикл(
    engine: Arc<Engine>,
    downloads: PathBuf,
    tx: watch::Sender<bool>,
    rt: tokio::runtime::Handle,
) -> anyhow::Result<()> {
    let відкрити = MenuItem::with_id("open", "Відкрити теку завантажень", true, None);
    let буфер = MenuItem::with_id("clip", "Додати з буфера обміну", true, None);
    let вихід = MenuItem::with_id("quit", "Вийти", true, None);
    let menu = Menu::new();
    menu.append(&відкрити)?;
    menu.append(&буфер)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&вихід)?;

    let icon = іконка()?;
    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("Downloader")
        .with_icon(icon)
        .build()?;

    let menu_rx = MenuEvent::receiver();
    let _tray_rx = TrayIconEvent::receiver();

    loop {
        if let Ok(ev) = menu_rx.try_recv() {
            let id = ev.id.0.as_str();
            match id {
                "open" => відкрити_теку(&downloads),
                "clip" => {
                    let engine = Arc::clone(&engine);
                    rt.spawn(async move {
                        if let Err(e) = додати_з_буфера(engine).await {
                            tracing::warn!(error = %e, "не додати з буфера");
                        }
                    });
                }
                "quit" => {
                    if tx.send(true).is_err() {
                        return Ok(());
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn іконка() -> anyhow::Result<Icon> {
    const N: u32 = 32;
    let mut rgba = vec![0u8; (N * N * 4) as usize];
    for px in rgba.chunks_exact_mut(4) {
        px[0] = 196;
        px[1] = 92;
        px[2] = 38;
        px[3] = 255;
    }
    Icon::from_rgba(rgba, N, N).map_err(|e| anyhow::anyhow!("іконка трею: {e}"))
}

fn відкрити_теку(dir: &std::path::Path) {
    let r = if cfg!(windows) {
        std::process::Command::new("explorer").arg(dir).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(dir).spawn()
    };
    if let Err(e) = r {
        tracing::warn!(error = %e, "не відкрити теку завантажень");
    }
}

async fn додати_з_буфера(engine: Arc<Engine>) -> anyhow::Result<()> {
    let text = текст_буфера()?;
    let urls = витягти_http(&text);
    if urls.is_empty() {
        anyhow::bail!("у буфері немає http(s) адреси");
    }
    for url in urls {
        match engine.add(&url, None, None, Session::default()).await {
            Ok(id) => tracing::info!(id, "з буфера"),
            Err(e) => tracing::warn!(error = %e, "буфер: не додати"),
        }
    }
    Ok(())
}
