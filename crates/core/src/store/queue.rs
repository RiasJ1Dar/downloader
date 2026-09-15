//! Іменовані черги завантажень: власні ліміти, розклад і післядії.
//!
//! Кожна черга керує своєю групою завдань. Черга має власну стелю одночасних
//! завантажень, обмеження швидкості, розклад роботи та післядію при спорожненні.
//! Типова черга називається `"default"` і створюється автоматично при міграції бази.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::post_action::PostAction;
use crate::schedule::parse_hhmm;

/// Ім'я типової черги ядра за замовчуванням.
pub const DEFAULT_QUEUE: &str = "default";

/// Рядок черги в базі.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueRow {
    pub id: i64,
    pub name: String,
    pub max_concurrent: u32,
    pub rate_limit: u64,
    pub paused: bool,
    pub schedule_from: Option<u16>,
    pub schedule_to: Option<u16>,
    pub post_action: PostAction,
    pub created_at: i64,
}

impl Default for QueueRow {
    fn default() -> Self {
        Self {
            id: 0,
            name: DEFAULT_QUEUE.to_owned(),
            max_concurrent: 3,
            rate_limit: 0,
            paused: false,
            schedule_from: None,
            schedule_to: None,
            post_action: PostAction::None,
            created_at: 0,
        }
    }
}

/// Зміна налаштувань черги.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueuePatch {
    pub max_concurrent: Option<u32>,
    pub rate_limit: Option<u64>,
    pub paused: Option<bool>,
    pub schedule_from: Option<String>,
    pub schedule_to: Option<String>,
    pub post_action: Option<String>,
}

impl QueueRow {
    /// Накласти зміни на чергу з валідацією полів.
    pub fn apply_patch(&mut self, patch: &QueuePatch) -> Result<()> {
        if let Some(max) = patch.max_concurrent {
            if max < 1 {
                return Err(Error::Store(
                    "одночасних завантажень у черзі має бути хоча б 1".to_owned(),
                ));
            }
            self.max_concurrent = max;
        }
        if let Some(rate) = patch.rate_limit {
            self.rate_limit = rate;
        }
        if let Some(paused) = patch.paused {
            self.paused = paused;
        }
        if let Some(ref action_raw) = patch.post_action {
            self.post_action = PostAction::parse(action_raw)?;
        }
        if let Some(ref from_raw) = patch.schedule_from {
            self.schedule_from = optional_hhmm(from_raw)?;
        }
        if let Some(ref to_raw) = patch.schedule_to {
            self.schedule_to = optional_hhmm(to_raw)?;
        }

        if self.schedule_from.is_some() != self.schedule_to.is_some() {
            return Err(Error::Store(
                "розклад черги потребує і початку, і кінця (або обидва порожні)".to_owned(),
            ));
        }

        Ok(())
    }

    /// Вікно розкладу черги.
    #[must_use]
    pub fn schedule(&self) -> crate::schedule::ClockWindow {
        crate::schedule::ClockWindow {
            from: self.schedule_from,
            to: self.schedule_to,
        }
    }

    /// Чи зараз дозволено завантаження за розкладом та станом паузи цієї черги.
    #[must_use]
    pub fn downloads_allowed(&self, now_min: u16) -> bool {
        !self.paused && self.schedule().contains(now_min)
    }
}

/// Валідація імені черги: не порожнє, до 64 символів, без керівних символів.
pub fn validate_queue_name(name: &str) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(Error::Store("ім'я черги не може бути порожнім".to_owned()));
    }
    if trimmed.len() > 64 {
        return Err(Error::Store(
            "ім'я черги не може перевищувати 64 символи".to_owned(),
        ));
    }
    if trimmed.chars().any(|c| c.is_control() || c == '/' || c == '\\') {
        return Err(Error::Store(
            "ім'я черги містить неприпустимі символи".to_owned(),
        ));
    }
    Ok(())
}

fn optional_hhmm(raw: &str) -> Result<Option<u16>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    parse_hhmm(trimmed).map(Some)
}

fn db_err(e: rusqlite::Error) -> Error {
    Error::Store(e.to_string())
}

