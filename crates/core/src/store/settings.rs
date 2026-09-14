//! Персист правил ядра: черга, ліміт, розклад, післядія.
//!
//! Самі правила живуть у модулях [`crate::schedule`] і [`crate::post_action`].
//! Тут лише ключ/значення в SQLite — щоб закрите вікно не скинуло стелю
//! чи «вимкнути ПК після черги». Теми вікна тут немає: це не ядро.

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};
use crate::post_action::PostAction;
use crate::schedule::{self, ClockWindow, format_hhmm, parse_hhmm};

/// Зміна правил. Порожнє поле — не чіпати; порожній рядок часу — прибрати вікно.
#[derive(Debug, Clone, Default)]
pub struct SettingsPatch {
    pub max_concurrent: Option<u32>,
    pub rate_limit: Option<u64>,
    pub post_action: Option<String>,
    pub schedule_from: Option<String>,
    pub schedule_to: Option<String>,
    pub quiet_from: Option<String>,
    pub quiet_to: Option<String>,
    pub quiet_rate: Option<u64>,
}

/// Зліпок правил, який ядро читає при старті й пише після Configure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Скільки завдань качати одночасно. Мінімум 1.
    pub max_concurrent: u32,
    /// Ліміт байт/с. 0 — без обмеження.
    pub rate_limit: u64,
    /// Що зробити, коли черга спорожніла.
    pub post_action: PostAction,
    /// Початок вікна, коли можна стартувати завдання, у хвилинах від півночі.
    pub schedule_from: Option<u16>,
    /// Кінець вікна (не включно). Разом із `schedule_from`.
    pub schedule_to: Option<u16>,
    /// Початок нічного вікна швидкості.
    pub quiet_from: Option<u16>,
    /// Кінець нічного вікна.
    pub quiet_to: Option<u16>,
    /// Нічний ліміт байт/с. 0 — нічний профіль вимкнено, береться денний.
    pub quiet_rate: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_concurrent: 3,
            rate_limit: 0,
            post_action: PostAction::None,
            schedule_from: None,
            schedule_to: None,
            quiet_from: None,
            quiet_to: None,
            quiet_rate: 0,
        }
    }
}

impl Settings {
    /// Прочитати з бази. Відсутні ключі — типові значення, не помилка.
    pub fn load(conn: &Connection) -> Result<Self> {
        let mut s = Self::default();

        if let Some(v) = get(conn, "max_concurrent")?
            && let Ok(n) = v.parse::<u32>()
            && n >= 1
        {
            s.max_concurrent = n;
        }
        if let Some(v) = get(conn, "rate_limit")?
            && let Ok(n) = v.parse::<u64>()
        {
            s.rate_limit = n;
        }
        if let Some(v) = get(conn, "post_action")? {
            s.post_action = PostAction::parse(&v)?;
        }
        s.schedule_from = hhmm_key(conn, "schedule_from")?;
        s.schedule_to = hhmm_key(conn, "schedule_to")?;
        s.quiet_from = hhmm_key(conn, "quiet_from")?;
        s.quiet_to = hhmm_key(conn, "quiet_to")?;
        if let Some(v) = get(conn, "quiet_rate")?
            && let Ok(n) = v.parse::<u64>()
        {
            s.quiet_rate = n;
        }

        Ok(s)
    }

    /// Накласти зміну. Перевіряє поля; запис на диск — окремо.
    pub fn apply_patch(&mut self, p: SettingsPatch) -> Result<()> {
        if let Some(n) = p.max_concurrent {
            if n < 1 {
                return Err(Error::Store(
                    "одночасних має бути хоча б 1".to_owned(),
                ));
            }
            self.max_concurrent = n;
        }
        if let Some(r) = p.rate_limit {
            self.rate_limit = r;
        }
        if let Some(raw) = p.post_action {
            self.post_action = PostAction::parse(&raw)?;
        }
        if let Some(raw) = p.schedule_from {
            self.schedule_from = optional_hhmm(&raw)?;
        }
        if let Some(raw) = p.schedule_to {
            self.schedule_to = optional_hhmm(&raw)?;
        }
        if let Some(raw) = p.quiet_from {
            self.quiet_from = optional_hhmm(&raw)?;
        }
        if let Some(raw) = p.quiet_to {
            self.quiet_to = optional_hhmm(&raw)?;
        }
        if let Some(r) = p.quiet_rate {
            self.quiet_rate = r;
        }
        if self.schedule_from.is_some() != self.schedule_to.is_some() {
            return Err(Error::Store(
                "розклад потребує і початку, і кінця (або обидва порожні)".to_owned(),
            ));
        }
        if self.quiet_from.is_some() != self.quiet_to.is_some() {
            return Err(Error::Store(
                "нічне вікно потребує і початку, і кінця (або обидва порожні)".to_owned(),
            ));
        }
        Ok(())
    }

