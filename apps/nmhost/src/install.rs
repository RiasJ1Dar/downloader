//! Поставити native-host маніфест у Chrome/Edge/Firefox.
//!
//! Без цього розширення каже «Specified native messaging host not found».
//! ID розширення зашитий (поле `key` у `ext/chrome/manifest.json`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::HOST_NAME;

/// Стабільний ID unpacked-розширення (з `key` у manifest.json).
pub const EXTENSION_ID: &str = "pionjhjgjaehkcpkidlblhonbejfdcdj";

/// Ідентифікатор розширення у Firefox.
///
/// Firefox не знає `chrome-extension://…` і звіряє відправника за полем
/// `allowed_extensions`. Теку для маніфеста ми прописували із самого початку,
/// а це поле — ні, тож у Firefox хост мовчки відмовляв би кожному
/// підключенню: тека є, дозволу немає.
pub const FIREFOX_EXTENSION_ID: &str = "downloader@riasj1dar.github.io";

/// JSON маніфесту native host; `додаткові` — ID розширень поза кодом
/// (порожній зріз означає лише типові).
///
/// Обидва списки лежать в одному файлі навмисно: Chrome читає
/// `allowed_origins` і не зважає на `allowed_extensions`, Firefox — навпаки.
/// Два окремі файли розійшлися б при першій же зміні шляху до програми.
///
/// Ідентифікатор виду `chrome-extension://…` вважається Chromium-адресою,
/// решта — розширенням Firefox. Розрізняти інакше нема за чим: Firefox
/// використовує довільний рядок, Chromium — 32 літери.
#[must_use]
pub fn host_manifest_json_with(exe: &Path, додаткові: &[String]) -> String {
    let exe = exe.display().to_string().replace('\\', "\\\\");

    let mut chromium = vec![format!("chrome-extension://{EXTENSION_ID}/")];
    let mut firefox = vec![FIREFOX_EXTENSION_ID.to_owned()];
    for id in додаткові {
        let id = id.trim();
        if id.is_empty() {
            continue;
        }
        if id.contains('@') || id.starts_with('{') {
            firefox.push(id.to_owned());
        } else if id.starts_with("chrome-extension://") {
            chromium.push(id.trim_end_matches('/').to_owned() + "/");
        } else {
            chromium.push(format!("chrome-extension://{id}/"));
        }
    }

    let список = |v: &[String]| {
        v.iter()
            .map(|s| format!("    \"{s}\""))
            .collect::<Vec<_>>()
            .join(",\n")
    };
    let chromium = список(&chromium);
    let firefox = список(&firefox);

    format!(
        r#"{{
  "name": "{HOST_NAME}",
  "description": "Downloader native messaging host",
  "path": "{exe}",
  "type": "stdio",
  "allowed_origins": [
{chromium}
  ],
  "allowed_extensions": [
{firefox}
  ]
}}
"#
    )
}

/// Прибрати префікс `\\?\`, який `canonicalize` додає на Windows.
///
/// У маніфесті лежить шлях, за яким **браузер** запускає програму. Rust
/// повертає розширену форму `\\?\D:\…`, і вона потрапляла просто в JSON:
/// шлях правильний, файл існує, а браузер із такого запису хост може не
/// запустити — і скаже лише «host not found», не пояснюючи, що шлях йому
/// не подобається.
#[must_use]
fn без_префікса_unc(шлях: &Path) -> PathBuf {
    let s = шлях.display().to_string();
    match s.strip_prefix(r"\\?\") {
        // `\\?\UNC\server\share` — мережевий шлях, його скорочують інакше.
        Some(решта) if решта.starts_with("UNC\\") => {
            PathBuf::from(format!(r"\\{}", &решта[4..]))
        }
        Some(решта) => PathBuf::from(решта),
        None => шлях.to_path_buf(),
    }
}

/// Куди класти JSON поруч із даними користувача або поруч із exe у портативному режимі.
pub fn manifest_path_for(exe: Option<&Path>) -> PathBuf {
    let base = downloader_winutil::data_dir_for_exe(exe);
    base.join("nm").join(format!("{HOST_NAME}.json"))
}

/// Куди класти JSON поруч із даними користувача.
#[allow(dead_code)]
pub fn default_manifest_path() -> Result<PathBuf> {
    Ok(manifest_path_for(std::env::current_exe().ok().as_deref()))
}

