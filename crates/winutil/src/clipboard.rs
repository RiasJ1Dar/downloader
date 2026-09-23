//! Текст із буфера обміну.
//!
//! Потрібен для `dl add --clipboard`: людина скопіювала посилання, ми його
//! читаємо. На Windows — `CF_UNICODETEXT` через `clipboard-win`; на інших ОС —
//! `arboard`. Картинка, список файлів чи порожній буфер — це не URL, і мовчки
//! підставляти порожній рядок тут означало б «додати нічого» без сліду.
//!
//! У цьому крейті `unsafe` лишається забороненим.

/// Помилки читання буфера обміну.
///
/// Текст називає симптом, не внутрішній код WinAPI: викликач покаже його
/// людині в `dl add --clipboard`.
#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    /// Буфер відкрився, але тексту в ньому немає — ні символа.
    #[error("буфер обміну порожній")]
    Empty,
    /// У буфері щось є (картинка, файли), але не Unicode-текст.
    #[error("у буфері обміну немає тексту")]
    NotText,
    /// Інша програма тримає буфер, або робочого столу немає.
    #[error("не вдалося відкрити буфер обміну: {0}")]
    Open(String),
    /// Буфер відкритий, формат є, але прочитати байти не вийшло.
    #[error("не вдалося прочитати текст з буфера обміну: {0}")]
    Read(String),
    /// Немає робочого буфера на цій ОС (немає дисплея / ще не зібрано підтримку).
    #[error("буфер обміну / --clipboard ще не підтримується на цій ОС")]
    Unsupported,
}

/// Прийняти рядок, ніби він щойно прийшов із буфера.
///
/// Порожній рядок і самі пробіли/переноси — це порожній буфер, не URL.
/// Краї зрізаємо: браузер часто кладе посилання з кінцевим `\n`.
pub(crate) fn прийняти_текст(raw: &str) -> Result<String, ClipboardError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ClipboardError::Empty);
    }
    Ok(trimmed.to_owned())
}

/// Немає `CF_UNICODETEXT`: нуль форматів — порожньо, інакше не текст.
fn помилка_без_тексту(
    кількість_форматів: Option<usize>
) -> ClipboardError {
    match кількість_форматів {
        Some(0) => ClipboardError::Empty,
        _ => ClipboardError::NotText,
    }
}

/// Прочитати Unicode-текст із буфера обміну.
///
/// Порожній буфер і не-текст повертають [`ClipboardError`], не порожній
/// `Ok`. На системах без WinAPI — [`ClipboardError::Unsupported`].
pub fn текст_буфера() -> Result<String, ClipboardError> {
    текст_буфера_на_цій_системі()
}

#[cfg(windows)]
fn текст_буфера_на_цій_системі() -> Result<String, ClipboardError> {
    use clipboard_win::formats::{self, CF_UNICODETEXT};
    use clipboard_win::{Clipboard, Getter};

    // Кілька спроб: провідник і браузер часто тримають буфер долю секунди.
    let _clip = Clipboard::new_attempts(10).map_err(|код| ClipboardError::Open(код.to_string()))?;

    if !clipboard_win::is_format_avail(CF_UNICODETEXT) {
        return Err(помилка_без_тексту(
            clipboard_win::count_formats(),
        ));
    }

    let mut текст = String::new();
    formats::Unicode
        .read_clipboard(&mut текст)
        .map_err(|код| ClipboardError::Read(код.to_string()))?;
    прийняти_текст(&текст)
}

#[cfg(not(windows))]
fn текст_буфера_на_цій_системі() -> Result<String, ClipboardError> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| {
        // Без дисплея (SSH, CI) arboard не відкриється — чесна відмова.
        let msg = e.to_string();
        if msg.to_lowercase().contains("display")
            || msg.to_lowercase().contains("wayland")
            || msg.to_lowercase().contains("x11")
        {
            ClipboardError::Unsupported
        } else {
            ClipboardError::Open(msg)
        }
    })?;
    match clipboard.get_text() {
        Ok(text) => прийняти_текст(&text),
        Err(arboard::Error::ContentNotAvailable) => Err(помилка_без_тексту(None)),
        Err(e) => Err(ClipboardError::Read(e.to_string())),
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]
mod tests {
    use super::*;

    #[test]
    fn порожній_рядок_це_помилка() {
        let err = прийняти_текст("").unwrap_err();
        assert!(
            matches!(err, ClipboardError::Empty),
            "очікували Empty, маємо {err:?}"
        );
        assert_eq!(err.to_string(), "буфер обміну порожній");
    }

    #[test]
    fn лише_пробіли_і_переноси_це_порожній_буфер() {
        let err = прийняти_текст(" \n\t\r\n ").unwrap_err();
        assert!(
            matches!(err, ClipboardError::Empty),
            "пробіли не є текстом посилання: {err:?}"
        );
    }

    #[test]
    fn текст_зрізає_краї_і_лишає_рядки_всередині() {
        let got =
            прийняти_текст("  https://a.example/1.zip\nhttps://a.example/2.zip\n").unwrap();
        assert_eq!(got, "https://a.example/1.zip\nhttps://a.example/2.zip");
    }

    #[test]
    fn нуль_форматів_це_порожній_буфер() {
        assert!(matches!(помилка_без_тексту(Some(0)), ClipboardError::Empty));
        assert_eq!(
            помилка_без_тексту(Some(0)).to_string(),
            "буфер обміну порожній"
        );
    }

    #[test]
    fn формати_без_unicodetext_це_не_текст() {
        assert!(matches!(
            помилка_без_тексту(Some(2)),
            ClipboardError::NotText
        ));
        assert!(matches!(помилка_без_тексту(None), ClipboardError::NotText));
        assert_eq!(
            помилка_без_тексту(Some(1)).to_string(),
            "у буфері обміну немає тексту"
        );
    }

    #[test]
    fn симптоми_open_read_unsupported_українською() {
        assert_eq!(
            ClipboardError::Open("занято".into()).to_string(),
            "не вдалося відкрити буфер обміну: занято"
        );
        assert_eq!(
            ClipboardError::Read("відмова".into()).to_string(),
            "не вдалося прочитати текст з буфера обміну: відмова"
        );
        assert_eq!(
            ClipboardError::Unsupported.to_string(),
            "буфер обміну / --clipboard ще не підтримується на цій ОС"
        );
    }
}
