//! Сховище завдань: SQLite.
//!
//! Тут живе те, що людина бачить у списку: завдання, їхні файли, категорії,
//! історія. Прогрес самого качання — окремо, у файлі `.dlpart` поруч із
//! файлом ([`crate::state`]): він оновлюється десятки разів на секунду й не
//! має права блокувати базу.
//!
//! Розподіл простий: **база — це що качаємо, sidecar — це докуди дійшли**.

pub mod queue;
pub mod schema;
pub mod settings;

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};
use crate::segments::{Segment, SegmentTable};

pub use queue::{DEFAULT_QUEUE, QueuePatch, QueueRow};
pub use settings::{Settings, SettingsPatch};

/// Ідентифікатор завдання.
pub type TaskId = i64;
/// Ідентифікатор файла в завданні.
pub type FileId = i64;

/// Стан завдання.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Стоїть у черзі.
    Queued,
    /// Качається зараз.
    Running,
    /// Зупинене людиною.
    Paused,
    /// Завершене успішно.
    Done,
    /// Впало з помилкою.
    Failed,
}

impl Status {
    /// Як зберігається в базі.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    /// Розібрати з бази.
    ///
    /// Невідоме значення — не привід гадати: повертаємо помилку, бо інакше
    /// завдання тихо перейде в стан, якого ніхто не задавав.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "paused" => Ok(Self::Paused),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            other => Err(Error::Store(format!(
                "невідомий стан завдання в базі: {other:?}"
            ))),
        }
    }
}

/// Завдання у списку.
#[derive(Debug, Clone)]
pub struct Task {
    pub id: TaskId,
    pub url: String,
    pub protocol: String,
    pub status: Status,
    pub title: Option<String>,
    pub category_id: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
    pub error: Option<String>,
    /// Обрана якість, як її назвав модуль. `None` — вибору не було.
    pub variant: Option<String>,
    /// Назва черги, до якої належить завдання.
    pub queue: String,
}

/// Файл усередині завдання.
#[derive(Debug, Clone)]
pub struct FileRow {
    pub id: FileId,
    pub task_id: TaskId,
    pub idx: i64,
    pub path: PathBuf,
    pub size: Option<u64>,
    pub fingerprint: Option<String>,
    pub selected: bool,
    pub done: u64,
    /// SHA-256 готового файла, якщо вже пораховано.
    pub checksum: Option<String>,
}

/// Категорія з текою призначення.
#[derive(Debug, Clone)]
pub struct Category {
    pub id: i64,
    pub name: String,
    pub folder: PathBuf,
    /// Розширення без крапок, у нижньому регістрі.
    pub extensions: Vec<String>,
}

/// Що додаємо.
#[derive(Debug, Clone)]
pub struct NewTask {
    pub url: String,
    pub protocol: String,
    pub title: Option<String>,
    pub category_id: Option<i64>,
    /// Обрана якість, як її назвав модуль.
    pub variant: Option<String>,
    /// Черга завдання. `None` — типова черга 'default'.
    pub queue: Option<String>,
    /// Файли завдання. Для звичайного HTTP тут рівно один запис.
    pub files: Vec<NewFile>,
}

/// Файл, який дає завдання.
#[derive(Debug, Clone)]
pub struct NewFile {
    pub path: PathBuf,
    pub size: Option<u64>,
    pub fingerprint: Option<String>,
    pub selected: bool,
}

