//! Сховище завдань: SQLite.
//!
//! Тут живе те, що людина бачить у списку: завдання, їхні файли, категорії,
//! історія. Прогрес самого качання — окремо, у файлі `.dlpart` поруч із
//! файлом ([`crate::state`]): він оновлюється десятки разів на секунду й не
//! має права блокувати базу.
//!
//! Розподіл простий: **база — це що качаємо, sidecar — це докуди дійшли**.

pub mod schema;

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};
use crate::segments::{Segment, SegmentTable};

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

        let now = now_ms();
        let tx = self.conn.transaction().map_err(db)?;

        tx.execute(
            "INSERT INTO task (url, protocol, status, title, category_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![
                new.url,
                new.protocol,
                Status::Queued.as_str(),
                new.title,
                new.category_id,
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
                        created_at, updated_at, finished_at, error
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

    fn tasks_where(&self, status: Option<Status>) -> Result<Vec<Task>> {
        let sql = "SELECT id, url, protocol, status, title, category_id,
                          created_at, updated_at, finished_at, error
                   FROM task";

        let mut out = Vec::new();

        match status {
            Some(s) => {
                let mut stmt = self
                    .conn
                    .prepare(&format!("{sql} WHERE status = ?1 ORDER BY id DESC"))
                    .map_err(db)?;
                let rows = stmt.query_map(params![s.as_str()], task_from_row).map_err(db)?;
                for r in rows {
                    out.push(r.map_err(db)??);
                }
            }
            None => {
                let mut stmt = self
                    .conn
                    .prepare(&format!("{sql} ORDER BY id DESC"))
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
            .prepare(
                "SELECT id, task_id, idx, path, size, fingerprint, selected, done
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
                })
            })
            .map_err(db)?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(db)?);
        }
        Ok(out)
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

        for seg in table.segments() {
            tx.execute(
                "INSERT INTO segment (file_id, seg_id, start, end, done)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    file_id,
                    seg.id as i64,
                    seg.start as i64,
                    seg.end as i64,
                    seg.done as i64
                ],
            )
            .map_err(db)?;
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
            .prepare(
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
            .prepare("SELECT id, name, folder, extensions FROM category ORDER BY name")
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

    fn звичайне_завдання(url: &str, path: &str) -> NewTask {
        NewTask {
            url: url.to_owned(),
            protocol: "http".to_owned(),
            title: None,
            category_id: None,
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
}
