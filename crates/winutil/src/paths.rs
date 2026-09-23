//! Довгі шляхи й унікальні імена.

use std::path::{Path, PathBuf};

/// Межа старого API Windows.
#[cfg(windows)]
const MAX_PATH: usize = 260;

/// Дати шляху форму, яку Windows прийме навіть коли він довгий.
///
/// ⚠️ Префікс `\\?\` — не вся правда. Щоб довгі шляхи справді працювали,
/// потрібно **і** ввімкнений `LongPathsEnabled` у реєстрі, **і**
/// `longPathAware` у маніфесті програми. Префікс рятує там, де ці двоє не
/// склались, бо вимикає обробку шляху старим API взагалі.
///
/// Наслідок префікса: шлях перестає бути «нормалізованим». `..` і `.` у
/// ньому більше не розкриваються системою, тож подавати сюди можна лише
/// абсолютний шлях, який ми вже склали самі.
#[cfg(windows)]
#[must_use]
pub fn long_path(path: &Path) -> PathBuf {
    let text = path.display().to_string();

    if text.len() < MAX_PATH || text.starts_with("\\\\?\\") {
        return path.to_path_buf();
    }

    // UNC-шлях має власну форму префікса.
    if let Some(rest) = text.strip_prefix("\\\\") {
        return PathBuf::from(format!("\\\\?\\UNC\\{rest}"));
    }

    PathBuf::from(format!("\\\\?\\{text}"))
}

/// На інших системах довжина шляху так не обмежена.
#[cfg(not(windows))]
#[must_use]
pub fn long_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Підібрати ім'я, яке ще не зайняте: `звіт.pdf` → `звіт (1).pdf`.
///
/// Саме так поводяться браузери, і люди цього очікують. Мовчазний перезапис
/// чужого файла — найгірший варіант: він знищує дані, яких ніхто не просив
/// чіпати.
#[must_use]
pub fn unique_path(desired: &Path) -> PathBuf {
    if !desired.exists() {
        return desired.to_path_buf();
    }

    let parent = desired.parent().unwrap_or_else(|| Path::new("."));
    let name = desired
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| crate::names::ЗАПАСНЕ_ІМʼЯ.to_owned());

    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && !e.is_empty() => (s.to_owned(), Some(e.to_owned())),
        _ => (name.clone(), None),
    };

    // Верхня межа навмисна: якщо в теці вже тисяча однойменних файлів, це не
    // нормальна робота, а цикл, який хтось запустив помилково.
    for n in 1..=1000 {
        let candidate = match &ext {
            Some(e) => parent.join(format!("{stem} ({n}).{e}")),
            None => parent.join(format!("{stem} ({n})")),
        };
        if !candidate.exists() {
            return candidate;
        }
    }

    // Тисяча зайнятих — додаємо мітку часу й не крутимось далі.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    match ext {
        Some(e) => parent.join(format!("{stem} ({stamp}).{e}")),
        None => parent.join(format!("{stem} ({stamp})")),
    }
}

/// Назва мітки портативного режиму поруч із виконуваним файлом.
pub const PORTABLE_MARKER: &str = "portable.txt";

/// Перевірити, чи тека містить маркер `portable.txt`.
#[must_use]
pub fn is_portable_dir(dir: &Path) -> bool {
    dir.join(PORTABLE_MARKER).is_file()
}

/// Перевірити, чи активний портативний режим.
///
/// Портативний режим увімкнено, якщо поруч із поточним виконуваним файлом
/// (`std::env::current_exe()`) існує файл `portable.txt`.
#[must_use]
pub fn is_portable() -> bool {
    portable_marker_dir().is_some()
}

/// Повернути теку, в якій знайдено маркер `portable.txt` поруч із exe (якщо є).
#[must_use]
pub fn portable_marker_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    if is_portable_dir(dir) {
        Some(dir.to_path_buf())
    } else {
        None
    }
}