/// Записати JSON і прописати реєстр Windows / теки браузерів на Unix.
///
/// `додаткові` — ідентифікатори розширень із магазинів, яких немає в коді.
pub fn install_with(exe: &Path, додаткові: &[String]) -> Result<PathBuf> {
    let exe = exe
        .canonicalize()
        .with_context(|| format!("немає {}", exe.display()))?;
    let exe = без_префікса_unc(&exe);
    let path = manifest_path_for(Some(&exe));
    if let Some(dir) = path.parent() {
        #[cfg(unix)]
        ensure_private_dir(dir)?;
        #[cfg(not(unix))]
        fs::create_dir_all(dir)?;
    }
    fs::write(&path, host_manifest_json_with(&exe, додаткові))?;

    #[cfg(windows)]
    {
        прописати_chromium(&path)?;
        прописати_firefox(&path)?;
    }
    #[cfg(unix)]
    {
        прописати_unix(&path)?;
    }
    Ok(path)
}

/// Прибрати маніфест і записи браузерів (реєстр / NativeMessagingHosts).
///
/// Відсутні файли чи ключі не є помилкою: повторне `--uninstall` має бути
/// ідемпотентним, як і встановлення в усі відомі гілки одразу.
pub fn uninstall_with(exe: Option<&Path>) -> Result<PathBuf> {
    let path = manifest_path_for(exe);

    #[cfg(windows)]
    {
        зняти_chromium()?;
        зняти_firefox()?;
    }
    #[cfg(unix)]
    {
        зняти_unix()?;
    }

    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("не знято {}", path.display())),
    }
    Ok(path)
}

/// Гілки реєстру браузерів на Chromium.
///
/// Кожен форк читає **свою** гілку, а не спільну: Brave не бачить запису
/// Chrome, Vivaldi не бачить запису Brave. Тому список, а не одна адреса —
/// інакше розширення в Brave казало б «host not found» при цілком робочій
/// установці для Chrome, і причина була б невидима.
///
/// Записуємо в усі відомі, навіть якщо браузера немає: гілка в HKCU — це
/// порожній ключ на кілька десятків байтів, а поява браузера пізніше не
/// потребуватиме перевстановлення.
#[cfg(windows)]
const ГІЛКИ_CHROMIUM: &[&str] = &[
    r"Software\Google\Chrome",
    r"Software\Chromium",
    r"Software\Microsoft\Edge",
    r"Software\BraveSoftware\Brave-Browser",
    r"Software\Vivaldi",
    r"Software\Opera Software\Opera Stable",
];
// ⚠️ Яндекс.Браузера тут немає і не буде: з російським софтом проєкт не
// взаємодіє (Р-15). Це той самий рядок правил, що й відсутність локалі `ru`.

#[cfg(windows)]
fn прописати_chromium(manifest: &Path) -> Result<()> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    for гілка in ГІЛКИ_CHROMIUM {
        let key = format!(r"{гілка}\NativeMessagingHosts\{HOST_NAME}");
        let (k, _) = hkcu
            .create_subkey(&key)
            .with_context(|| format!("реєстр {key}"))?;
        k.set_value("", &manifest.display().to_string())
            .with_context(|| format!("запис {key}"))?;
    }
    Ok(())
}

#[cfg(windows)]
fn зняти_chromium() -> Result<()> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    for гілка in ГІЛКИ_CHROMIUM {
        let parent = format!(r"{гілка}\NativeMessagingHosts");
        // delete_subkey ігнорує відсутній ключ через перевірку; winreg
        // повертає Err, якщо немає — це норма для повторного uninstall.
        match hkcu.delete_subkey(format!(r"{parent}\{HOST_NAME}")) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("реєстр {parent}\\{HOST_NAME}"));
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn прописати_firefox(manifest: &Path) -> io::Result<()> {
    let Some(roaming) = std::env::var_os("APPDATA") else {
        return Ok(());
    };
    let dest = PathBuf::from(roaming)
        .join("Mozilla")
        .join("NativeMessagingHosts")
        .join(format!("{HOST_NAME}.json"));
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::copy(manifest, dest)?;
    Ok(())
}

