//! Запис у файл з багатьох потоків одночасно.
//!
//! # Чому тут немає `&mut self`
//!
//! [`SparseFile::write_at`] бере `&self`, і це навмисно: у файл одночасно
//! пишуть 16–32 воркери, кожен у свій діапазон. Так само влаштовані й самі
//! системні виклики — `seek_write` на Windows і `pwrite` на Unix беруть
//! незмінне посилання.
//!
//! ⚠️ **Компілятор тут не захищає.** Два потоки можуть передати той самий
//! `offset` — це чудово скомпілюється й тихо зіпсує файл. Єдина гарантія
//! неперетинання діапазонів — [`crate::SegmentTable`], і саме тому її
//! інваріанти перевіряються тестами після кожної операції.
//!
//! # Чому синхронний код
//!
//! Позиційний запис — це один системний виклик без очікування мережі.
//! Загортати його в async означало б платити за планувальник там, де
//! виграшу немає; рушій викликає це з `spawn_blocking`.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Файл, у який пишуть з багатьох потоків за абсолютними зсувами.
#[derive(Debug)]
pub struct SparseFile {
    file: File,
    path: PathBuf,
}

impl SparseFile {
    /// Створити або відкрити файл під завантаження.
    ///
    /// Якщо розмір відомий, місце резервується одразу: так менша
    /// фрагментація, і диск, на якому не вистачить місця, скаже про це
    /// **зараз**, а не через годину качання.
    pub fn create(path: impl AsRef<Path>, size: Option<u64>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;

        if let Some(size) = size {
            file.set_len(size)?;
        }

        Ok(Self { file, path })
    }

