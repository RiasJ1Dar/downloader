//! Стан завантаження на диску — те, завдяки чому докачування переживає
//! вимкнене живлення.
//!
//! # Головне правило: стан **відстає** від диска, ніколи не випереджає
//!
//! Порядок запису жорсткий:
//!
//! 1. байти у файл;
//! 2. `sync` — дочекатись, доки вони справді на диску;
//! 3. і аж тоді новий стан.
//!
//! Зворотний порядок дає найгірший можливий результат: стан каже
//! «завантажено 40 МБ», а на диску їх немає — і докачування продовжиться з
//! **дірки**, якої ніхто не помітить, доки людина не відкриє файл.
//!
//! Відставання ж безпечне: у найгіршому разі ми перекачаємо кілька останніх
//! мегабайтів удруге. Зайва робота проти битого файла — обмін, який не
//! обговорюється.
//!
//! # Чому запис атомарний
//!
//! Стан пишеться у сусідній файл і **перейменовується** поверх старого.
//! Якщо живлення зникне посеред запису, лишиться цілий старий стан, а не
//! обрізаний новий. Половина JSON — це те саме, що відсутність стану, тільки
//! гірше: вона виглядає як щось придатне.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::segments::{Segment, SegmentTable};

/// Версія формату файла стану.
///
/// Зростає, коли структура змінюється несумісно. Стан старшої версії ми
/// **не намагаємось прочитати** — краще перекачати, ніж витлумачити чужі
/// поля навмання.
pub const STATE_VERSION: u32 = 1;

/// Розширення файла стану поруч із цільовим файлом.
pub const STATE_SUFFIX: &str = "dlpart";

/// Знімок завантаження, який переживає перезапуск.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadState {
    /// Версія формату.
    pub version: u32,
    /// Який модуль писав цей стан: `http`, згодом `torrent` тощо.
    ///
    /// Ядро сюди не заглядає — воно лише звіряє, що стан писав той самий
    /// протокол, який тепер його читає.
    pub protocol: String,
    /// Джерело. Змінилось — стан чужий.
    pub url: String,
    /// Повний розмір, якщо відомий.
    pub total: Option<u64>,
    /// Ознака версії ресурсу очима протоколу.
    ///
    /// Для HTTP це `ETag` або `Last-Modified`. Ядро не знає й не має знати,
    /// що саме тут лежить: його справа — порівняти рядки.
    pub fingerprint: Option<String>,
    /// Непрозорий стан протоколу.
    ///
    /// Закладено під торент: там докачування — не `(offset, len)`, а бітова
    /// карта частин. Ядро зберігає це, не тлумачачи.
    pub opaque: Option<String>,
    /// Сегменти на момент останнього чекпоінта.
    pub segments: Vec<Segment>,
}

impl DownloadState {
    /// Зібрати стан із таблиці.
    #[must_use]
    pub fn from_table(
        protocol: impl Into<String>,
        url: impl Into<String>,
        table: &SegmentTable,
        fingerprint: Option<String>,
    ) -> Self {
        Self {
            version: STATE_VERSION,
            protocol: protocol.into(),
            url: url.into(),
            total: Some(table.total()),
            fingerprint,
            opaque: None,
            segments: table.segments().to_vec(),
        }
    }

    /// Скільки байтів було завантажено на момент знімка.
    #[must_use]
    pub fn downloaded(&self) -> u64 {
        self.segments.iter().map(|s| s.done).sum()
    }

    /// Чи цей стан можна застосувати до нового завантаження.
    ///
    /// Розбіжність у будь-чому — не привід гадати. Повертаємо причину, щоб
    /// у журналі було видно, **чому** довелось качати з нуля: «просто
    /// почалось спочатку» — найдратівливіше повідомлення на світі.
    pub fn accepts(
        &self,
        protocol: &str,
        url: &str,
        total: Option<u64>,
        fingerprint: Option<&str>,
    ) -> std::result::Result<(), StateMismatch> {
        if self.version != STATE_VERSION {
            return Err(StateMismatch::Version {
                found: self.version,
                expected: STATE_VERSION,
            });
        }
        if self.protocol != protocol {
            return Err(StateMismatch::Protocol {
                found: self.protocol.clone(),
                expected: protocol.to_owned(),
            });
        }
        if self.url != url {
            return Err(StateMismatch::Url);
        }
        if self.total != total {
            return Err(StateMismatch::Size {
                found: self.total,
                expected: total,
            });
        }

        // Ознаки версії ресурсу мають збігатися. Якщо сервер раніше давав
        // `ETag`, а тепер не дає (або навпаки) — вважаємо це зміною: без
        // ознаки докачування все одно було б наосліп.
        match (self.fingerprint.as_deref(), fingerprint) {
            (Some(a), Some(b)) if a == b => Ok(()),
            (None, None) => Ok(()),
            _ => Err(StateMismatch::Fingerprint),
        }
    }