/// Визначити теку даних програми:
/// - Якщо передано шлях до exe (або знайдено поточний exe) і поруч є `portable.txt` → тека бінарника.
/// - Інакше → [`default_data_dir`].
#[must_use]
pub fn data_dir_for_exe(exe: Option<&Path>) -> PathBuf {
    if let Some(exe_path) = exe
        && let Some(parent) = exe_path.parent()
        && is_portable_dir(parent)
    {
        return parent.to_path_buf();
    }
    if let Some(dir) = portable_marker_dir() {
        return dir;
    }

    default_data_dir()
}

/// Тека даних програми для поточного процесу.
#[must_use]
pub fn app_data_dir() -> PathBuf {
    data_dir_for_exe(std::env::current_exe().ok().as_deref())
}

/// Типова тека даних без портативного режиму.
///
/// * Windows: `%LOCALAPPDATA%\Downloader`
/// * Linux: `$XDG_DATA_HOME/downloader` або `$HOME/.local/share/Downloader`
/// * macOS: `$HOME/Library/Application Support/Downloader`
#[must_use]
pub fn default_data_dir() -> PathBuf {
    default_data_dir_os()
}

#[cfg(windows)]
fn default_data_dir_os() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map_or_else(
        || PathBuf::from("."),
        |base| PathBuf::from(base).join("Downloader"),
    )
}

#[cfg(target_os = "linux")]
fn default_data_dir_os() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("downloader");
    }
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from("."),
        |home| PathBuf::from(home).join(".local/share/Downloader"),
    )
}

#[cfg(target_os = "macos")]
fn default_data_dir_os() -> PathBuf {
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from("."),
        |home| PathBuf::from(home).join("Library/Application Support/Downloader"),
    )
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn default_data_dir_os() -> PathBuf {
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from("."),
        |home| PathBuf::from(home).join(".local/share/Downloader"),
    )
}

/// Типова тека завантажень:
/// - У портативному режимі → `Downloads` поруч із виконуваним файлом.
/// - У звичайному режимі → `%USERPROFILE%\Downloads` (або `$HOME/Downloads`).
#[must_use]
pub fn downloads_dir_for_exe(exe: Option<&Path>) -> PathBuf {
    if let Some(exe_path) = exe
        && let Some(parent) = exe_path.parent()
        && is_portable_dir(parent)
    {
        return parent.join("Downloads");
    }
    if let Some(dir) = portable_marker_dir() {
        return dir.join("Downloads");
    }

    default_downloads_dir()
}

