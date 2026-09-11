//! Протокол HTTP: проба URL, сегментоване качання, докачування.
//!
//! Окремий крейт, а не частина ядра, — саме тому ядро й не знає, що таке
//! HTTP. Коли з'явиться торент, він стане таким самим крейтом поруч, і
//! планувальник не зміниться жодним рядком.

pub mod adapter;
pub mod download;
pub mod headers;
pub mod probe;

pub use headers::{ContentRange, RangeSupport, Validator};
pub use adapter::HttpProtocol;
pub use download::{DownloadError, Options, Outcome, download};
pub use probe::{Probe, ProbeError, probe};
