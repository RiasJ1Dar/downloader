//! Опитування буфера обміну. Без HWND: sequence/текст раз на 500 мс.
//!
//! Завдання **не** додаються самі — лише журнал. Увімкнення:
//! `DOWNLOADER_WATCH_CLIPBOARD=1`.

use std::collections::HashSet;

const СМІТТЯ: &[&str] = &["w3.org/", "xmlns", "schema.org"];

/// http(s) з тексту, без junk, унікальні в порядку появи.
#[must_use]
pub fn витягти_http(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for raw in text.split(|c: char| c.is_whitespace()) {
        let line = raw.trim();
        if !(line.starts_with("http://") || line.starts_with("https://")) {
            continue;
        }
        if СМІТТЯ.iter().any(|j| line.contains(j)) {
            continue;
        }
        if seen.insert(line.to_owned()) {
            out.push(line.to_owned());
        }
    }
    out
}

fn хост(url: &str) -> &str {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(url)
}

/// Цикл, поки процес живе.
pub async fn run() {
    tracing::info!("слухач буфера ввімкнено (DOWNLOADER_WATCH_CLIPBOARD)");
    let mut prev = String::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
    loop {
        tick.tick().await;
        let now = match downloader_winutil::текст_буфера() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if now == prev {
            continue;
        }
        prev.clone_from(&now);
        let urls = витягти_http(&now);
        if urls.is_empty() {
            continue;
        }
        let hosts: Vec<&str> = urls.iter().map(|u| хост(u)).collect();
        tracing::info!(n = urls.len(), hosts = ?hosts, "буфер: нові http-адреси");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn витягає_http_і_ріже_сміття() {
        let t = "see https://cdn.example/a.zip and https://www.w3.org/1999/xhtml ftp://x";
        let got = витягти_http(t);
        assert_eq!(got, vec!["https://cdn.example/a.zip"]);
    }

    #[test]
    fn дубль_лишає_перший() {
        let t = "https://a/x https://a/x https://b/y";
        assert_eq!(витягти_http(t), vec!["https://a/x", "https://b/y"]);
    }
}