/// Типова тека завантажень програми у звичайному режимі.
#[must_use]
pub fn default_downloads_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(
            || PathBuf::from("."),
            |home| PathBuf::from(home).join("Downloads"),
        )
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let mut p = std::env::temp_dir();
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            p.push(format!("paths-test-{tag}-{unique}"));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.0));
        }
    }

    #[test]
    fn вільне_імʼя_лишається_як_є() {
        let dir = TempDir::new("free");
        let want = dir.0.join("звіт.pdf");
        assert_eq!(unique_path(&want), want);
    }

    #[test]
    fn зайняте_імʼя_отримує_номер() {
        let dir = TempDir::new("taken");
        let want = dir.0.join("звіт.pdf");
        std::fs::write(&want, b"x").unwrap();

        let got = unique_path(&want);
        assert_eq!(
            got.file_name().unwrap().to_string_lossy(),
            "звіт (1).pdf",
            "мовчазний перезапис знищив би чужий файл"
        );
    }

    #[test]
    fn номер_зростає_доки_не_знайдеться_вільний() {
        let dir = TempDir::new("many");
        let want = dir.0.join("f.bin");
        std::fs::write(&want, b"x").unwrap();
        std::fs::write(dir.0.join("f (1).bin"), b"x").unwrap();
        std::fs::write(dir.0.join("f (2).bin"), b"x").unwrap();

        let got = unique_path(&want);
        assert_eq!(got.file_name().unwrap().to_string_lossy(), "f (3).bin");
    }

    #[test]
    fn файл_без_розширення_теж_нумерується() {
        let dir = TempDir::new("noext");
        let want = dir.0.join("README");
        std::fs::write(&want, b"x").unwrap();

        let got = unique_path(&want);
        assert_eq!(got.file_name().unwrap().to_string_lossy(), "README (1)");
    }

    #[test]
    fn складне_розширення_зберігається_повністю() {
        let dir = TempDir::new("tar");
        let want = dir.0.join("archive.tar.gz");
        std::fs::write(&want, b"x").unwrap();

        let got = unique_path(&want);
        assert_eq!(
            got.file_name().unwrap().to_string_lossy(),
            "archive.tar (1).gz",
            "номер має стати перед останнім розширенням"
        );
    }

    #[cfg(windows)]
    #[test]
    fn короткий_шлях_не_отримує_префікса() {
        let p = Path::new("C:\\dl\\file.bin");
        assert_eq!(long_path(p), p.to_path_buf());
    }

    #[cfg(windows)]
    #[test]
    fn довгий_шлях_отримує_префікс() {
        let long = format!("C:\\dl\\{}\\file.bin", "тека".repeat(80));
        let out = long_path(Path::new(&long));

        assert!(
            out.display().to_string().starts_with("\\\\?\\"),
            "без префікса система обрізала б шлях за MAX_PATH"
        );
    }

    #[cfg(windows)]
    #[test]
    fn мережевий_шлях_отримує_свою_форму_префікса() {
        let long = format!("\\\\server\\share\\{}\\file.bin", "тека".repeat(80));
        let out = long_path(Path::new(&long)).display().to_string();

        assert!(out.starts_with("\\\\?\\UNC\\"), "{out}");
        assert!(
            !out.starts_with("\\\\?\\\\\\"),
            "подвійний слеш лишився: {out}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn вже_префіксований_шлях_не_подвоюється() {
        let p = format!("\\\\?\\C:\\dl\\{}\\f.bin", "т".repeat(300));
        let out = long_path(Path::new(&p)).display().to_string();

        assert_eq!(out.matches("\\\\?\\").count(), 1, "{out}");
    }

    #[cfg(windows)]
    #[test]
    fn типова_тека_даних_на_windows_з_localappdata() {
        // Функція читає середовище процесу; перевіряємо форму шляху.
        let got = default_data_dir();
        assert!(
            got.ends_with("Downloader"),
            "очікували …\\Downloader, маємо {}",
            got.display()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn типова_тека_даних_на_linux_xdg_або_home() {
        let got = default_data_dir();
        let s = got.display().to_string();
        assert!(
            s.ends_with("/downloader") || s.ends_with("/.local/share/Downloader"),
            "очікували XDG downloader або ~/.local/share/Downloader, маємо {s}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn типова_тека_даних_на_macos_application_support() {
        let got = default_data_dir();
        assert!(
            got.ends_with("Library/Application Support/Downloader"),
            "очікували Application Support/Downloader, маємо {}",
            got.display()
        );
    }

    #[test]
    fn звичайний_режим_без_маркера_повертає_localappdata() {
        let dir = TempDir::new("regular");
        let fake_exe = dir.0.join("downloader-core.exe");
        std::fs::write(&fake_exe, b"").unwrap();

        assert!(!is_portable_dir(&dir.0));
        let data = data_dir_for_exe(Some(&fake_exe));
        assert_eq!(data, default_data_dir());
        let dl = downloads_dir_for_exe(Some(&fake_exe));
        assert_eq!(dl, default_downloads_dir());
    }

    #[test]
    fn портативний_режим_з_маркером_кладе_все_поруч_із_exe() {
        let dir = TempDir::new("portable");
        let fake_exe = dir.0.join("downloader-core.exe");
        let marker = dir.0.join(PORTABLE_MARKER);
        std::fs::write(&fake_exe, b"").unwrap();
        std::fs::write(&marker, b"").unwrap();

        assert!(is_portable_dir(&dir.0));
        let data = data_dir_for_exe(Some(&fake_exe));
        assert_eq!(data, dir.0, "у портативному режимі база має лягати в теку exe");
        let dl = downloads_dir_for_exe(Some(&fake_exe));
        assert_eq!(
            dl,
            dir.0.join("Downloads"),
            "завантаження мають бути у підтеці Downloads поруч"
        );
    }
}

