//! Вільне місце на диску призначення — до старту завантаження.
//!
//! Шлях файла ще може не існувати (тека з'явиться пізніше). `fs2` питає
//! файлову систему за живим шляхом, тому піднімаємось до найближчого предка.
//! Запас 64 МіБ, не гігабайт як у DLMan: ми пишемо одразу в цільовий файл,
//! без другої копії сегментів.

use std::path::{Path, PathBuf};

/// Запас поверх розміру файла, щоб не забити том вщент.
const ЗАПАС_БАЙТ: u64 = 64 * 1024 * 1024;

/// Помилки перевірки вільного місця.
///
/// Текст називає симптом: скільки є і скільки треба, або чому диск не
/// вдалося запитати. Викликач покаже це перед стартом качання.
#[derive(Debug, thiserror::Error)]
pub enum DiskError {
    /// Жоден предок шляху не існує — питати нічого.
    #[error("немає існуючого диска для {path}")]
    NoAncestor { path: String },
    /// `available_space` не зміг прочитати том.
    #[error("не вдалося визначити вільне місце для {path}: {source}")]
    Query {
        path: String,
        #[source]
        source: std::io::Error,
    },
    /// Вільно менше, ніж файл плюс запас.
    #[error("не вистачає місця на диску: вільно {free} байт, потрібно {need} байт")]
    NotEnough { free: u64, need: u64 },
}

/// Найближчий існуючий предок — том, куди ляже файл.
fn існуючий_предок(path: &Path) -> Result<PathBuf, DiskError> {
    let mut current = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|source| DiskError::Query {
                path: path.display().to_string(),
                source,
            })?
    };

    loop {
        if current.exists() {
            return Ok(current);
        }
        if !current.pop() {
            return Err(DiskError::NoAncestor {
                path: path.display().to_string(),
            });
        }
    }
}

/// Порівняти вже зняті числа: `need == 0` не перевіряємо, інакше файл + запас.
pub(crate) fn порівняти_місце(вільно: u64, need: u64) -> Result<(), DiskError> {
    if need == 0 {
        return Ok(());
    }
    let потрібно = need.saturating_add(ЗАПАС_БАЙТ);
    if вільно >= потрібно {
        Ok(())
    } else {
        Err(DiskError::NotEnough {
            free: вільно,
            need: потрібно,
        })
    }
}

/// Скільки байт ще можна зайняти не-привілейованому користувачу на томі `path`.
///
/// Якщо `path` ще немає — питаємо предка, що вже є.
pub fn вільні_байти(path: &Path) -> Result<u64, DiskError> {
    let предок = існуючий_предок(path)?;
    fs2::available_space(&предок).map_err(|source| DiskError::Query {
        path: path.display().to_string(),
        source,
    })
}

/// Чи вистачить місця під `need` байт (плюс 64 МіБ запасу).
///
/// `need == 0` — розмір ще невідомий, перевірку пропускаємо.
pub fn вистачить_місця(path: &Path, need: u64) -> Result<(), DiskError> {
    if need == 0 {
        return Ok(());
    }
    порівняти_місце(вільні_байти(path)?, need)
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]
mod tests {
    use super::*;

    #[test]
    fn байт_на_тимчасовій_теці_вистачає() {
        вистачить_місця(&std::env::temp_dir(), 1).unwrap();
    }

    #[test]
    fn нульовий_розмір_не_питає_диск() {
        let missing = Path::new("Z:\\e12-немає-такого\\file.bin");
        вистачить_місця(missing, 0).unwrap();
    }

    #[test]
    fn неіснуючий_файл_бере_предка() {
        let missing = std::env::temp_dir()
            .join("e12-немає-такої-теки")
            .join("file.bin");
        assert!(
            !missing.exists(),
            "фікстура має бути шляхом, якого ще немає"
        );
        вільні_байти(&missing).unwrap();
    }

    #[test]
    fn мало_місця_називає_вільні_і_потрібні_байти() {
        let err = порівняти_місце(100, 1).unwrap_err();
        assert_eq!(
            err.to_string(),
            "не вистачає місця на диску: вільно 100 байт, потрібно 67108865 байт"
        );
        assert!(
            matches!(
                err,
                DiskError::NotEnough { free: 100, need } if need == 1 + ЗАПАС_БАЙТ
            ),
            "очікували NotEnough, маємо {err:?}"
        );
    }

    #[test]
    fn запас_64_міб_входить_у_потрібне() {
        assert!(порівняти_місце(ЗАПАС_БАЙТ, 1).is_err());
        порівняти_місце(ЗАПАС_БАЙТ + 1, 1).unwrap();
        порівняти_місце(0, 0).unwrap();
        assert!(порівняти_місце(1_000_000, u64::MAX / 2).is_err());
    }

    #[test]
    fn немає_предка_це_український_симптом() {
        let err = DiskError::NoAncestor {
            path: "Q:\\немає".into(),
        };
        assert_eq!(err.to_string(), "немає існуючого диска для Q:\\немає");
    }
}