#[cfg(windows)]
fn зняти_firefox() -> io::Result<()> {
    let Some(roaming) = std::env::var_os("APPDATA") else {
        return Ok(());
    };
    let dest = PathBuf::from(roaming)
        .join("Mozilla")
        .join("NativeMessagingHosts")
        .join(format!("{HOST_NAME}.json"));
    match fs::remove_file(dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Створити батьківську теку з режимом `0700`, якщо її ще немає.
///
/// Якщо тека вже є — не чіпаємо (зокрема не chmod-имо чужі `.config` /
/// `Application Support`). `recursive` + `mode` задають 0700 лише новим.
#[cfg(unix)]
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    if dir.exists() {
        return Ok(());
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(dir)?;
    Ok(())
}

#[cfg(unix)]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// `$XDG_CONFIG_HOME` або `~/.config` — база для Chromium-форків на Linux.
#[cfg(target_os = "linux")]
fn config_home(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
}

/// Теки `NativeMessagingHosts` / `native-messaging-hosts` для поточного ОС.
///
/// Пишемо в усі відомі одразу, навіть якщо браузера немає: порожня тека
/// коштує нічого, а поява браузера пізніше не потребуватиме перевстановлення.
/// Кожен форк читає **свою** теку — як і окремі гілки реєстру на Windows.
#[cfg(unix)]
#[must_use]
pub fn unix_native_messaging_dirs(home: &Path) -> Vec<PathBuf> {
    unix_native_messaging_dirs_os(home)
}

#[cfg(target_os = "linux")]
fn unix_native_messaging_dirs_os(home: &Path) -> Vec<PathBuf> {
    let config = config_home(home);
    vec![
        config.join("google-chrome/NativeMessagingHosts"),
        config.join("chromium/NativeMessagingHosts"),
        config.join("microsoft-edge/NativeMessagingHosts"),
        config.join("BraveSoftware/Brave-Browser/NativeMessagingHosts"),
        config.join("vivaldi/NativeMessagingHosts"),
        // Firefox на Linux — окрема конвенція (не XDG, і lowercase).
        home.join(".mozilla/native-messaging-hosts"),
    ]
}

#[cfg(target_os = "macos")]
fn unix_native_messaging_dirs_os(home: &Path) -> Vec<PathBuf> {
    let support = home.join("Library/Application Support");
    vec![
        support.join("Google/Chrome/NativeMessagingHosts"),
        support.join("Chromium/NativeMessagingHosts"),
        support.join("Microsoft Edge/NativeMessagingHosts"),
        support.join("BraveSoftware/Brave-Browser/NativeMessagingHosts"),
        support.join("Vivaldi/NativeMessagingHosts"),
        support.join("Mozilla/NativeMessagingHosts"),
    ]
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn unix_native_messaging_dirs_os(home: &Path) -> Vec<PathBuf> {
    // Інші Unix: ті самі шляхи, що й Linux (XDG + ~/.mozilla).
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    vec![
        config.join("google-chrome/NativeMessagingHosts"),
        config.join("chromium/NativeMessagingHosts"),
        config.join("microsoft-edge/NativeMessagingHosts"),
        config.join("BraveSoftware/Brave-Browser/NativeMessagingHosts"),
        config.join("vivaldi/NativeMessagingHosts"),
        home.join(".mozilla/native-messaging-hosts"),
    ]
}

#[cfg(unix)]
fn прописати_unix(manifest: &Path) -> Result<()> {
    let Some(home) = home_dir() else {
        return Ok(());
    };
    прописати_unix_у(manifest, &home)
}

/// Скопіювати маніфест у всі стандартні теки браузерів під `home`.
///
/// Винесено окремо, щоб тести могли підставити тимчасову «домівку» без
/// мутації `HOME` (у Rust 2024 `env::set_var` — unsafe).
#[cfg(unix)]
fn прописати_unix_у(manifest: &Path, home: &Path) -> Result<()> {
    let name = format!("{HOST_NAME}.json");
    for dir in unix_native_messaging_dirs(home) {
        ensure_private_dir(&dir)
            .with_context(|| format!("тека {}", dir.display()))?;
        let dest = dir.join(&name);
        fs::copy(manifest, &dest)
            .with_context(|| format!("копія → {}", dest.display()))?;
    }
    Ok(())
}

#[cfg(unix)]
fn зняти_unix() -> Result<()> {
    let Some(home) = home_dir() else {
        return Ok(());
    };
    зняти_unix_у(&home)
}

#[cfg(unix)]
fn зняти_unix_у(home: &Path) -> Result<()> {
    let name = format!("{HOST_NAME}.json");
    for dir in unix_native_messaging_dirs(home) {
        let dest = dir.join(&name);
        match fs::remove_file(&dest) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("не знято {}", dest.display()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "у тестах падіння і є повідомленням про помилку"
)]
mod tests {
    use super::*;

    #[test]
    fn json_містить_id_і_stdio() {
        let j = host_manifest_json_with(Path::new(r"C:\dl\downloader-nmhost.exe"), &[]);
        assert!(j.contains(EXTENSION_ID));
        assert!(j.contains("stdio"));
        assert!(j.contains("downloader-nmhost.exe"));
        assert!(j.contains(HOST_NAME));
    }

    /// ID з магазину додається без перезбірки програми.
    ///
    /// Chrome Web Store призначає ідентифікатор сам, і наперед він невідомий.
    /// Якби єдиним джерелом лишалась константа в коді, кожна публікація
    /// вимагала б нового випуску програми — а до того розширення з магазину
    /// мовчки не працювало б.
    #[test]
    fn додаткові_id_потрапляють_у_потрібний_список() {
        let j = host_manifest_json_with(
            Path::new(r"C:\dl\downloader-nmhost.exe"),
            &[
                "abcdefghijklmnopabcdefghijklmnop".to_owned(),
                "інше@розширення".to_owned(),
            ],
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&j).expect("маніфест має бути валідним JSON");

        let chromium = parsed["allowed_origins"].as_array().expect("масив");
        assert!(
            chromium
                .iter()
                .any(|v| v == "chrome-extension://abcdefghijklmnopabcdefghijklmnop/"),
            "ID магазину не потрапив до Chromium: {j}"
        );
        assert!(
            chromium.iter().any(|v| v
                == &format!("chrome-extension://{EXTENSION_ID}/")),
            "типовий ID зник: {j}"
        );

        let firefox = parsed["allowed_extensions"].as_array().expect("масив");
        assert!(
            firefox.iter().any(|v| v == "інше@розширення"),
            "ID з @ мав піти до Firefox: {j}"
        );
    }

    /// Шлях у маніфесті має бути звичайним, без `\\?\`.
    #[test]
    fn префікс_unc_не_потрапляє_в_маніфест() {
        assert_eq!(
            без_префікса_unc(Path::new(r"\\?\D:\dl\downloader-nmhost.exe")),
            PathBuf::from(r"D:\dl\downloader-nmhost.exe")
        );
        assert_eq!(
            без_префікса_unc(Path::new(r"\\?\UNC\server\share\dl.exe")),
            PathBuf::from(r"\\server\share\dl.exe")
        );
        // Звичайний шлях лишається як є.
        assert_eq!(
            без_префікса_unc(Path::new(r"D:\dl\dl.exe")),
            PathBuf::from(r"D:\dl\dl.exe")
        );
    }

    /// Запобіжник Р-15 у місці, де його легко порушити не думаючи.
    ///
    /// Перелік гілок реєстру поповнюється механічно — «ще один браузер на
    /// Chromium». Саме так російський софт і потрапляє в проєкт: не рішенням,
    /// а доповненням списку.
    #[cfg(windows)]
    #[test]
    fn серед_браузерів_немає_російських() {
        for гілка in ГІЛКИ_CHROMIUM {
            let нижній = гілка.to_lowercase();
            assert!(
                !нижній.contains("yandex")
                    && !нижній.contains("mail.ru")
                    && !нижній.contains("atom"),
                "російський браузер у переліку: {гілка}"
            );
        }
    }

    /// Той самий запобіжник Р-15 для Unix-шляхів.
    #[cfg(unix)]
    #[test]
    fn серед_браузерів_unix_немає_російських() {
        let home = Path::new("/home/test");
        for dir in unix_native_messaging_dirs(home) {
            let нижній = dir.display().to_string().to_lowercase();
            assert!(
                !нижній.contains("yandex")
                    && !нижній.contains("mail.ru")
                    && !нижній.contains("atom"),
                "російський браузер у переліку: {}",
                dir.display()
            );
        }
    }

    /// Один маніфест обслуговує обидва сімейства браузерів.
    ///
    /// Без `allowed_extensions` Firefox відхиляє підключення, і симптом
    /// оманливий: тека маніфеста на місці, файл читається, а розширення
    /// бачить лише «host not found» — так ніби програму не встановлено.
    #[test]
    fn маніфест_дозволяє_і_chrome_і_firefox() {
        let j = host_manifest_json_with(Path::new(r"C:\dl\downloader-nmhost.exe"), &[]);
        // Розбираємо JSON, а не шукаємо підрядок: `contains("allowed_extensions")`
        // знаходить сам себе всередині будь-якого схожого імені поля, і тест
        // лишається зеленим на зламаному маніфесті. Перевірено: перейменування
        // поля таку перевірку не валить, а цю — валить.
        let parsed: serde_json::Value =
            serde_json::from_str(&j).expect("маніфест має бути валідним JSON");
        assert_eq!(parsed["type"], "stdio");

        let chrome = parsed["allowed_origins"]
            .as_array()
            .expect("allowed_origins має бути масивом");
        assert!(
            chrome
                .iter()
                .any(|v| v == &format!("chrome-extension://{EXTENSION_ID}/")),
            "немає дозволу для Chrome: {j}"
        );

        let firefox = parsed["allowed_extensions"]
            .as_array()
            .expect("allowed_extensions має бути масивом");
        assert!(
            firefox.iter().any(|v| v == FIREFOX_EXTENSION_ID),
            "немає дозволу для Firefox: {j}"
        );
    }

    #[test]
    fn шлях_маніфесту_враховує_портативний_режим() -> Result<()> {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let temp = std::env::temp_dir().join(format!("nm-port-{unique}"));
        std::fs::create_dir_all(&temp)?;
        let fake_exe = temp.join("downloader-nmhost.exe");
        std::fs::write(&fake_exe, b"")?;

        // Без маркера
        let normal_path = manifest_path_for(Some(&fake_exe));
        assert!(
            normal_path.ends_with(format!("Downloader\\nm\\{HOST_NAME}.json"))
                || normal_path.ends_with(format!("Downloader/nm/{HOST_NAME}.json"))
                || normal_path.ends_with(format!("downloader/nm/{HOST_NAME}.json"))
                || normal_path
                    .ends_with(format!("Application Support/Downloader/nm/{HOST_NAME}.json")),
            "несподіваний шлях маніфесту: {}",
            normal_path.display()
        );

        // З маркером
        std::fs::write(temp.join("portable.txt"), b"")?;
        let port_path = manifest_path_for(Some(&fake_exe));
        assert_eq!(
            port_path,
            temp.join("nm").join(format!("{HOST_NAME}.json")),
            "у портативному режимі маніфест кладеться поруч"
        );

        assert!(default_manifest_path().is_ok());

        std::fs::remove_dir_all(&temp)?;
        Ok(())
    }

    /// `прописати_unix_у` створює теки і кладе копію маніфесту в кожну.
    #[cfg(unix)]
    #[test]
    fn unix_прописує_всі_стандартні_теки() -> Result<()> {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("nm-unix-{unique}"));
        let home = root.join("home");
        fs::create_dir_all(&home)?;

        let canonical = root.join("canonical.json");
        fs::write(&canonical, b"{\"name\":\"com.downloader.host\"}\n")?;

        прописати_unix_у(&canonical, &home)?;

        let dirs = unix_native_messaging_dirs(&home);
        assert!(
            dirs.len() >= 5,
            "очікували щонайменше Chrome/Chromium/Edge/Brave/Firefox"
        );
        let name = format!("{HOST_NAME}.json");
        for dir in &dirs {
            let dest = dir.join(&name);
            assert!(
                dest.is_file(),
                "немає маніфесту в {}",
                dest.display()
            );
            let body = fs::read_to_string(&dest)?;
            assert!(body.contains(HOST_NAME), "вміст: {body}");
        }

        // Firefox: на Linux — lowercase `native-messaging-hosts`.
        #[cfg(target_os = "linux")]
        {
            assert!(
                dirs.iter()
                    .any(|d| d.ends_with(".mozilla/native-messaging-hosts")),
                "немає Firefox-теки: {dirs:?}"
            );
            assert!(
                dirs.iter()
                    .any(|d| d.ends_with("google-chrome/NativeMessagingHosts")),
                "немає Chrome-теки: {dirs:?}"
            );
        }

        зняти_unix_у(&home)?;
        for dir in &dirs {
            let dest = dir.join(&name);
            assert!(
                !dest.exists(),
                "uninstall лишив {}",
                dest.display()
            );
        }
        // Повторне зняття — ідемпотентне.
        зняти_unix_у(&home)?;

        fs::remove_dir_all(&root)?;
        Ok(())
    }
}