    /// Відновити таблицю сегментів зі стану.
    ///
    /// Перевіряє інваріанти: стан на диску міг зіпсуватись, і таблиця з
    /// дірками дала б файл із дірками.
    pub fn into_table(self) -> Result<SegmentTable> {
        let total = self.total.unwrap_or_else(|| {
            self.segments
                .iter()
                .map(|s| s.end)
                .max()
                .unwrap_or_default()
        });
        SegmentTable::from_parts(self.segments, total)
    }
}

/// Чому збережений стан не підійшов.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StateMismatch {
    #[error("файл стану версії {found}, а ми розуміємо {expected}")]
    Version { found: u32, expected: u32 },

    #[error("файл стану писав протокол {found}, а качає {expected}")]
    Protocol { found: String, expected: String },

    #[error("файл стану належить іншому посиланню")]
    Url,

    #[error("розмір змінився: у стані {found:?}, тепер {expected:?}")]
    Size {
        found: Option<u64>,
        expected: Option<u64>,
    },

    #[error("ресурс на сервері змінився — докачувати не можна")]
    Fingerprint,
}

/// Шлях до файла стану поруч із цільовим файлом.
#[must_use]
pub fn state_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(STATE_SUFFIX);
    target.with_file_name(name)
}

/// Записати стан **атомарно**: спершу сусідній файл, потім перейменування.
///
/// ⚠️ Викликати лише **після** `sync` цільового файла. Інакше стан
/// випереджатиме дані, і після падіння живлення докачування піде з дірки.
pub fn save(target: &Path, state: &DownloadState) -> Result<()> {
    let path = state_path(target);
    let tmp = path.with_extension(format!("{STATE_SUFFIX}.tmp"));

    let text = serde_json::to_vec_pretty(state).map_err(|e| {
        Error::Io(std::io::Error::other(format!(
            "не вдалося скласти файл стану для {}: {e}",
            target.display()
        )))
    })?;

    {
        use std::io::Write as _;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&text)?;
        // Без цього перейменування може випередити самі дані.
        f.sync_all()?;
    }

    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Прочитати стан, якщо він є і читається.
///
/// Пошкоджений файл стану — не помилка завантаження: просто качаємо з нуля.
/// Але про це треба **сказати**, а не проковтнути, тому повертається причина.
pub fn load(target: &Path) -> std::result::Result<DownloadState, LoadError> {
    let path = state_path(target);

    let data = std::fs::read(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            LoadError::Absent
        } else {
            LoadError::Unreadable(e.to_string())
        }
    })?;

    serde_json::from_slice(&data).map_err(|e| LoadError::Corrupt(e.to_string()))
}

/// Чому стан не вдалося прочитати.
#[derive(Debug, Clone, thiserror::Error)]
pub enum LoadError {
    /// Файла немає — звичайна річ для першого запуску.
    #[error("файла стану немає")]
    Absent,

    #[error("файл стану не читається: {0}")]
    Unreadable(String),

    #[error("файл стану пошкоджено: {0}")]
    Corrupt(String),
}

