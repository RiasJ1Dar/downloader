//! Ядро менеджера завантажень.
//!
//! Крейт **не знає про UI** і **не згадує жодного протоколу на ім'я**:
//! HTTP, HLS, DASH і майбутній торент приходять сюди через контракт
//! `Protocol` (Ф4). Це те, що дозволяє доточити торент без переписування.
//!
//! Ядро живе в окремому процесі (`downloader-core.exe`); UI, CLI і
//! native messaging host — його клієнти по локальному IPC.

pub mod error;
pub mod file;
pub mod protocol;
pub mod rate;
pub mod segments;
pub mod state;
pub mod store;

pub use error::{Error, Result};
pub use file::SparseFile;
pub use protocol::{Protocol, Registry};
pub use rate::{Allowance, RateLimiter};
pub use segments::{Segment, SegmentId, SegmentTable};
pub use state::{DownloadState, StateMismatch};
pub use store::{Status, Store, Task};

/// Версія ядра — нею вітаються клієнти IPC при рукостисканні.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Версія протоколу `Protocol`, за яким живуть модулі й зовнішні плагіни.
///
/// Зростає, коли контракт змінюється несумісно. Зовнішній плагін зі старшою
/// версією має отримати відмову з поясненням, а не загадкову поведінку.
pub const PROTOCOL_VERSION: u32 = 1;

#[cfg(test)]
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
}
