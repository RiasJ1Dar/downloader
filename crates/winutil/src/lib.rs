//! Windows-специфіка менеджера завантажень.
//!
//! П'ять речей, без яких програма на Windows поводиться неправильно:
//!
//! * [`motw`] — мітка «завантажено з інтернету». Без неї ми обходимо
//!   SmartScreen, і антивіруси починають вважати нас засобом доставки.
//! * [`names`] — безпечні імена файлів. Ім'я з мережі складала стороння
//!   людина, іноді зумисне.
//! * [`paths`] — довгі шляхи й унікальні імена.
//! * [`clipboard`] — текст буфера обміну (`CF_UNICODETEXT`).
//! * [`disk`] — вільне місце на томі призначення до старту качання.
//! * [`power`] — сон і вимкнення ПК після порожньої черги.
//!
//! Крейт збирається й на інших системах: те, чого там немає, стає чесною
//! заглушкою, а не помилкою збірки. Це та межа, яку тримаємо зараз, щоб
//! Linux і macOS колись коштували днів, а не тижнів.

pub mod clipboard;
pub mod disk;
pub mod motw;
pub mod names;
pub mod paths;
pub mod power;

pub use clipboard::{ClipboardError, текст_буфера};
pub use disk::{DiskError, вільні_байти, вистачить_місця};
pub use power::{PowerError, shutdown, sleep};
pub use motw::{Zone, is_marked_internet, mark};
pub use names::{extension_for_mime, sanitize, з_розширенням_mime};
pub use paths::{
    PORTABLE_MARKER, app_data_dir, data_dir_for_exe, default_data_dir,
    default_downloads_dir, downloads_dir_for_exe, is_portable, is_portable_dir,
    long_path, portable_marker_dir, unique_path,
};

