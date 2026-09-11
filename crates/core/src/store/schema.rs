//! Схема бази й міграції.
//!
//! # Чому `task 1..N file 1..N segment` з першого дня
//!
//! Спокуса зробити `task.path TEXT` велика: сьогодні одне завдання — це
//! один файл. Але торент — це набір файлів, а DASH — окремі доріжки відео,
//! аудіо й субтитрів, які треба звести в один файл. Якщо покласти шлях у
//! завдання, доточити їх потім означатиме **міграцію бази на дисках у
//! людей** і переписування половини UI.
//!
//! Тому файл — окрема сутність від початку, навіть коли він завжди один.
//!
//! # Пастка, через яку каскадне видалення мовчки не працює
//!
//! SQLite вимикає зовнішні ключі **за замовчуванням**. `ON DELETE CASCADE`
//! у схемі при цьому нікого не обманює: він просто не діє, і після
//! видалення завдання його файли й сегменти лишаються в базі назавжди.
//! Тому `PRAGMA foreign_keys = ON` виконується на **кожному** з'єднанні, і
//! на це є окремий тест.

use rusqlite::Connection;

use crate::error::{Error, Result};

/// Поточна версія схеми.
///
/// Зростає з кожною несумісною зміною. База новішої версії відкриттю не
/// підлягає: старша програма не знає про нові поля й тихо їх загубить.
pub const SCHEMA_VERSION: i64 = 1;

/// Налаштування з'єднання.
///
/// Викликається для кожного з'єднання, а не один раз при створенні бази:
/// `foreign_keys` — властивість з'єднання, а не файла.
pub fn configure(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(db_err)?;

    // WAL: читачі не блокують писача. Для нас це означає, що UI може
    // спокійно перечитувати список, поки ядро пише прогрес.
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(db_err)?;

    // NORMAL замість FULL: втратити останні мілісекунди прогресу при
    // раптовому вимкненні живлення не страшно — справжній стан завантаження
    // все одно живе у файлі `.dlpart` поруч із самим файлом.
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(db_err)?;

    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(db_err)?;

    Ok(())
}

/// Створити або оновити схему.
pub fn migrate(conn: &Connection) -> Result<()> {
    let current: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(db_err)?;

    if current > SCHEMA_VERSION {
        return Err(Error::Store(format!(
            "база має схему версії {current}, а програма розуміє лише {SCHEMA_VERSION} — \
             ймовірно, її створила новіша версія програми"
        )));
    }

    if current == SCHEMA_VERSION {
        return Ok(());
    }

    if current < 1 {
        conn.execute_batch(V1).map_err(db_err)?;
    }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(db_err)?;

    Ok(())
}

fn db_err(e: rusqlite::Error) -> Error {
    Error::Store(e.to_string())
}

/// Перша версія схеми.
const V1: &str = r#"
CREATE TABLE IF NOT EXISTS category (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    folder      TEXT NOT NULL,
    -- Розширення через кому, без крапок: "mp4,mkv,avi".
    extensions  TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS task (
    id           INTEGER PRIMARY KEY,
    url          TEXT NOT NULL,
    -- Який модуль качає: http, hls, dash, torrent. Ядро не тлумачить.
    protocol     TEXT NOT NULL,
    status       TEXT NOT NULL,
    title        TEXT,
    category_id  INTEGER REFERENCES category(id) ON DELETE SET NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    finished_at  INTEGER,
    -- Текст помилки для людини, а не код: його читатимуть, а не парситимуть.
    error        TEXT,
    -- Непрозорий стан протоколу (бітова карта торента тощо).
    opaque       TEXT
);

CREATE TABLE IF NOT EXISTS file (
    id           INTEGER PRIMARY KEY,
    task_id      INTEGER NOT NULL REFERENCES task(id) ON DELETE CASCADE,
    -- Порядок усередині завдання: для торента це порядок у роздачі.
    idx          INTEGER NOT NULL,
    path         TEXT NOT NULL,
    size         INTEGER,
    -- ETag або Last-Modified очима протоколу.
    fingerprint  TEXT,
    -- Для наборів: чи качаємо цей файл узагалі.
    selected     INTEGER NOT NULL DEFAULT 1,
    done         INTEGER NOT NULL DEFAULT 0,
    UNIQUE(task_id, idx)
);

CREATE TABLE IF NOT EXISTS segment (
    id       INTEGER PRIMARY KEY,
    file_id  INTEGER NOT NULL REFERENCES file(id) ON DELETE CASCADE,
    -- SegmentId із таблиці сегментів: стабільний, переживає поділи.
    seg_id   INTEGER NOT NULL,
    start    INTEGER NOT NULL,
    end      INTEGER NOT NULL,
    done     INTEGER NOT NULL DEFAULT 0,
    UNIQUE(file_id, seg_id)
);

CREATE INDEX IF NOT EXISTS idx_task_status  ON task(status);
CREATE INDEX IF NOT EXISTS idx_file_task    ON file(task_id);
CREATE INDEX IF NOT EXISTS idx_segment_file ON segment(file_id);
"#;