/// Сховище.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Відкрити базу, створивши її за потреби.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref()).map_err(db)?;
        schema::configure(&conn)?;
        schema::migrate(&conn)?;
        Ok(Self { conn })
    }

    /// База в пам'яті — для тестів.
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(db)?;
        schema::configure(&conn)?;
        schema::migrate(&conn)?;
        Ok(Self { conn })
    }

    // ── Завдання ────────────────────────────────────────────────────────

    /// Додати завдання разом із його файлами.
    ///
    /// Усе одним записом: завдання без файлів — це напівстан, у якому UI
    /// показував би порожній рядок, а рушій не знав би, куди писати.
    pub fn add_task(&mut self, new: &NewTask) -> Result<TaskId> {
        if new.files.is_empty() {
            return Err(Error::Store(
                "завдання без жодного файла додавати не можна".to_owned(),
            ));
        }

        let q_name = new.queue.as_deref().unwrap_or(DEFAULT_QUEUE).trim();
        queue::validate_queue_name(q_name)?;

        let now = now_ms();
        let tx = self.conn.transaction().map_err(db)?;

        // Перевіряємо існування черги, якщо це не 'default'
        if q_name != DEFAULT_QUEUE {
            let exists: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM queue WHERE name = ?1)",
                    params![q_name],
                    |r| r.get(0),
                )
                .map_err(db)?;
            if !exists {
                return Err(Error::Store(format!("чергу '{q_name}' не знайдено")));
            }
        }

        tx.execute(
            "INSERT INTO task
                (url, protocol, status, title, category_id, variant, queue_name, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![
                new.url,
                new.protocol,
                Status::Queued.as_str(),
                new.title,
                new.category_id,
                new.variant,
                q_name,
                now
            ],
        )
        .map_err(db)?;

        let task_id = tx.last_insert_rowid();

        for (i, f) in new.files.iter().enumerate() {
            tx.execute(
                "INSERT INTO file (task_id, idx, path, size, fingerprint, selected)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    task_id,
                    i as i64,
                    f.path.to_string_lossy(),
                    f.size.map(|v| v as i64),
                    f.fingerprint,
                    i64::from(f.selected)
                ],
            )
            .map_err(db)?;
        }

        tx.commit().map_err(db)?;
        Ok(task_id)
    }

    /// Одне завдання за ідентифікатором.
    pub fn task(&self, id: TaskId) -> Result<Option<Task>> {
        self.conn
            .query_row(
                "SELECT id, url, protocol, status, title, category_id,
                        created_at, updated_at, finished_at, error, variant, queue_name
                 FROM task WHERE id = ?1",
                params![id],
                task_from_row,
            )
            .optional()
            .map_err(db)?
            .transpose()
    }

    /// Усі завдання, найновіші зверху.
    pub fn tasks(&self) -> Result<Vec<Task>> {
        self.tasks_where(None)
    }

    /// Завдання в заданому стані.
    pub fn tasks_with_status(&self, status: Status) -> Result<Vec<Task>> {
        self.tasks_where(Some(status))
    }

    /// Завдання вказаної черги, найновіші зверху.
    pub fn tasks_in_queue(&self, queue_name: &str) -> Result<Vec<Task>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT id, url, protocol, status, title, category_id,
                        created_at, updated_at, finished_at, error, variant, queue_name
                 FROM task WHERE queue_name = ?1 ORDER BY id DESC",
            )
            .map_err(db)?;
        let rows = stmt.query_map(params![queue_name], task_from_row).map_err(db)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(db)??);
        }
        Ok(out)
    }

    /// Завдання вказаної черги у певному стані, за зростанням id (FIFO черга).
    pub fn queue_tasks_ordered(&self, queue_name: &str, status: Status) -> Result<Vec<Task>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT id, url, protocol, status, title, category_id,
                        created_at, updated_at, finished_at, error, variant, queue_name
                 FROM task WHERE queue_name = ?1 AND status = ?2 ORDER BY id ASC",
            )
            .map_err(db)?;
        let rows = stmt
            .query_map(params![queue_name, status.as_str()], task_from_row)
            .map_err(db)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(db)??);
        }
        Ok(out)
    }

    fn tasks_where(&self, status: Option<Status>) -> Result<Vec<Task>> {
        let mut out = Vec::new();

        match status {
            Some(s) => {
                let mut stmt = self
                    .conn
                    .prepare_cached(
                        "SELECT id, url, protocol, status, title, category_id,
                                created_at, updated_at, finished_at, error, variant, queue_name
                         FROM task WHERE status = ?1 ORDER BY id DESC",
                    )
                    .map_err(db)?;
                let rows = stmt.query_map(params![s.as_str()], task_from_row).map_err(db)?;
                for r in rows {
                    out.push(r.map_err(db)??);
                }
            }
            None => {
                let mut stmt = self
                    .conn
                    .prepare_cached(
                        "SELECT id, url, protocol, status, title, category_id,
                                created_at, updated_at, finished_at, error, variant, queue_name
                         FROM task ORDER BY id DESC",
                    )
                    .map_err(db)?;
                let rows = stmt.query_map([], task_from_row).map_err(db)?;
                for r in rows {
                    out.push(r.map_err(db)??);
                }
            }
        }

        Ok(out)
    }

    /// Змінити стан завдання.
    ///
    /// `error` має сенс лише для [`Status::Failed`]; при переході в будь-який
    /// інший стан старий текст помилки прибирається — інакше в UI поруч із
    /// «качається» висіла б скарга з минулого тижня.
    pub fn set_status(&mut self, id: TaskId, status: Status, error: Option<&str>) -> Result<()> {
        let now = now_ms();
        let finished = matches!(status, Status::Done | Status::Failed).then_some(now);
        let error = if status == Status::Failed { error } else { None };

        let changed = self
            .conn
            .execute(
                "UPDATE task SET status = ?2, error = ?3, updated_at = ?4, finished_at = ?5
                 WHERE id = ?1",
                params![id, status.as_str(), error, now, finished],
            )
            .map_err(db)?;

        if changed == 0 {
            return Err(Error::Store(format!("завдання {id} не знайдено")));
        }
        Ok(())
    }

    /// Видалити завдання разом із файлами й сегментами.
    pub fn remove_task(&mut self, id: TaskId) -> Result<()> {
        self.conn
            .execute("DELETE FROM task WHERE id = ?1", params![id])
            .map_err(db)?;
        Ok(())
    }

    // ── Файли ───────────────────────────────────────────────────────────

    /// Файли завдання, у порядку додавання.
    pub fn files(&self, task_id: TaskId) -> Result<Vec<FileRow>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT id, task_id, idx, path, size, fingerprint, selected, done, checksum
                 FROM file WHERE task_id = ?1 ORDER BY idx",
            )
            .map_err(db)?;

        let rows = stmt
            .query_map(params![task_id], |row| {
                Ok(FileRow {
                    id: row.get(0)?,
                    task_id: row.get(1)?,
                    idx: row.get(2)?,
                    path: PathBuf::from(row.get::<_, String>(3)?),
                    size: row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                    fingerprint: row.get(5)?,
                    selected: row.get::<_, i64>(6)? != 0,
                    done: row.get::<_, i64>(7)? as u64,
                    checksum: row.get(8)?,
                })
            })
            .map_err(db)?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(db)?);
        }
        Ok(out)
    }

    /// Записати SHA-256 готового файла.
    pub fn set_file_checksum(&mut self, file_id: FileId, checksum: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE file SET checksum = ?2 WHERE id = ?1",
                params![file_id, checksum],
            )
            .map_err(db)?;
        Ok(())
    }

    /// Оновити, скільки завантажено у файлі.
    pub fn set_file_done(&mut self, file_id: FileId, done: u64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE file SET done = ?2 WHERE id = ?1",
                params![file_id, done as i64],
            )
            .map_err(db)?;
        Ok(())
    }

    // ── Сегменти ────────────────────────────────────────────────────────

    /// Зберегти таблицю сегментів файла.
    ///
    /// ⚠️ Це **не** заміна `.dlpart`. Сюди сегменти лягають рідко — щоб UI
    /// міг намалювати розкладку, і щоб після перезапуску було видно картину
    /// ще до відкриття самого файла. Гарячий шлях качання пише в sidecar.
    pub fn save_segments(&mut self, file_id: FileId, table: &SegmentTable) -> Result<()> {
        let tx = self.conn.transaction().map_err(db)?;

        tx.execute("DELETE FROM segment WHERE file_id = ?1", params![file_id])
            .map_err(db)?;

        {
            let mut stmt = tx
                .prepare_cached(
                    "INSERT INTO segment (file_id, seg_id, start, end, done)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .map_err(db)?;

            for seg in table.segments() {
                stmt.execute(params![
                    file_id,
                    seg.id as i64,
                    seg.start as i64,
                    seg.end as i64,
                    seg.done as i64
                ])
                .map_err(db)?;
            }
        }

        tx.commit().map_err(db)?;
        Ok(())
    }

    /// Відновити таблицю сегментів файла.
    ///
    /// `None`, якщо сегментів немає — завдання ще не починалось.
    pub fn load_segments(&self, file_id: FileId, total: u64) -> Result<Option<SegmentTable>> {
        let mut stmt = self
            .conn
            .prepare_cached(
                "SELECT seg_id, start, end, done FROM segment
                 WHERE file_id = ?1 ORDER BY start",
            )
            .map_err(db)?;

        let rows = stmt
            .query_map(params![file_id], |row| {
                Ok(Segment {
                    id: row.get::<_, i64>(0)? as u64,
                    start: row.get::<_, i64>(1)? as u64,
                    end: row.get::<_, i64>(2)? as u64,
                    done: row.get::<_, i64>(3)? as u64,
                })
            })
            .map_err(db)?;

        let mut segs = Vec::new();
        for r in rows {
            segs.push(r.map_err(db)?);
        }

        if segs.is_empty() {
            return Ok(None);
        }

        // Перевірка інваріантів — база могла постраждати, а таблиця з
        // дірками дала б файл із дірками.
        SegmentTable::from_parts(segs, total).map(Some)
    }

    // ── Категорії ───────────────────────────────────────────────────────

    /// Додати категорію.
    pub fn add_category(
        &mut self,
        name: &str,
        folder: &Path,
        extensions: &[&str],
    ) -> Result<i64> {
        let exts = extensions.join(",").to_lowercase();
        self.conn
            .execute(
                "INSERT INTO category (name, folder, extensions) VALUES (?1, ?2, ?3)",
                params![name, folder.to_string_lossy(), exts],
            )
            .map_err(db)?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Усі категорії.
    pub fn categories(&self) -> Result<Vec<Category>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, name, folder, extensions FROM category ORDER BY name")
            .map_err(db)?;

        let rows = stmt
            .query_map([], |row| {
                let exts: String = row.get(3)?;
                Ok(Category {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    folder: PathBuf::from(row.get::<_, String>(2)?),
                    extensions: exts
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect(),
                })
            })
            .map_err(db)?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(db)?);
        }
        Ok(out)
    }

    /// Підібрати категорію за іменем файла.
    ///
    /// Порівняння за розширенням без урахування регістру: `.MP4` і `.mp4` —
    /// те саме відео.
    pub fn category_for(&self, filename: &str) -> Result<Option<Category>> {
        let Some(ext) = filename.rsplit_once('.').map(|(_, e)| e.to_lowercase()) else {
            return Ok(None);
        };

        Ok(self
            .categories()?
            .into_iter()
            .find(|c| c.extensions.contains(&ext)))
    }

    /// Типові категорії, лише якщо таблиця порожня. Теки ще не створюємо —
    /// вони з'являться, коли туди вперше ляже файл.
    pub fn seed_default_categories(&mut self, downloads: &Path) -> Result<usize> {
        if !self.categories()?.is_empty() {
            return Ok(0);
        }
        let rows: [(&str, &str, &[&str]); 3] = [
            ("Відео", "Video", &["mp4", "mkv", "webm", "avi", "mov"]),
            ("Аудіо", "Audio", &["mp3", "m4a", "flac", "ogg", "wav"]),
            ("Архіви", "Archives", &["zip", "7z", "rar", "tar", "gz"]),
        ];
        for (name, folder, exts) in rows {
            self.add_category(name, &downloads.join(folder), exts)?;
        }
        Ok(rows.len())
    }

    // ── Налаштування ────────────────────────────────────────────────────

    /// Поточні правила ядра. Відсутня таблиця чи ключі — типові значення.
    pub fn settings(&self) -> Result<Settings> {
        Settings::load(&self.conn)
    }

    /// Записати правила. Викликач уже перевірив поля.
    pub fn save_settings(&mut self, settings: &Settings) -> Result<()> {
        settings.save(&self.conn)
    }

    // ── Черги ───────────────────────────────────────────────────────────

    /// Усі наявні черги завантажень.
    pub fn queues(&self) -> Result<Vec<QueueRow>> {
        queue::load_queues(&self.conn)
    }

    /// Черга за її назвою.
    pub fn queue(&self, name: &str) -> Result<Option<QueueRow>> {
        queue::load_queue_by_name(&self.conn, name.trim())
    }

    /// Створити нову чергу із заданими правилами.
    pub fn create_queue(&mut self, name: &str, patch: &QueuePatch) -> Result<QueueRow> {
        let now = now_ms();
        queue::insert_queue(&self.conn, name, patch, now)
    }

    /// Змінити правила існуючої черги.
    pub fn update_queue(&mut self, name: &str, patch: &QueuePatch) -> Result<QueueRow> {
        queue::update_queue(&self.conn, name.trim(), patch)
    }

    /// Перейменувати чергу з оновленням усіх її завдань.
    pub fn rename_queue(&mut self, old_name: &str, new_name: &str) -> Result<()> {
        let old = old_name.trim();
        let new = new_name.trim();
        if old == DEFAULT_QUEUE {
            return Err(Error::Store("типову чергу не можна перейменовувати".to_owned()));
        }
        queue::validate_queue_name(new)?;
        if self.queue(new)?.is_some() {
            return Err(Error::Store(format!("черга '{new}' вже існує")));
        }
        let tx = self.conn.transaction().map_err(db)?;
        let changed = tx.execute(
            "UPDATE queue SET name = ?2 WHERE name = ?1",
            params![old, new],
        ).map_err(db)?;
        if changed == 0 {
            return Err(Error::Store(format!("чергу '{old}' не знайдено")));
        }
        tx.execute(
            "UPDATE task SET queue_name = ?2 WHERE queue_name = ?1",
            params![old, new],
        ).map_err(db)?;
        tx.commit().map_err(db)?;
        Ok(())
    }

    /// Видалити чергу. Її завдання автоматично повертаються в типову чергу 'default'.
    /// Типову чергу видаляти заборонено.
    pub fn delete_queue(&mut self, name: &str) -> Result<()> {
        queue::delete_queue(&mut self.conn, name.trim())
    }

    /// Перенести завдання в іншу чергу.
    pub fn set_task_queue(&mut self, task_id: TaskId, queue_name: &str) -> Result<()> {
        let trimmed = queue_name.trim();
        queue::validate_queue_name(trimmed)?;
        if trimmed != DEFAULT_QUEUE && self.queue(trimmed)?.is_none() {
            return Err(Error::Store(format!("чергу '{trimmed}' не знайдено")));
        }
        let now = now_ms();
        let changed = self
            .conn
            .execute(
                "UPDATE task SET queue_name = ?2, updated_at = ?3 WHERE id = ?1",
                params![task_id, trimmed, now],
            )
            .map_err(db)?;
        if changed == 0 {
            return Err(Error::Store(format!("завдання {task_id} не знайдено")));
        }
        Ok(())
    }
}