    /// Відкрити наявний файл для докачування, не чіпаючи вмісту.
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        Ok(Self { file, path })
    }

    /// Шлях файла.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Поточний розмір на диску.
    pub fn len(&self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// Чи файл порожній.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Записати буфер за абсолютним зсувом, повністю.
    ///
    /// Системний виклик має право записати менше, ніж просили, — тому цикл.
    /// Без нього при частковому записі у файлі лишилася б дірка, про яку
    /// ніхто б не дізнався до перевірки хеша.
    pub fn write_all_at(&self, mut offset: u64, mut buf: &[u8]) -> Result<()> {
        while !buf.is_empty() {
            let written = self.write_at(offset, buf)?;

            if written == 0 {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    format!(
                        "запис у {} на зсуві {offset} не просунувся: лишилось {} байтів",
                        self.path.display(),
                        buf.len()
                    ),
                )));
            }

            offset += written as u64;
            buf = &buf[written..];
        }
        Ok(())
    }

    /// Один позиційний запис. Повертає, скільки байтів справді лягло.
    #[cfg(windows)]
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        use std::os::windows::fs::FileExt;
        Ok(self.file.seek_write(buf, offset)?)
    }

    /// Один позиційний запис. Повертає, скільки байтів справді лягло.
    #[cfg(unix)]
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize> {
        use std::os::unix::fs::FileExt;
        Ok(self.file.write_at(buf, offset)?)
    }

    /// Прочитати за абсолютним зсувом — потрібно для перевірки після резюме.
    #[cfg(windows)]
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        use std::os::windows::fs::FileExt;
        Ok(self.file.seek_read(buf, offset)?)
    }

    /// Прочитати за абсолютним зсувом — потрібно для перевірки після резюме.
    #[cfg(unix)]
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        use std::os::unix::fs::FileExt;
        Ok(self.file.read_at(buf, offset)?)
    }

    /// Дочекатися, доки дані справді ляжуть на диск.
    ///
    /// ⚠️ Порядок фіксації при качанні жорсткий: **спершу дані, потім
    /// `sync`, і лише потім оновлений стан сегментів**. Зворотний порядок
    /// дає найгірший можливий результат — файл стану каже «завантажено», а
    /// байтів на диску немає, і після падіння живлення докачування
    /// продовжиться з дірки.
    pub fn sync(&self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }

    /// Обрізати файл до потрібного розміру.
    ///
    /// Потрібно, коли сервер збрехав у `Content-Length` і файл вийшов
    /// довшим за реальні дані.
    pub fn truncate_to(&self, size: u64) -> Result<()> {
        self.file.set_len(size)?;
        Ok(())
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Тимчасовий файл, який прибирається сам.
    struct TempPath(PathBuf);

    impl TempPath {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            p.push(format!("downloader-test-{tag}-{unique}.bin"));
            Self(p)
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn резервування_місця_дає_файл_потрібного_розміру() {
        let tmp = TempPath::new("prealloc");
        let f = SparseFile::create(&tmp.0, Some(4096)).unwrap();

        assert_eq!(f.len().unwrap(), 4096, "місце мало зарезервуватись одразу");
    }

    #[test]
    fn запис_за_зсувом_не_чіпає_сусідні_байти() {
        let tmp = TempPath::new("offset");
        let f = SparseFile::create(&tmp.0, Some(16)).unwrap();

        f.write_all_at(4, b"ABCD").unwrap();
        f.sync().unwrap();

        let data = std::fs::read(&tmp.0).unwrap();
        assert_eq!(&data[4..8], b"ABCD");
        assert_eq!(&data[0..4], &[0, 0, 0, 0], "початок мав лишитись недоторканим");
        assert_eq!(&data[8..16], &[0; 8], "хвіст мав лишитись недоторканим");
    }

    /// Головний тест модуля: 16 потоків пишуть у той самий файл одночасно,
    /// кожен у свій діапазон. Саме так працює рушій.
    #[test]
    fn шістнадцять_потоків_пишуть_одночасно_без_псування() {
        const WORKERS: u64 = 16;
        const CHUNK: u64 = 4096;
        let total = WORKERS * CHUNK;

        let tmp = TempPath::new("parallel");
        let f = Arc::new(SparseFile::create(&tmp.0, Some(total)).unwrap());

        let mut handles = Vec::new();
        for w in 0..WORKERS {
            let f = Arc::clone(&f);
            handles.push(std::thread::spawn(move || {
                // Кожен воркер пише свій байт-маркер у свій діапазон.
                let marker = u8::try_from(w + 1).unwrap_or(255);
                let buf = vec![marker; usize::try_from(CHUNK).unwrap()];
                f.write_all_at(w * CHUNK, &buf).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap_or_else(|_| panic!("воркер упав"));
        }
        f.sync().unwrap();

        let data = std::fs::read(&tmp.0).unwrap();
        assert_eq!(data.len(), usize::try_from(total).unwrap());

        // Кожен діапазон має бути заповнений рівно своїм маркером: жодного
        // перемішування, жодної дірки.
        for w in 0..WORKERS {
            let start = usize::try_from(w * CHUNK).unwrap();
            let end = start + usize::try_from(CHUNK).unwrap();
            let marker = u8::try_from(w + 1).unwrap_or(255);

            assert!(
                data[start..end].iter().all(|&b| b == marker),
                "діапазон воркера {w} зіпсовано — байти перемішались"
            );
        }
    }

    #[test]
    fn запис_частинами_склеюється_у_суцільні_дані() {
        let tmp = TempPath::new("pieces");
        let f = SparseFile::create(&tmp.0, Some(9)).unwrap();

        // Навмисно не по порядку — рушій пише саме так.
        f.write_all_at(6, b"789").unwrap();
        f.write_all_at(0, b"123").unwrap();
        f.write_all_at(3, b"456").unwrap();
        f.sync().unwrap();

        assert_eq!(std::fs::read(&tmp.0).unwrap(), b"123456789");
    }

    #[test]
    fn читання_повертає_записане() {
        let tmp = TempPath::new("readback");
        let f = SparseFile::create(&tmp.0, Some(32)).unwrap();
        let payload = "перевірка".as_bytes();
        f.write_all_at(10, payload).unwrap();
        f.sync().unwrap();

        let mut buf = vec![0u8; payload.len()];
        let n = f.read_at(10, &mut buf).unwrap();

        assert_eq!(n, buf.len());
        assert_eq!(String::from_utf8_lossy(&buf), "перевірка");
    }

    #[test]
    fn обрізання_прибирає_зайве_після_брехливої_довжини() {
        let tmp = TempPath::new("truncate");
        // Сервер заявив 1000, а віддав 10 — файл треба вкоротити.
        let f = SparseFile::create(&tmp.0, Some(1000)).unwrap();
        f.write_all_at(0, b"0123456789").unwrap();

        f.truncate_to(10).unwrap();
        f.sync().unwrap();

        assert_eq!(f.len().unwrap(), 10);
        assert_eq!(std::fs::read(&tmp.0).unwrap(), b"0123456789");
    }

    #[test]
    fn докачування_не_затирає_вже_завантажене() {
        let tmp = TempPath::new("resume");
        {
            let f = SparseFile::create(&tmp.0, Some(8)).unwrap();
            f.write_all_at(0, b"ABCD").unwrap();
            f.sync().unwrap();
        }

        // Друга сесія відкриває той самий файл і дописує хвіст.
        let f = SparseFile::open_existing(&tmp.0).unwrap();
        f.write_all_at(4, b"EFGH").unwrap();
        f.sync().unwrap();

        assert_eq!(
            std::fs::read(&tmp.0).unwrap(),
            b"ABCDEFGH",
            "відкриття для докачування не має обнуляти файл"
        );
    }
}
