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
pub const SCHEMA_VERSION: i64 = 6;

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

    // Тимчасові таблиці та індекси в оперативній пам'яті, а не на диску.
    conn.pragma_update(None, "temp_store", "MEMORY")
        .map_err(db_err)?;

    // Кеш сторінок на 64 МБ (від'ємне число в SQLite означає кілобайти).
    conn.pragma_update(None, "cache_size", -64000)
        .map_err(db_err)?;

    // Memory-mapped I/O на 256 МБ для швидких zero-copy операцій читання.
    conn.pragma_update(None, "mmap_size", 268435456i64)
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
    if current < 2 {
        conn.execute_batch(V2).map_err(db_err)?;
    }
    if current < 3 {
        conn.execute_batch(V3).map_err(db_err)?;
    }
    if current < 4 {
        conn.execute_batch(V4).map_err(db_err)?;
    }
    if current < 5 {
        conn.execute_batch(V5).map_err(db_err)?;
    }
    if current < 6 {
        conn.execute_batch(V6).map_err(db_err)?;
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

/// Налаштування ядра — ключ/значення, щоб не плодити колонки на кожне поле.
///
/// Живуть у базі, а не у вікні: закрите вікно не має губити стелю, ліміт,
/// розклад і післядію. Черга качає далі з тими самими правилами.
const V2: &str = r#"
CREATE TABLE IF NOT EXISTS setting (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);
"#;

/// Контрольна сума готового файла — окремо від ETag (`fingerprint`).
const V3: &str = r#"
ALTER TABLE file ADD COLUMN checksum TEXT;
"#;

/// Обрана якість.
///
/// ⚠️ Тримати її лише в пам'яті не можна: після перезапуску ядра
/// недокачане завдання поновилося б **без** вибору, модуль обрав би якість
/// сам — і хвіст файла виявився б іншої якості, ніж початок. Зовні це
/// виглядає як зіпсований файл без жодної помилки в журналі.
const V4: &str = r#"
ALTER TABLE task ADD COLUMN variant TEXT;
"#;

/// Додаткові індекси для сортування за статусом і швидких зовнішніх ключів.
const V5: &str = r#"
CREATE INDEX IF NOT EXISTS idx_task_status_id ON task(status, id DESC);
CREATE INDEX IF NOT EXISTS idx_task_category  ON task(category_id);
"#;

/// Іменовані черги завантажень: власні ліміти, розклад і післядії.
///
/// Кожне завдання прив'язується до черги через `queue_name`. За замовчуванням —
/// типова черга 'default' зі стелею 3 одночасних і без ліміту швидкості.
const V6: &str = r#"
CREATE TABLE IF NOT EXISTS queue (
    id              INTEGER PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE,
    max_concurrent  INTEGER NOT NULL DEFAULT 3,
    rate_limit      INTEGER NOT NULL DEFAULT 0,
    paused          INTEGER NOT NULL DEFAULT 0,
    schedule_from   INTEGER,
    schedule_to     INTEGER,
    post_action     TEXT NOT NULL DEFAULT 'none',
    created_at      INTEGER NOT NULL
);

INSERT OR IGNORE INTO queue (name, max_concurrent, rate_limit, paused, post_action, created_at)
VALUES ('default', 3, 0, 0, 'none', 0);

ALTER TABLE task ADD COLUMN queue_name TEXT NOT NULL DEFAULT 'default';
CREATE INDEX IF NOT EXISTS idx_task_queue_status ON task(queue_name, status, id DESC);
"#;

#[cfg(test)]
pub(crate) fn seed_v1(conn: &Connection) -> Result<()> {
    conn.execute_batch(V1).map_err(db_err)?;
    conn.pragma_update(None, "user_version", 1).map_err(db_err)?;
    Ok(())
}