fn task_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Task>> {
    let status_raw: String = row.get(3)?;

    Ok(Status::parse(&status_raw).map(|status| Task {
        id: row.get(0).unwrap_or_default(),
        url: row.get(1).unwrap_or_default(),
        protocol: row.get(2).unwrap_or_default(),
        status,
        title: row.get(4).unwrap_or_default(),
        category_id: row.get(5).unwrap_or_default(),
        created_at: row.get(6).unwrap_or_default(),
        updated_at: row.get(7).unwrap_or_default(),
        finished_at: row.get(8).unwrap_or_default(),
        error: row.get(9).unwrap_or_default(),
        variant: row.get(10).unwrap_or_default(),
        queue: row.get(11).unwrap_or_else(|_| DEFAULT_QUEUE.to_owned()),
    }))
}

fn db(e: rusqlite::Error) -> Error {
    Error::Store(e.to_string())
}

/// Поточний час у мілісекундах.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::let_underscore_must_use,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]
mod tests {
    use super::*;
    use crate::PostAction;

    fn звичайне_завдання(url: &str, path: &str) -> NewTask {
        NewTask {
            url: url.to_owned(),
            protocol: "http".to_owned(),
            title: None,
            category_id: None,
            variant: None,
            queue: None,
            files: vec![NewFile {
                path: PathBuf::from(path),
                size: Some(1000),
                fingerprint: Some("\"abc\"".to_owned()),
                selected: true,
            }],
        }
    }

