//! Windows-специфіка менеджера завантажень.
//!
//! Чотири речі, без яких програма на Windows поводиться неправильно:
//!
//! * [`motw`] — мітка «завантажено з інтернету». Без неї ми обходимо
//!   SmartScreen, і антивіруси починають вважати нас засобом доставки.
//! * [`names`] — безпечні імена файлів. Ім'я з мережі складала стороння
//!   людина, іноді зумисне.
//! * [`paths`] — довгі шляхи й унікальні імена.
//! * [`clipboard`] — текст буфера обміну (`CF_UNICODETEXT`).
//!
//! Крейт збирається й на інших системах: те, чого там немає, стає чесною
//! заглушкою, а не помилкою збірки. Це та межа, яку тримаємо зараз, щоб
//! Linux і macOS колись коштували днів, а не тижнів.

pub mod clipboard;
pub mod motw;
pub mod names;
pub mod paths;

pub use clipboard::{ClipboardError, текст_буфера};
pub use motw::{Zone, is_marked_internet, mark};
pub use names::{extension_for_mime, sanitize, з_розширенням_mime};
pub use paths::{long_path, unique_path};