/// Прибрати стан після успішного завантаження.
///
/// Помилка тут не критична — файл лише займає місце, — але мовчати про неї
/// не варто: сміття поруч із завантаженням спантеличує людину.
pub fn remove(target: &Path) -> Result<()> {
    let path = state_path(target);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Io(e)),
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::let_underscore_must_use,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]
mod tests {
    use super::*;

    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            p.push(format!("state-test-{tag}-{unique}.bin"));
            Self(p)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(state_path(&self.0));
        }
    }

    fn зразок() -> (SegmentTable, DownloadState) {
        let mut table = SegmentTable::new(1000, 4, 1);
        table.advance(0, 100).unwrap();
        table.advance(2, 50).unwrap();
        let state = DownloadState::from_table(
            "http",
            "https://example.com/f.bin",
            &table,
            Some("\"abc\"".into()),
        );
        (table, state)
    }

    #[test]
    fn стан_обходить_диск_і_повертається_таким_самим() {
        let tmp = Temp::new("roundtrip");
        let (table, state) = зразок();

        save(&tmp.0, &state).unwrap();
        let back = load(&tmp.0).unwrap();

        assert_eq!(back.downloaded(), 150);
        assert_eq!(back.segments.len(), table.len());
        assert_eq!(back.url, "https://example.com/f.bin");

        let restored = back.into_table().unwrap();
        assert_eq!(restored.downloaded(), table.downloaded());
        assert_eq!(restored.total(), 1000);
    }

    #[test]
    fn файл_стану_лежить_поруч_із_цільовим() {
        let p = state_path(Path::new("C:/dl/звіт.pdf"));
        assert_eq!(
            p.file_name().unwrap().to_string_lossy(),
            "звіт.pdf.dlpart",
            "стан має бути видно поруч і зрозуміло, до чого він"
        );
    }

    #[test]
    fn відсутній_стан_це_не_помилка_а_окремий_випадок() {
        let tmp = Temp::new("absent");
        assert!(matches!(load(&tmp.0), Err(LoadError::Absent)));
    }

    #[test]
    fn пошкоджений_стан_розпізнається_а_не_тлумачиться() {
        let tmp = Temp::new("corrupt");
        std::fs::write(state_path(&tmp.0), "{ це не json".as_bytes()).unwrap();

        match load(&tmp.0) {
            Err(LoadError::Corrupt(msg)) => assert!(!msg.is_empty()),
            other => panic!("пошкоджений файл мав дати Corrupt, а дав {other:?}"),
        }
    }

    #[test]
    fn змінений_etag_забороняє_докачування() {
        let (_, state) = зразок();

        let err = state
            .accepts(
                "http",
                "https://example.com/f.bin",
                Some(1000),
                Some("\"інший\""),
            )
            .expect_err("ознака ресурсу змінилась — докачувати не можна");

        assert_eq!(err, StateMismatch::Fingerprint);
    }

    #[test]
    fn зниклий_etag_теж_вважається_зміною() {
        let (_, state) = зразок();
        // Раніше сервер давав ознаку, тепер не дає — далі наосліп не йдемо.
        let err = state
            .accepts("http", "https://example.com/f.bin", Some(1000), None)
            .expect_err("без ознаки докачування було б навмання");
        assert_eq!(err, StateMismatch::Fingerprint);
    }

    #[test]
    fn інший_розмір_забороняє_докачування() {
        let (_, state) = зразок();
        let err = state
            .accepts(
                "http",
                "https://example.com/f.bin",
                Some(999),
                Some("\"abc\""),
            )
            .expect_err("розмір змінився");
        assert!(matches!(err, StateMismatch::Size { .. }));
    }

    #[test]
    fn чужий_протокол_не_приймається() {
        let (_, state) = зразок();
        let err = state
            .accepts(
                "torrent",
                "https://example.com/f.bin",
                Some(1000),
                Some("\"abc\""),
            )
            .expect_err("стан писав інший протокол");
        assert!(matches!(err, StateMismatch::Protocol { .. }));
    }

    #[test]
    fn збіг_усього_дозволяє_докачування() {
        let (_, state) = зразок();
        assert!(
            state
                .accepts(
                    "http",
                    "https://example.com/f.bin",
                    Some(1000),
                    Some("\"abc\"")
                )
                .is_ok()
        );
    }

    #[test]
    fn стан_із_дірками_не_перетворюється_на_таблицю() {
        let broken = DownloadState {
            version: STATE_VERSION,
            protocol: "http".into(),
            url: "https://example.com/f.bin".into(),
            total: Some(100),
            fingerprint: None,
            opaque: None,
            segments: vec![
                Segment { id: 0, start: 0, end: 10, done: 10 },
                Segment { id: 1, start: 50, end: 100, done: 0 },
            ],
        };

        assert!(
            broken.into_table().is_err(),
            "склейка по такій таблиці дала б файл із діркою"
        );
    }

    #[test]
    fn перезапис_стану_не_лишає_тимчасового_сміття() {
        let tmp = Temp::new("atomic");
        let (_, state) = зразок();

        save(&tmp.0, &state).unwrap();
        save(&tmp.0, &state).unwrap();

        let leftovers = state_path(&tmp.0).with_extension(format!("{STATE_SUFFIX}.tmp"));
        assert!(
            !leftovers.exists(),
            "тимчасовий файл мав зникнути при перейменуванні"
        );
    }

    #[test]
    fn прибирання_відсутнього_стану_не_помилка() {
        let tmp = Temp::new("cleanup");
        assert!(remove(&tmp.0).is_ok(), "нема чого прибирати — теж успіх");
    }

    #[test]
    fn тисяча_зіпсованих_станів_не_виглядають_як_валідні() {
        let tmp = Temp::new("crash1000");
        let (_, state) = зразок();
        save(&tmp.0, &state).unwrap();
        let path = state_path(&tmp.0);
        let intact = std::fs::read(&path).unwrap();
        assert!(!intact.is_empty());

        for i in 0..1000u32 {
            let cut = (i as usize) % intact.len();
            std::fs::write(&path, &intact[..cut]).unwrap();
            match load(&tmp.0) {
                Err(LoadError::Corrupt(_) | LoadError::Unreadable(_) | LoadError::Absent) => {}
                Ok(_) => panic!("обрізаний стан на кроці {i} прийнято як валідний"),
            }
        }

        save(&tmp.0, &state).unwrap();
        assert_eq!(load(&tmp.0).unwrap().downloaded(), 150);
    }
}