    #[test]
    fn нова_база_має_поточну_версію_схеми() {
        let s = Store::in_memory().unwrap();
        let v: i64 = s
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, schema::SCHEMA_VERSION);
    }

    #[test]
    fn міграція_ідемпотентна() {
        let s = Store::in_memory().unwrap();
        // Повторний прогін не має ані падати, ані щось ламати.
        schema::migrate(&s.conn).unwrap();
        schema::migrate(&s.conn).unwrap();
    }

    #[test]
    fn база_новішої_версії_не_відкривається() {
        let s = Store::in_memory().unwrap();
        s.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION_MAJBUTNJA)
            .unwrap();

        let err = schema::migrate(&s.conn).expect_err("новішу схему читати не можна");
        assert!(
            err.to_string().contains("новіша версія"),
            "помилка не пояснює причини: {err}"
        );
    }
    const SCHEMA_VERSION_MAJBUTNJA: i64 = 99;

    #[test]
    fn завдання_додається_разом_із_файлом() {
        let mut s = Store::in_memory().unwrap();
        let id = s
            .add_task(&звичайне_завдання("https://e.com/f.bin", "C:/dl/f.bin"))
            .unwrap();

        let t = s.task(id).unwrap().expect("щойно додане завдання");
        assert_eq!(t.url, "https://e.com/f.bin");
        assert_eq!(t.status, Status::Queued);
        assert!(t.error.is_none());

        let files = s.files(id).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].size, Some(1000));
        assert!(files[0].selected);
    }

    #[test]
    fn завдання_без_файлів_не_приймається() {
        let mut s = Store::in_memory().unwrap();
        let порожнє = NewTask {
            url: "https://e.com/x".to_owned(),
            protocol: "http".to_owned(),
            title: None,
            category_id: None,
            variant: None,
            queue: None,
            files: Vec::new(),
        };

        let err = s.add_task(&порожнє).expect_err("завдання має мати файли");
        assert!(err.to_string().contains("без жодного файла"), "{err}");
    }

    /// Заради цього тесту й зроблено модель `task 1..N file`.
    #[test]
    fn одне_завдання_може_мати_багато_файлів() {
        let mut s = Store::in_memory().unwrap();

        // Так виглядатиме DASH: відео, аудіо, субтитри одним завданням.
        let набір = NewTask {
            url: "https://e.com/manifest.mpd".to_owned(),
            protocol: "dash".to_owned(),
            title: Some("Фільм".to_owned()),
            category_id: None,
            variant: None,
            queue: None,
            files: vec![
                NewFile {
                    path: PathBuf::from("video.m4s"),
                    size: Some(900),
                    fingerprint: None,
                    selected: true,
                },
                NewFile {
                    path: PathBuf::from("audio.m4s"),
                    size: Some(100),
                    fingerprint: None,
                    selected: true,
                },
                NewFile {
                    path: PathBuf::from("subs.vtt"),
                    size: Some(10),
                    fingerprint: None,
                    // Субтитри людина може й не захотіти.
                    selected: false,
                },
            ],
        };

        let id = s.add_task(&набір).unwrap();
        let files = s.files(id).unwrap();

        assert_eq!(files.len(), 3);
        assert_eq!(files[0].idx, 0, "порядок файлів має зберігатись");
        assert!(!files[2].selected, "невибраний файл лишається невибраним");
    }

    #[test]
    fn видалення_завдання_прибирає_файли_й_сегменти() {
        let mut s = Store::in_memory().unwrap();
        let id = s
            .add_task(&звичайне_завдання("https://e.com/f.bin", "f.bin"))
            .unwrap();
        let file_id = s.files(id).unwrap()[0].id;

        let table = SegmentTable::new(1000, 4, 1);
        s.save_segments(file_id, &table).unwrap();

        s.remove_task(id).unwrap();

        // ⚠️ Саме тут ловиться вимкнений `foreign_keys`: без PRAGMA
        // каскад мовчки не спрацював би, і сироти лишились би назавжди.
        let files: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM file", [], |r| r.get(0))
            .unwrap();
        let segs: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM segment", [], |r| r.get(0))
            .unwrap();

        assert_eq!(files, 0, "файли лишились сиротами — каскад не працює");
        assert_eq!(segs, 0, "сегменти лишились сиротами — каскад не працює");
    }

    #[test]
    fn сегменти_обходять_базу_і_повертаються_такими_самими() {
        let mut s = Store::in_memory().unwrap();
        let id = s
            .add_task(&звичайне_завдання("https://e.com/f.bin", "f.bin"))
            .unwrap();
        let file_id = s.files(id).unwrap()[0].id;

        let mut table = SegmentTable::new(1000, 4, 1);
        table.advance(0, 100).unwrap();
        let stolen = table.steal(8).unwrap();
        table.advance(stolen, 5).unwrap();

        s.save_segments(file_id, &table).unwrap();
        let back = s.load_segments(file_id, 1000).unwrap().expect("сегменти є");

        assert_eq!(back.len(), table.len());
        assert_eq!(back.downloaded(), table.downloaded());
        back.check().unwrap();
    }

    #[test]
    fn повторне_збереження_сегментів_не_дублює_їх() {
        let mut s = Store::in_memory().unwrap();
        let id = s
            .add_task(&звичайне_завдання("https://e.com/f.bin", "f.bin"))
            .unwrap();
        let file_id = s.files(id).unwrap()[0].id;

        let table = SegmentTable::new(1000, 4, 1);
        s.save_segments(file_id, &table).unwrap();
        s.save_segments(file_id, &table).unwrap();

        let back = s.load_segments(file_id, 1000).unwrap().unwrap();
        assert_eq!(back.len(), 4, "сегменти продубльовано");
    }

    #[test]
    fn відсутні_сегменти_це_не_помилка() {
        let mut s = Store::in_memory().unwrap();
        let id = s
            .add_task(&звичайне_завдання("https://e.com/f.bin", "f.bin"))
            .unwrap();
        let file_id = s.files(id).unwrap()[0].id;

        assert!(s.load_segments(file_id, 1000).unwrap().is_none());
    }

    #[test]
    fn помилка_зникає_при_поверненні_в_роботу() {
        let mut s = Store::in_memory().unwrap();
        let id = s
            .add_task(&звичайне_завдання("https://e.com/f.bin", "f.bin"))
            .unwrap();

        s.set_status(id, Status::Failed, Some("сервер віддав 403"))
            .unwrap();
        let t = s.task(id).unwrap().unwrap();
        assert_eq!(t.error.as_deref(), Some("сервер віддав 403"));
        assert!(t.finished_at.is_some());

        s.set_status(id, Status::Running, None).unwrap();
        let t = s.task(id).unwrap().unwrap();
        assert!(
            t.error.is_none(),
            "стара помилка висіла б у списку поруч зі «качається»"
        );
        assert!(t.finished_at.is_none());
    }

    #[test]
    fn стан_невідомого_завдання_міняти_не_можна() {
        let mut s = Store::in_memory().unwrap();
        assert!(s.set_status(999, Status::Done, None).is_err());
    }

    #[test]
    fn невідомий_стан_у_базі_не_тлумачиться_навмання() {
        let mut s = Store::in_memory().unwrap();
        let id = s
            .add_task(&звичайне_завдання("https://e.com/f.bin", "f.bin"))
            .unwrap();

        s.conn
            .execute("UPDATE task SET status = 'дивина' WHERE id = ?1", params![id])
            .unwrap();

        assert!(
            s.task(id).is_err(),
            "невідомий стан мусить бути помилкою, а не мовчазним переходом"
        );
    }

    #[test]
    fn фільтр_за_станом_повертає_потрібні() {
        let mut s = Store::in_memory().unwrap();
        let a = s.add_task(&звичайне_завдання("https://e.com/a", "a")).unwrap();
        let _b = s.add_task(&звичайне_завдання("https://e.com/b", "b")).unwrap();

        s.set_status(a, Status::Done, None).unwrap();

        assert_eq!(s.tasks().unwrap().len(), 2);
        assert_eq!(s.tasks_with_status(Status::Done).unwrap().len(), 1);
        assert_eq!(s.tasks_with_status(Status::Queued).unwrap().len(), 1);
    }

    #[test]
    fn типові_категорії_лише_коли_порожньо() {
        let mut s = Store::in_memory().unwrap();
        let n = s.seed_default_categories(Path::new("D:/Downloads")).unwrap();
        assert_eq!(n, 3);
        assert_eq!(s.seed_default_categories(Path::new("D:/Downloads")).unwrap(), 0);
        let c = s.category_for("фільм.mkv").unwrap().expect("відео");
        assert_eq!(c.name, "Відео");
        assert_eq!(c.folder, PathBuf::from("D:/Downloads/Video"));
    }

    #[test]
    fn категорія_підбирається_за_розширенням() {
        let mut s = Store::in_memory().unwrap();
        s.add_category("Відео", Path::new("D:/Video"), &["mp4", "mkv", "avi"])
            .unwrap();
        s.add_category("Документи", Path::new("D:/Docs"), &["pdf", "docx"])
            .unwrap();

        let c = s.category_for("фільм.MKV").unwrap().expect("є категорія");
        assert_eq!(c.name, "Відео", "регістр розширення не має значення");
        assert_eq!(c.folder, PathBuf::from("D:/Video"));

        assert!(s.category_for("архів.7z").unwrap().is_none());
        assert!(s.category_for("README").unwrap().is_none());
    }

    #[test]
    fn база_переживає_закриття_і_відкриття() {
        let mut dir = std::env::temp_dir();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        dir.push(format!("store-test-{unique}.db"));

        let id = {
            let mut s = Store::open(&dir).unwrap();
            s.add_task(&звичайне_завдання("https://e.com/f.bin", "f.bin"))
                .unwrap()
        };

        {
            let s = Store::open(&dir).unwrap();
            let t = s.task(id).unwrap().expect("завдання мало пережити перезапуск");
            assert_eq!(t.url, "https://e.com/f.bin");
        }

        let _ = std::fs::remove_file(&dir);
        let _ = std::fs::remove_file(dir.with_extension("db-wal"));
        let _ = std::fs::remove_file(dir.with_extension("db-shm"));
    }

    #[test]
    fn нова_база_має_таблицю_setting() {
        let mut s = Store::in_memory().unwrap();
        let def = Settings::default();
        assert_eq!(s.settings().unwrap(), def);

        let mut next = def.clone();
        next.max_concurrent = 8;
        next.rate_limit = 4096;
        next.post_action = PostAction::Sleep;
        next.schedule_from = Some(22 * 60);
        next.schedule_to = Some(7 * 60);
        next.quiet_from = Some(0);
        next.quiet_to = Some(6 * 60);
        next.quiet_rate = 50 * 1024;
        s.save_settings(&next).unwrap();
        assert_eq!(s.settings().unwrap(), next);
    }

    #[test]
    fn налаштування_переживають_перезапуск() {
        let mut dir = std::env::temp_dir();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        dir.push(format!("store-settings-{unique}.db"));

        {
            let mut s = Store::open(&dir).unwrap();
            let next = Settings {
                max_concurrent: 5,
                post_action: PostAction::Shutdown,
                schedule_from: Some(1),
                schedule_to: Some(2),
                ..Settings::default()
            };
            s.save_settings(&next).unwrap();
        }

        {
            let s = Store::open(&dir).unwrap();
            let back = s.settings().unwrap();
            assert_eq!(back.max_concurrent, 5);
            assert_eq!(back.post_action, PostAction::Shutdown);
            assert_eq!(back.schedule_from, Some(1));
            assert_eq!(back.schedule_to, Some(2));
        }

        let _ = std::fs::remove_file(&dir);
        let _ = std::fs::remove_file(dir.with_extension("db-wal"));
        let _ = std::fs::remove_file(dir.with_extension("db-shm"));
    }

    #[test]
    fn порожнє_вікно_прибирає_ключ() {
        let mut s = Store::in_memory().unwrap();
        let with_window = Settings {
            schedule_from: Some(10),
            schedule_to: Some(20),
            ..Settings::default()
        };
        s.save_settings(&with_window).unwrap();
        s.save_settings(&Settings::default()).unwrap();
        let back = s.settings().unwrap();
        assert!(back.schedule_from.is_none());
        assert!(back.schedule_to.is_none());
    }

    #[test]
    fn checksum_переживає_перезапуск() {
        let mut s = Store::in_memory().unwrap();
        let id = s
            .add_task(&звичайне_завдання("https://e.com/f.bin", "f.bin"))
            .unwrap();
        let fid = s.files(id).unwrap()[0].id;
        s.set_file_checksum(fid, "abc").unwrap();
        assert_eq!(s.files(id).unwrap()[0].checksum.as_deref(), Some("abc"));
    }

    #[test]
    fn v1_база_отримує_setting() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        schema::configure(&conn).unwrap();
        schema::seed_v1(&conn).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 1);

        schema::migrate(&conn).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, schema::SCHEMA_VERSION);

        conn.execute(
            "INSERT INTO setting (key, value) VALUES ('max_concurrent', '4')",
            [],
        )
        .unwrap();
        let n: String = conn
            .query_row(
                "SELECT value FROM setting WHERE key = 'max_concurrent'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, "4");
    }

    #[test]
    fn типова_черга_існує_від_початку() {
        let s = Store::in_memory().unwrap();
        let queues = s.queues().unwrap();
        assert_eq!(queues.len(), 1);
        assert_eq!(queues[0].name, DEFAULT_QUEUE);
        assert_eq!(queues[0].max_concurrent, 3);
        assert_eq!(queues[0].rate_limit, 0);
        assert!(!queues[0].paused);
    }

    #[test]
    fn створення_налаштування_перейменування_та_видалення_черги() {
        let mut s = Store::in_memory().unwrap();

        // Створення черги
        let patch = QueuePatch {
            max_concurrent: Some(5),
            rate_limit: Some(1024 * 1024),
            paused: Some(false),
            schedule_from: Some("23:00".to_owned()),
            schedule_to: Some("07:00".to_owned()),
            post_action: Some("sleep".to_owned()),
        };
        let q = s.create_queue("night", &patch).unwrap();
        assert_eq!(q.name, "night");
        assert_eq!(q.max_concurrent, 5);
        assert_eq!(q.rate_limit, 1024 * 1024);
        assert_eq!(q.schedule_from, Some(23 * 60));
        assert_eq!(q.schedule_to, Some(7 * 60));
        assert_eq!(q.post_action, PostAction::Sleep);

        // Додавання завдання до нової черги
        let mut task_req = звичайне_завдання("https://e.com/iso.zip", "iso.zip");
        task_req.queue = Some("night".to_owned());
        let tid = s.add_task(&task_req).unwrap();

        let t = s.task(tid).unwrap().unwrap();
        assert_eq!(t.queue, "night");

        let night_tasks = s.tasks_in_queue("night").unwrap();
        assert_eq!(night_tasks.len(), 1);
        assert_eq!(night_tasks[0].id, tid);

        // Оновлення параметрів черги
        let update_patch = QueuePatch {
            max_concurrent: Some(2),
            paused: Some(true),
            ..QueuePatch::default()
        };
        let updated = s.update_queue("night", &update_patch).unwrap();
        assert_eq!(updated.max_concurrent, 2);
        assert!(updated.paused);
        assert_eq!(updated.rate_limit, 1024 * 1024, "інші поля лишились незмінними");

        // Перейменування черги
        s.rename_queue("night", "nightly").unwrap();
        assert!(s.queue("night").unwrap().is_none());
        assert!(s.queue("nightly").unwrap().is_some());
        let t_after_rename = s.task(tid).unwrap().unwrap();
        assert_eq!(t_after_rename.queue, "nightly", "завдання отримало нове ім'я черги");

        // Перенесення завдання між чергами
        s.set_task_queue(tid, DEFAULT_QUEUE).unwrap();
        let t_moved = s.task(tid).unwrap().unwrap();
        assert_eq!(t_moved.queue, DEFAULT_QUEUE);

        s.set_task_queue(tid, "nightly").unwrap();

        // Видалення черги повертає завдання в default
        s.delete_queue("nightly").unwrap();
        assert!(s.queue("nightly").unwrap().is_none());
        let t_restored = s.task(tid).unwrap().unwrap();
        assert_eq!(t_restored.queue, DEFAULT_QUEUE, "після видалення черги завдання перейшло в default");

        // Заборона видалення типової черги
        assert!(s.delete_queue(DEFAULT_QUEUE).is_err());
        // Заборона перейменування типової черги
        assert!(s.rename_queue(DEFAULT_QUEUE, "other").is_err());
    }

    #[test]
    fn додавання_в_неіснуючу_чергу_заборонено() {
        let mut s = Store::in_memory().unwrap();
        let mut task_req = звичайне_завдання("https://e.com/test", "test");
        task_req.queue = Some("ghost_queue".to_owned());
        assert!(s.add_task(&task_req).is_err());
    }
}