    /// Записати всі поля. Порожні вікна прибирають ключ, а не пишуть порожнє.
    pub fn save(&self, conn: &Connection) -> Result<()> {
        put(conn, "max_concurrent", Some(&self.max_concurrent.to_string()))?;
        put(conn, "rate_limit", Some(&self.rate_limit.to_string()))?;
        put(conn, "post_action", Some(self.post_action.as_str()))?;
        put(
            conn,
            "schedule_from",
            self.schedule_from.map(format_hhmm).as_deref(),
        )?;
        put(
            conn,
            "schedule_to",
            self.schedule_to.map(format_hhmm).as_deref(),
        )?;
        put(
            conn,
            "quiet_from",
            self.quiet_from.map(format_hhmm).as_deref(),
        )?;
        put(conn, "quiet_to", self.quiet_to.map(format_hhmm).as_deref())?;
        put(conn, "quiet_rate", Some(&self.quiet_rate.to_string()))?;
        Ok(())
    }

    /// Вікно старту завдань.
    #[must_use]
    pub fn schedule(&self) -> ClockWindow {
        ClockWindow {
            from: self.schedule_from,
            to: self.schedule_to,
        }
    }

    /// Вікно нічного ліміту.
    #[must_use]
    pub fn quiet(&self) -> ClockWindow {
        ClockWindow {
            from: self.quiet_from,
            to: self.quiet_to,
        }
    }

    /// Чи зараз можна стартувати нове завдання за розкладом.
    #[must_use]
    pub fn downloads_allowed(&self, now_min: u16) -> bool {
        self.schedule().contains(now_min)
    }

    /// Який ліміт швидкості діє в цю хвилину.
    #[must_use]
    pub fn effective_rate(&self, now_min: u16) -> u64 {
        schedule::effective_rate(self.rate_limit, self.quiet(), self.quiet_rate, now_min)
    }
}

fn optional_hhmm(raw: &str) -> Result<Option<u16>> {
    if raw.trim().is_empty() {
        Ok(None)
    } else {
        parse_hhmm(raw).map(Some)
    }
}

fn hhmm_key(conn: &Connection, key: &str) -> Result<Option<u16>> {
    match get(conn, key)? {
        Some(v) if v.trim().is_empty() => Ok(None),
        Some(v) => parse_hhmm(&v).map(Some),
        None => Ok(None),
    }
}

fn get(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM setting WHERE key = ?1",
        params![key],
        |r| r.get(0),
    )
    .optional()
    .map_err(|e| Error::Store(e.to_string()))
}

fn put(conn: &Connection, key: &str, value: Option<&str>) -> Result<()> {
    match value {
        Some(v) => {
            conn.execute(
                "INSERT INTO setting (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, v],
            )
            .map_err(|e| Error::Store(e.to_string()))?;
        }
        None => {
            conn.execute("DELETE FROM setting WHERE key = ?1", params![key])
                .map_err(|e| Error::Store(e.to_string()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]
mod tests {
    use super::*;

    #[test]
    fn без_розкладу_завжди_можна() {
        let s = Settings::default();
        assert!(s.downloads_allowed(0));
        assert!(s.downloads_allowed(1439));
    }

    #[test]
    fn латка_порожнього_вікна_прибирає_розклад() {
        let mut s = Settings {
            schedule_from: Some(22 * 60),
            schedule_to: Some(7 * 60),
            ..Settings::default()
        };
        s.apply_patch(SettingsPatch {
            schedule_from: Some(String::new()),
            schedule_to: Some(String::new()),
            ..SettingsPatch::default()
        })
        .expect("порожнє вікно");
        assert!(s.schedule_from.is_none());
        assert!(s.schedule_to.is_none());
    }

    #[test]
    fn латка_половини_розкладу_це_помилка() {
        let mut s = Settings::default();
        let err = s
            .apply_patch(SettingsPatch {
                schedule_from: Some("22:00".into()),
                ..SettingsPatch::default()
            })
            .expect_err("одна межа");
        assert!(err.to_string().contains("початку"));
    }

    #[test]
    fn нічний_ліміт_делегує_в_schedule() {
        let s = Settings {
            rate_limit: 1000,
            quiet_rate: 100,
            quiet_from: Some(0),
            quiet_to: Some(7 * 60),
            ..Settings::default()
        };
        assert_eq!(s.effective_rate(3 * 60), 100);
        assert_eq!(s.effective_rate(12 * 60), 1000);
    }
}
