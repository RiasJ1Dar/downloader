//! Mark-of-the-Web — мітка «це прийшло з інтернету».
//!
//! # Чому це обов'язково, а не приємна дрібниця
//!
//! Windows позначає завантажені файли альтернативним потоком
//! `Zone.Identifier`. За ним працюють SmartScreen, «захищений перегляд» в
//! Office і попередження при запуску `.exe`. Браузери ставлять цю мітку
//! завжди.
//!
//! Менеджер завантажень, який її **не ставить**, — це інструмент обходу
//! SmartScreen. Файл, завантажений ним, запускається без жодного
//! попередження. Наслідок передбачуваний: антивіруси починають вважати саму
//! програму засобом доставки шкідливого коду, і вона потрапляє в детект.
//!
//! Тобто мітка потрібна не «для галочки» — без неї продукт, який ми
//! роздаємо людям, стає небезпечним і для них, і для себе.
//!
//! # Як це влаштовано
//!
//! `Zone.Identifier` — звичайний альтернативний потік NTFS. Записати його
//! можна як файл `<шлях>:Zone.Identifier`, без COM і без WinAPI:
//!
//! ```text
//! [ZoneTransfer]
//! ZoneId=3
//! ReferrerUrl=https://example.com/page
//! HostUrl=https://cdn.example.com/file.zip
//! ```
//!
//! `ZoneId=3` означає «інтернет». Саме його ставлять браузери.

use std::path::Path;

/// Зона, з якої прийшов файл.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Zone {
    /// Локальний комп'ютер.
    Local = 0,
    /// Локальна мережа.
    Intranet = 1,
    /// Довірений вузол.
    Trusted = 2,
    /// **Інтернет.** Те, що ставимо ми.
    Internet = 3,
    /// Обмежений вузол.
    Restricted = 4,
}

/// Помилки позначення.
#[derive(Debug, thiserror::Error)]
pub enum MotwError {
    #[error("не вдалося позначити {path} як завантажений з інтернету: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Позначити файл як завантажений із мережі.
///
/// `source_url` — звідки саме приїхали байти, `referrer` — сторінка, з якої
/// людина почала. Обидва потрапляють у потік і видимі у властивостях файла;
/// це те, що потім дозволяє з'ясувати походження підозрілого файла.
///
/// ⚠️ На файлових системах без підтримки потоків (FAT32, exFAT, мережеві
/// диски) запис не вдасться. Це **не привід валити завантаження**: файл уже
/// на диску й цілий. Але й мовчати не можна — повертаємо помилку, щоб
/// викликач вирішив сам і записав це в журнал.
#[cfg(windows)]
pub fn mark(
    path: &Path,
    source_url: Option<&str>,
    referrer: Option<&str>,
) -> Result<(), MotwError> {
    use std::io::Write as _;

    let stream = format!("{}:Zone.Identifier", path.display());

    let mut content = String::from("[ZoneTransfer]\r\nZoneId=3\r\n");
    if let Some(r) = referrer {
        content.push_str(&format!("ReferrerUrl={r}\r\n"));
    }
    if let Some(u) = source_url {
        content.push_str(&format!("HostUrl={u}\r\n"));
    }

    let mut f = std::fs::File::create(&stream).map_err(|source| MotwError::Write {
        path: path.display().to_string(),
        source,
    })?;

    f.write_all(content.as_bytes())
        .map_err(|source| MotwError::Write {
            path: path.display().to_string(),
            source,
        })
}

/// На не-Windows мітки немає — і це не помилка.
#[cfg(not(windows))]
pub fn mark(
    _path: &Path,
    _source_url: Option<&str>,
    _referrer: Option<&str>,
) -> Result<(), MotwError> {
    Ok(())
}

/// Прочитати наявну мітку. Повертає вміст потоку, якщо він є.
///
/// Потрібно для перевірок і діагностики: «а чи справді ми позначили».
#[cfg(windows)]
#[must_use]
pub fn read_mark(path: &Path) -> Option<String> {
    let stream = format!("{}:Zone.Identifier", path.display());
    std::fs::read_to_string(stream).ok()
}

/// На не-Windows мітки не буває.
#[cfg(not(windows))]
#[must_use]
pub fn read_mark(_path: &Path) -> Option<String> {
    None
}

/// Чи файл позначений як завантажений з інтернету.
#[must_use]
pub fn is_marked_internet(path: &Path) -> bool {
    read_mark(path).is_some_and(|text| {
        text.lines()
            .any(|line| line.trim().eq_ignore_ascii_case("ZoneId=3"))
    })
}

#[cfg(all(test, windows))]
#[expect(
    clippy::unwrap_used,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Temp(PathBuf);

    impl Temp {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            p.push(format!("motw-test-{tag}-{unique}.bin"));
            std::fs::write(&p, b"payload").unwrap();
            Self(p)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            drop(std::fs::remove_file(&self.0));
        }
    }

    #[test]
    fn позначений_файл_має_зону_інтернету() {
        let tmp = Temp::new("basic");
        assert!(
            !is_marked_internet(&tmp.0),
            "щойно створений файл не мав бути позначений"
        );

        mark(&tmp.0, Some("https://cdn.example.com/f.bin"), None).unwrap();

        assert!(
            is_marked_internet(&tmp.0),
            "після позначення SmartScreen має бачити зону інтернету"
        );
    }

    #[test]
    fn мітка_несе_джерело_і_сторінку() {
        let tmp = Temp::new("urls");
        mark(
            &tmp.0,
            Some("https://cdn.example.com/f.bin"),
            Some("https://example.com/page"),
        )
        .unwrap();

        let text = read_mark(&tmp.0).unwrap();
        assert!(
            text.contains("HostUrl=https://cdn.example.com/f.bin"),
            "{text}"
        );
        assert!(
            text.contains("ReferrerUrl=https://example.com/page"),
            "{text}"
        );
        assert!(text.contains("ZoneId=3"), "{text}");
    }

    #[test]
    fn позначення_не_чіпає_вміст_файла() {
        let tmp = Temp::new("content");
        mark(&tmp.0, Some("https://example.com/f"), None).unwrap();

        assert_eq!(
            std::fs::read(&tmp.0).unwrap(),
            b"payload",
            "мітка живе в окремому потоці й не має торкатись самих даних"
        );
    }

    #[test]
    fn повторне_позначення_перезаписує_а_не_дублює() {
        let tmp = Temp::new("twice");
        mark(&tmp.0, Some("https://перший.example/f"), None).unwrap();
        mark(&tmp.0, Some("https://другий.example/f"), None).unwrap();

        let text = read_mark(&tmp.0).unwrap();
        assert!(!text.contains("перший"), "стара мітка лишилась: {text}");
        assert_eq!(
            text.matches("[ZoneTransfer]").count(),
            1,
            "секція продубльована: {text}"
        );
    }

    #[test]
    fn непозначений_файл_розпізнається() {
        let tmp = Temp::new("clean");
        assert!(!is_marked_internet(&tmp.0));
        assert!(read_mark(&tmp.0).is_none());
    }
}