/// Прочитати рядок черги з курсора SQLite.
pub(crate) fn queue_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<QueueRow>> {
    let post_action_raw: String = row.get(7)?;
    let post_action = match PostAction::parse(&post_action_raw) {
        Ok(a) => a,
        Err(e) => return Ok(Err(e)),
    };

    let from_i64: Option<i64> = row.get(5)?;
    let to_i64: Option<i64> = row.get(6)?;

    Ok(Ok(QueueRow {
        id: row.get(0)?,
        name: row.get(1)?,
        max_concurrent: row.get::<_, i64>(2)? as u32,
        rate_limit: row.get::<_, i64>(3)? as u64,
        paused: row.get::<_, i64>(4)? != 0,
        schedule_from: from_i64.map(|v| v as u16),
        schedule_to: to_i64.map(|v| v as u16),
        post_action,
        created_at: row.get(8)?,
    }))
}

/// Завантажити всі черги.
pub fn load_queues(conn: &Connection) -> Result<Vec<QueueRow>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT id, name, max_concurrent, rate_limit, paused, schedule_from, schedule_to, post_action, created_at
             FROM queue ORDER BY id",
        )
        .map_err(db_err)?;

    let rows = stmt.query_map([], queue_from_row).map_err(db_err)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(db_err)??);
    }
    Ok(out)
}

/// Завантажити одну чергу за її назвою.
pub fn load_queue_by_name(conn: &Connection, name: &str) -> Result<Option<QueueRow>> {
    conn.query_row(
        "SELECT id, name, max_concurrent, rate_limit, paused, schedule_from, schedule_to, post_action, created_at
         FROM queue WHERE name = ?1",
        params![name],
        queue_from_row,
    )
    .optional()
    .map_err(db_err)?
    .transpose()
}

/// Створити нову чергу.
pub fn insert_queue(
    conn: &Connection,
    name: &str,
    patch: &QueuePatch,
    now: i64,
) -> Result<QueueRow> {
    validate_queue_name(name)?;
    let trimmed = name.trim();

    let mut row = QueueRow {
        name: trimmed.to_owned(),
        created_at: now,
        ..QueueRow::default()
    };
    row.apply_patch(patch)?;

    let from_i64 = row.schedule_from.map(|v| v as i64);
    let to_i64 = row.schedule_to.map(|v| v as i64);

    conn.execute(
        "INSERT INTO queue (name, max_concurrent, rate_limit, paused, schedule_from, schedule_to, post_action, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            row.name,
            row.max_concurrent as i64,
            row.rate_limit as i64,
            i64::from(row.paused),
            from_i64,
            to_i64,
            row.post_action.as_str(),
            row.created_at,
        ],
    )
    .map_err(db_err)?;

    row.id = conn.last_insert_rowid();
    Ok(row)
}

/// Оновити налаштування існуючої черги.
pub fn update_queue(conn: &Connection, name: &str, patch: &QueuePatch) -> Result<QueueRow> {
    let mut row = load_queue_by_name(conn, name)?
        .ok_or_else(|| Error::Store(format!("чергу '{name}' не знайдено")))?;

    row.apply_patch(patch)?;

    let from_i64 = row.schedule_from.map(|v| v as i64);
    let to_i64 = row.schedule_to.map(|v| v as i64);

    let changed = conn
        .execute(
            "UPDATE queue SET max_concurrent = ?1, rate_limit = ?2, paused = ?3, schedule_from = ?4, schedule_to = ?5, post_action = ?6
             WHERE name = ?7",
            params![
                row.max_concurrent as i64,
                row.rate_limit as i64,
                i64::from(row.paused),
                from_i64,
                to_i64,
                row.post_action.as_str(),
                name,
            ],
        )
        .map_err(db_err)?;

    if changed == 0 {
        return Err(Error::Store(format!("чергу '{name}' не знайдено")));
    }

    Ok(row)
}

/// Видалити чергу (перенісши її завдання у типову чергу 'default').
/// Типову чергу 'default' видаляти заборонено.
pub fn delete_queue(conn: &mut Connection, name: &str) -> Result<()> {
    if name == DEFAULT_QUEUE {
        return Err(Error::Store(
            "типову чергу 'default' не можна видаляти".to_owned(),
        ));
    }

    // Перевіримо наявність черги
    if load_queue_by_name(conn, name)?.is_none() {
        return Err(Error::Store(format!("чергу '{name}' не знайдено")));
    }

    let tx = conn.transaction().map_err(db_err)?;

    // Усі завдання видаленої черги повертаються в типову
    tx.execute(
        "UPDATE task SET queue_name = ?1 WHERE queue_name = ?2",
        params![DEFAULT_QUEUE, name],
    )
    .map_err(db_err)?;

    tx.execute("DELETE FROM queue WHERE name = ?1", params![name])
        .map_err(db_err)?;

    tx.commit().map_err(db_err)?;
    Ok(())
}
