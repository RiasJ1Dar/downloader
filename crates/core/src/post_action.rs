//! Післядія порожньої черги.
//!
//! Окремий модуль, бо це не качання і не розклад: політика «що зробити,
//! коли немає `running` і `queued`». ОС-виклик (сон, вимкнення) лишається
//! в `winutil` — цей крейт його не знає.

use crate::error::{Error, Result};

/// Що зробити, коли черга спорожніла.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PostAction {
    /// Нічого — типове.
    #[default]
    None,
    /// Сон (не гібернація).
    Sleep,
    /// Вимкнути комп'ютер.
    Shutdown,
}

impl PostAction {
    /// Як зберігається в базі й IPC.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Sleep => "sleep",
            Self::Shutdown => "shutdown",
        }
    }

    /// Розібрати з бази, IPC або CLI.
    ///
    /// Невідоме значення — помилка, не «нічого»: інакше людина ввімкнула б
    /// вимкнення ПК, а ми тихо проковтнули б друкарську помилку.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "none" | "нічого" => Ok(Self::None),
            "sleep" | "сон" => Ok(Self::Sleep),
            "shutdown" | "вимкнути" | "poweroff" => Ok(Self::Shutdown),
            other => Err(Error::Store(format!("невідома післядія: {other:?}"))),
        }
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
    fn післядія_не_гадає() {
        assert_eq!(PostAction::parse("сон").unwrap(), PostAction::Sleep);
        assert_eq!(PostAction::parse("SHUTDOWN").unwrap(), PostAction::Shutdown);
        assert!(PostAction::parse("reboot").is_err());
        assert_eq!(PostAction::parse("").unwrap(), PostAction::None);
    }
}
