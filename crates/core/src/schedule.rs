//! Вікно доби: коли стартувати завдання і який ліміт швидкості діє.
//!
//! Не знає про чергу, IPC і вікно програми. На вхід — хвилини від півночі
//! і межі; на вихід — так/ні або байт/с.

use crate::error::{Error, Result};

/// Відрізок доби. Порожні межі — завжди відкрито.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClockWindow {
    pub from: Option<u16>,
    pub to: Option<u16>,
}

impl ClockWindow {
    /// Чи `now` у вікні. Без обох меж — так.
    #[must_use]
    pub fn contains(self, now: u16) -> bool {
        match (self.from, self.to) {
            (Some(from), Some(to)) => in_window(now, from, to),
            _ => true,
        }
    }
}

/// `"22:00"` → 1320. Пробіли навколо дозволені.
pub fn parse_hhmm(raw: &str) -> Result<u16> {
    let s = raw.trim();
    let (h, m) = s.split_once(':').ok_or_else(|| {
        Error::Store(format!("час має бути ГГ:ХХ, а не {raw:?}"))
    })?;
    let h: u16 = h.parse().map_err(|_| {
        Error::Store(format!("година не число: {raw:?}"))
    })?;
    let m: u16 = m.parse().map_err(|_| {
        Error::Store(format!("хвилина не число: {raw:?}"))
    })?;
    if h > 23 || m > 59 {
        return Err(Error::Store(format!("час поза добою: {raw:?}")));
    }
    Ok(h * 60 + m)
}

/// 1320 → `"22:00"`.
#[must_use]
pub fn format_hhmm(minutes: u16) -> String {
    let minutes = minutes % (24 * 60);
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// Чи `now` у вікні `[from, to)`. `from == to` — ціла доба.
///
/// Вікно через північ (`22:00`–`07:00`) — `now >= from || now < to`.
#[must_use]
pub fn in_window(now: u16, from: u16, to: u16) -> bool {
    if from == to {
        return true;
    }
    if from < to {
        now >= from && now < to
    } else {
        now >= from || now < to
    }
}

/// Денний ліміт, або нічний, якщо вікно тиші задане і `quiet_rate > 0`.
#[must_use]
pub fn effective_rate(day: u64, quiet: ClockWindow, quiet_rate: u64, now: u16) -> u64 {
    if quiet_rate > 0 && quiet.contains(now) {
        quiet_rate
    } else {
        day
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
    fn hhmm_крутиться_туди_й_назад() {
        assert_eq!(parse_hhmm("00:00").unwrap(), 0);
        assert_eq!(parse_hhmm("7:05").unwrap(), 7 * 60 + 5);
        assert_eq!(parse_hhmm(" 22:00 ").unwrap(), 1320);
        assert_eq!(format_hhmm(1320), "22:00");
        assert_eq!(format_hhmm(0), "00:00");
        assert!(parse_hhmm("24:00").is_err());
        assert!(parse_hhmm("12:60").is_err());
        assert!(parse_hhmm("вечір").is_err());
    }

    #[test]
    fn вікно_всередині_доби() {
        assert!(in_window(8 * 60, 8 * 60, 18 * 60));
        assert!(in_window(12 * 60, 8 * 60, 18 * 60));
        assert!(!in_window(18 * 60, 8 * 60, 18 * 60));
        assert!(!in_window(7 * 60 + 59, 8 * 60, 18 * 60));
    }

    #[test]
    fn вікно_через_північ() {
        assert!(in_window(22 * 60, 22 * 60, 7 * 60));
        assert!(in_window(23 * 60 + 59, 22 * 60, 7 * 60));
        assert!(in_window(0, 22 * 60, 7 * 60));
        assert!(in_window(6 * 60 + 59, 22 * 60, 7 * 60));
        assert!(!in_window(7 * 60, 22 * 60, 7 * 60));
        assert!(!in_window(12 * 60, 22 * 60, 7 * 60));
    }

    #[test]
    fn однакові_межі_це_ціла_доба() {
        assert!(in_window(0, 10, 10));
        assert!(in_window(999, 10, 10));
    }

    #[test]
    fn порожнє_вікно_завжди_відкрите() {
        let w = ClockWindow::default();
        assert!(w.contains(0));
        assert!(w.contains(1439));
    }

    #[test]
    fn нічний_ліміт_лише_коли_задано_і_вікно() {
        let quiet = ClockWindow {
            from: Some(0),
            to: Some(7 * 60),
        };
        assert_eq!(effective_rate(1000, quiet, 100, 3 * 60), 100);
        assert_eq!(effective_rate(1000, quiet, 100, 12 * 60), 1000);
        assert_eq!(effective_rate(1000, quiet, 0, 3 * 60), 1000);
    }
}
