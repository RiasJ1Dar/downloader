//! Ядро менеджера завантажень.
//!
//! Крейт **не знає про UI** і **не згадує жодного протоколу на ім'я**:
//! HTTP, HLS, DASH і майбутній торент приходять сюди через контракт
//! `Protocol`. Це те, що дозволяє доточити торент без переписування.
//!
//! Ядро живе в окремому процесі (`downloader-core.exe`); UI, CLI і
//! native messaging host — його клієнти по локальному IPC.

pub mod error;
pub mod file;
pub mod post_action;
pub mod protocol;
pub mod rate;
pub mod schedule;
pub mod segments;
pub mod state;
pub mod store;
pub mod verify;

pub use error::{Error, Result};
pub use file::SparseFile;
pub use post_action::PostAction;
pub use protocol::{Protocol, Registry};
pub use rate::{Allowance, RateLimiter};
pub use schedule::{ClockWindow, format_hhmm, in_window, parse_hhmm};
pub use segments::{Segment, SegmentId, SegmentTable};
pub use state::{DownloadState, StateMismatch};
pub use store::{Settings, SettingsPatch, Status, Store, Task};

/// Версія ядра — нею вітаються клієнти IPC при рукостисканні.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Версія протоколу `Protocol`, за яким живуть модулі й зовнішні плагіни.
///
/// Зростає, коли контракт змінюється несумісно. Зовнішній плагін зі старшою
/// версією має отримати відмову з поясненням, а не загадкову поведінку.
pub const PROTOCOL_VERSION: u32 = 1;

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]
mod tests {
    use super::*;

    #[test]
    fn версія_ядра_не_порожня() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn у_корені_репозиторію_лише_readme_md() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let root = root
            .canonicalize()
            .expect("корінь репозиторію");
        let mut extra = Vec::new();
        fn walk(dir: &std::path::Path, root: &std::path::Path, extra: &mut Vec<std::path::PathBuf>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.flatten() {
                let name = e.file_name();
                if name == ".git" || name == "target" {
                    continue;
                }
                let p = e.path();
                if p.is_dir() {
                    walk(&p, root, extra);
                    continue;
                }
                if p.extension().and_then(|x| x.to_str()) != Some("md") {
                    continue;
                }
                let rel = p.strip_prefix(root).unwrap_or(&p);
                if rel != std::path::Path::new("README.md") {
                    extra.push(rel.to_path_buf());
                }
            }
        }
        walk(&root, &root, &mut extra);
        assert!(
            extra.is_empty(),
            "на D:\\Downloader зайві .md (правило: лише README.md): {extra:?}"
        );
    }

    #[test]
    fn помилка_зміненого_ресурсу_називає_url() {
        let err = Error::ResourceChanged {
            url: "https://example.com/file.iso".into(),
        };
        let text = err.to_string();

        // Людина має впізнати помилку за симптомом, а не за назвою типу.
        assert!(text.contains("example.com/file.iso"), "у тексті немає URL: {text}");
        assert!(text.contains("докачування неможливе"), "текст не пояснює наслідок: {text}");
    }

    #[test]
    fn крейт_ядра_не_залежить_від_іменованих_протоколів() {
        let toml = include_str!("../Cargo.toml");
        for заборона in [
            "downloader-proto-http",
            "downloader-proto-hls",
            "downloader-proto-dash",
            "downloader-proto-ytdlp",
        ] {
            assert!(
                !toml.contains(заборона),
                "ядро тягне {заборона} — межа Protocol зламана"
            );
        }

        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut hits = Vec::new();
        fn walk(dir: &std::path::Path, hits: &mut Vec<String>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, hits);
                    continue;
                }
                if p.extension().and_then(|x| x.to_str()) != Some("rs") {
                    continue;
                }
                // Сам цей тест згадує імена як заборонені рядки.
                if p.file_name().and_then(|n| n.to_str()) == Some("lib.rs") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&p) else {
                    continue;
                };
                for маркер in ["HttpProtocol", "HlsProtocol", "DashProtocol", "YtdlpProtocol"] {
                    if text.contains(маркер) {
                        hits.push(format!("{}:{маркер}", p.display()));
                    }
                }
            }
        }
        walk(&src, &mut hits);
        assert!(
            hits.is_empty(),
            "ядро згадує протокол на ім'я: {hits:?}"
        );
    }
}
