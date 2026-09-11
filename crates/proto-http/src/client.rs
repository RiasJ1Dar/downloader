//! Спільний HTTP-клієнт для модулів: UA і проксі.

use std::time::Duration;

use downloader_core::error::{Error, Result};
use reqwest::{Client, Proxy};

/// Ідентифікація в мережі. Без цього частина CDN віддає 403.
pub const USER_AGENT: &str = "Downloader/0.1";

/// Зібрати клієнт: UA, таймаут з'єднання, проксі з оточення.
pub fn зібрати_клієнт() -> Result<Client> {
    let mut b = Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(20));

    match проксі_з_оточення() {
        ProxyChoice::None => {
            b = b.no_proxy();
        }
        ProxyChoice::Url(url) => {
            let p = Proxy::all(&url).map_err(|e| {
                Error::Store(format!("невірний DOWNLOADER_PROXY: {e}"))
            })?;
            b = b.proxy(p);
        }
        ProxyChoice::Default => {}
    }

    b.build()
        .map_err(|e| Error::Store(format!("не вдалося створити HTTP-клієнт: {e}")))
}

enum ProxyChoice {
    /// `DOWNLOADER_PROXY=none` — явно без проксі.
    None,
    /// URL з `DOWNLOADER_PROXY` або `HTTPS_PROXY`/`HTTP_PROXY`.
    Url(String),
    /// Як у reqwest за замовчуванням.
    Default,
}

fn проксі_з_оточення() -> ProxyChoice {
    if let Ok(p) = std::env::var("DOWNLOADER_PROXY") {
        let p = p.trim().to_owned();
        if p.is_empty() || p.eq_ignore_ascii_case("none") {
            return ProxyChoice::None;
        }
        return ProxyChoice::Url(p);
    }
    if let Ok(p) = std::env::var("HTTPS_PROXY").or_else(|_| std::env::var("HTTP_PROXY")) {
        let p = p.trim().to_owned();
        if !p.is_empty() {
            return ProxyChoice::Url(p);
        }
    }
    ProxyChoice::Default
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_це_вимкнений_проксі() {
        // Сама функція читає env процесу — тут лише збір клієнта не падає.
        let c = зібрати_клієнт();
        assert!(c.is_ok(), "{c:?}");
    }

    #[test]
    fn ua_не_порожній() {
        assert!(USER_AGENT.starts_with("Downloader/"));
    }
}
