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
