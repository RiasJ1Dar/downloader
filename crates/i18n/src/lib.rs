//! Каталоги **uk** (типовий) і **en**. Російської немає.
//!
//! `DOWNLOADER_LANG=en|uk` перебиває ОС. Локаль ОС `ru*` → українська,
//! не англійська і не російська (Р-15).

use std::sync::OnceLock;

use downloader_core::error::Error;
use fluent_bundle::{FluentArgs, FluentBundle, FluentResource, FluentValue};
use unic_langid::LanguageIdentifier;

const UK_FTL: &str = include_str!("../l10n/uk.ftl");
const EN_FTL: &str = include_str!("../l10n/en.ftl");

/// Мова видимого тексту.
#[derive(Debug, Clone)]
pub enum Lang {
    Uk,
    En,
    Custom {
        code: String,
        source: Option<String>,
    },
}

impl PartialEq for Lang {
    fn eq(&self, other: &Self) -> bool {
        self.code() == other.code()
    }
}

impl Eq for Lang {}

impl Lang {
    fn id(&self) -> LanguageIdentifier {
        match self {
            Lang::Uk => unic_langid::langid!("uk"),
            Lang::En => unic_langid::langid!("en"),
            Lang::Custom { code, .. } => {
                code.parse().unwrap_or_else(|_| unic_langid::langid!("und"))
            }
        }
    }

    /// Створити сторонню мову за кодом (джерело шукається у стандартних шляхах).
    #[must_use]
    pub fn custom(code: impl Into<String>) -> Self {
        let code = code.into();
        let source = знайти_джерело_локалі(&code);
        Self::Custom { code, source }
    }

    /// Дволітерний або повноцінний BCP-47 код мови.
    #[must_use]
    pub fn code(&self) -> &str {
        match self {
            Lang::Uk => "uk",
            Lang::En => "en",
            Lang::Custom { code, .. } => code.as_str(),
        }
    }
}

static LANG: OnceLock<Lang> = OnceLock::new();

/// Каталоги, де шукаються додаткові локалі (.ftl файли).
#[must_use]
pub fn пошукові_шляхи_l10n() -> Vec<std::path::PathBuf> {
    пошукові_шляхи_з_додатковим(None)
}

/// Каталоги для пошуку з опційним додатковим каталогом (зручно для тестів без змін оточення).
#[must_use]
pub fn пошукові_шляхи_з_додатковим(додатковий: Option<&std::path::Path>) -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(p) = додатковий {
        dirs.push(p.to_path_buf());
    }
    if let Ok(p) = std::env::var("DOWNLOADER_L10N_DIR") {
        dirs.push(std::path::PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        dirs.push(parent.join("l10n"));
        dirs.push(parent.to_path_buf());
    }
    dirs.push(std::path::PathBuf::from("l10n"));
    dirs.push(std::path::PathBuf::from("crates/i18n/l10n"));
    dirs.push(std::path::PathBuf::from("../../crates/i18n/l10n"));
    dirs
}

/// Шукає вміст .ftl файлу для вказаної мови у переданих шляхах.
/// Російська мова суворо блокується за правилом Р-15 (завжди повертає None).
#[must_use]
pub fn знайти_джерело_локалі_в_шляхах(code: &str, paths: &[std::path::PathBuf]) -> Option<String> {
    let code_norm = code.trim().to_ascii_lowercase();
    if code_norm == "ru" || code_norm.starts_with("ru-") || code_norm.starts_with("ru_") {
        return None;
    }
    for dir in paths {
        let path = dir.join(format!("{code_norm}.ftl"));
        if path.is_file()
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            return Some(content);
        }
    }
    None
}

/// Шукає вміст .ftl файлу для вказаної мови у стандартних шляхах.
#[must_use]
pub fn знайти_джерело_локалі(code: &str) -> Option<String> {
    знайти_джерело_локалі_в_шляхах(code, &пошукові_шляхи_l10n())
}

/// Повертає список доступних локалей (вбудовані + знайдені на диску).
/// ru ніколи не включається до списку за правилом Р-15.
#[must_use]
pub fn доступні_локалі_в_шляхах(paths: &[std::path::PathBuf]) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    set.insert("uk".to_string());
    set.insert("en".to_string());

    for dir in paths {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("ftl")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                let s = stem.to_ascii_lowercase();
                if s != "ru" && !s.starts_with("ru-") && !s.starts_with("ru_") {
                    set.insert(s);
                }
            }
        }
    }
    set.into_iter().collect()
}

/// Повертає список доступних локалей у стандартних шляхах.
#[must_use]
pub fn доступні_локалі() -> Vec<String> {
    доступні_локалі_в_шляхах(&пошукові_шляхи_l10n())
}

/// Обрати мову з явним переліком пошукових шляхів.
#[must_use]
pub fn обрати_мову_в_шляхах(
    заявлене: Option<&str>,
    os: Option<&str>,
    paths: &[std::path::PathBuf],
) -> Lang {
    if let Some(s) = заявлене {
        let code = s.trim().to_ascii_lowercase();
        // ОС чи аргумент російською — українська. Немає гілки, яка б увімкнула ru (Р-15).
        if code == "ru" || code.starts_with("ru-") || code.starts_with("ru_") {
            return Lang::Uk;
        }
        match code.as_str() {
            "en" | "en-us" | "en-gb" => return Lang::En,
            "uk" | "uk-ua" => return Lang::Uk,
            _ => {
                // Перевіряємо, чи є відповідний .ftl на диску без змін коду (Ф12)
                if let Some(src) = знайти_джерело_локалі_в_шляхах(&code, paths) {
                    return Lang::Custom {
                        code,
                        source: Some(src),
                    };
                }
                if let Some((base, _)) = code.split_once(['-', '_'])
                    && let Some(src) = знайти_джерело_локалі_в_шляхах(base, paths)
                {
                    return Lang::Custom {
                        code: base.to_string(),
                        source: Some(src),
                    };
                }
            }
        }
    }
    if let Some(os) = os {
        let l = os.to_ascii_lowercase();
        // ОС російською — українська. Немає гілки, яка б увімкнула ru.
        if l.starts_with("ru") {
            return Lang::Uk;
        }
        if l.starts_with("en") {
            return Lang::En;
        }
        if l.starts_with("uk") {
            return Lang::Uk;
        }
        if let Some((base, _)) = l.split_once(['-', '_'])
            && let Some(src) = знайти_джерело_локалі_в_шляхах(base, paths)
        {
            return Lang::Custom {
                code: base.to_string(),
                source: Some(src),
            };
        }
        if let Some(src) = знайти_джерело_локалі_в_шляхах(&l, paths) {
            return Lang::Custom {
                code: l,
                source: Some(src),
            };
        }
    }
    Lang::Uk
}

/// Обрати мову: прапорець/змінна, потім ОС, російська ОС → uk, інакше uk.
#[must_use]
pub fn обрати_мову(заявлене: Option<&str>, os: Option<&str>) -> Lang {
    обрати_мову_в_шляхах(заявлене, os, &пошукові_шляхи_l10n())
}

/// Зафіксувати мову процесу. Повторний виклик ігнорується.
pub fn init(заявлене: Option<&str>) {
    let env = std::env::var("DOWNLOADER_LANG").ok();
    let os = sys_locale::get_locale();
    let lang = обрати_мову(заявлене.or(env.as_deref()), os.as_deref());
    if LANG.set(lang).is_err() {
        // Мову вже зафіксовано в цьому процесі.
    }
}

fn поточна() -> Lang {
    LANG.get().cloned().unwrap_or(Lang::Uk)
}

fn bundle(lang: &Lang) -> Option<FluentBundle<FluentResource>> {
    let source = match lang {
        Lang::Uk => UK_FTL.to_string(),
        Lang::En => EN_FTL.to_string(),
        Lang::Custom { code, source } => {
            if let Some(s) = source {
                s.clone()
            } else {
                знайти_джерело_локалі(code)?
            }
        }
    };
    let res = FluentResource::try_new(source).ok()?;
    let mut b = FluentBundle::new(vec![lang.id()]);
    b.set_use_isolating(false);
    b.add_resource(res).ok()?;
    Some(b)
}

/// Отримати переклад для явно вказаної мови з підтримкою запасного варіанту uk.
#[must_use]
pub fn t_for_lang(lang: &Lang, id: &str) -> String {
    t_args_for_lang(lang, id, &FluentArgs::new())
}

/// Отримати переклад з аргументами для явно вказаної мови.
#[must_use]
pub fn t_args_for_lang(lang: &Lang, id: &str, args: &FluentArgs) -> String {
    if let Some(s) = спробувати(lang, id, args) {
        return s;
    }
    if *lang != Lang::Uk
        && let Some(s) = спробувати(&Lang::Uk, id, args)
    {
        return s;
    }
    id.to_owned()
}

/// Ключ без аргументів. Немає ключа — рядок ключа (щоб було видно дірку).
#[must_use]
pub fn t(id: &str) -> String {
    t_args(id, &FluentArgs::new())
}

/// Зручно з CLI без прямої залежності на fluent-bundle.
#[must_use]
pub fn t_pairs(id: &str, pairs: &[(&str, String)]) -> String {
    let mut args = FluentArgs::new();
    for (k, v) in pairs {
        args.set(*k, v.clone());
    }
    t_args(id, &args)
}

/// Ключ з аргументами Fluent.
#[must_use]
pub fn t_args(id: &str, args: &FluentArgs) -> String {
    let lang = поточна();
    t_args_for_lang(&lang, id, args)
}

fn спробувати(lang: &Lang, id: &str, args: &FluentArgs) -> Option<String> {
    let bundle = bundle(lang)?;
    let msg = bundle.get_message(id)?;
    let pat = msg.value()?;
    let mut errors = Vec::new();
    let s = bundle.format_pattern(pat, Some(args), &mut errors);
    Some(s.into_owned())
}

fn arg_str<'a>(k: &'a str, v: impl ToString) -> FluentArgs<'a> {
    let mut a = FluentArgs::new();
    a.set(k, FluentValue::from(v.to_string()));
    a
}

/// Текст помилки ядра мовою інтерфейсу. Журнал ядра лишається українським.
#[must_use]
pub fn помилка_ядра(e: &Error) -> String {
    match e {
        Error::RangeUnsupported { url } => t_args("err-range", &arg_str("url", url)),
        Error::ResourceChanged { url } => t_args("err-changed", &arg_str("url", url)),
        Error::VerificationFailed {
            path,
            expected,
            actual,
        } => {
            let mut a = FluentArgs::new();
            a.set("path", path.display().to_string());
            a.set("expected", expected.clone());
            a.set("actual", actual.clone());
            t_args("err-verify", &a)
        }
        Error::SegmentOutOfBounds { id, count } => {
            let mut a = FluentArgs::new();
            a.set("id", i64::try_from(*id).unwrap_or(i64::MAX));
            a.set("count", i64::try_from(*count).unwrap_or(i64::MAX));
            t_args("err-seg-oob", &a)
        }
        Error::SegmentOverrun {
            id,
            start,
            end,
            attempted,
        } => {
            let mut a = FluentArgs::new();
            a.set("id", i64::try_from(*id).unwrap_or(i64::MAX));
            a.set("start", i64::try_from(*start).unwrap_or(i64::MAX));
            a.set("end", i64::try_from(*end).unwrap_or(i64::MAX));
            a.set("attempted", i64::try_from(*attempted).unwrap_or(i64::MAX));
            t_args("err-overrun", &a)
        }
        Error::SegmentCoverageBroken { reason } => t_args("err-coverage", &arg_str("reason", reason)),
        Error::Store(detail) => t_args("err-store", &arg_str("detail", detail)),
        Error::AuthRequired { url, status } => {
            let mut a = FluentArgs::new();
            a.set("url", url.clone());
            a.set("status", i64::from(*status));
            t_args("err-auth", &a)
        }
        Error::Io(e) => t_args("err-io", &arg_str("detail", e)),
    }
}

/// Рекурсивно шукає заборонені російські каталоги та файли локалізації (Р-15, Ф12).
#[must_use]
pub fn шукати_заборонені_ru_файли(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut extra = Vec::new();
    fn walk(dir: &std::path::Path, extra: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let name = e.file_name();
            if name == ".git" || name == "target" {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                walk(&p, extra);
                continue;
            }
            let n = name.to_string_lossy();
            if n == "ru.ftl" || n == "ru.json" || (n.starts_with("ru.") && n.ends_with(".ftl")) {
                extra.push(p);
            }
        }
    }
    walk(dir, &mut extra);
    extra
}

#[cfg(test)]
#[expect(clippy::expect_used, clippy::unwrap_used, reason = "тест")]
mod tests {
    use super::*;

    #[test]
    fn типова_українська() {
        assert_eq!(обрати_мову(None, None), Lang::Uk);
        assert_eq!(обрати_мову(None, Some("de-DE")), Lang::Uk);
    }

    #[test]
    fn прапорець_en() {
        assert_eq!(обрати_мову(Some("en"), Some("uk-UA")), Lang::En);
    }

    #[test]
    fn ос_російська_це_українська() {
        assert_eq!(обрати_мову(None, Some("ru-RU")), Lang::Uk);
        assert_eq!(обрати_мову(None, Some("ru")), Lang::Uk);
        assert_ne!(обрати_мову(None, Some("ru-RU")), Lang::En);
    }

    #[test]
    fn ос_англійська() {
        assert_eq!(обрати_мову(None, Some("en-US")), Lang::En);
    }

    /// Ключі обох каталогів, у порядку появи.
    fn ключі(джерело: &str) -> Vec<&str> {
        джерело
            .lines()
            .map(str::trim_start)
            .filter(|ln| !ln.is_empty() && !ln.starts_with('#'))
            // Продовження багаторядкового значення власного ключа не має.
            .filter_map(|ln| ln.split_once('=').map(|(k, _)| k.trim()))
            .filter(|k| !k.contains(' '))
            .collect()
    }

    /// ⚠️ Каталоги мусять збігатися ключ у ключ.
    ///
    /// Без цієї перевірки розходження не видно ніяк: англійський інтерфейс
    /// просто показує український рядок із запасного каталогу — і виглядає
    /// це як «переклад ще не дійшов», а не як помилка. Дізнаються про неї
    /// від користувача, не від збірки.
    #[test]
    fn каталоги_збігаються_ключами() {
        let uk: std::collections::BTreeSet<_> = ключі(UK_FTL).into_iter().collect();
        let en: std::collections::BTreeSet<_> = ключі(EN_FTL).into_iter().collect();

        let лише_uk: Vec<_> = uk.difference(&en).copied().collect();
        let лише_en: Vec<_> = en.difference(&uk).copied().collect();

        assert!(
            лише_uk.is_empty(),
            "є лише в uk.ftl, англійською покажеться український текст: {лише_uk:?}"
        );
        assert!(
            лише_en.is_empty(),
            "є лише в en.ftl — українською такого рядка немає взагалі: {лише_en:?}"
        );
        assert!(uk.len() > 50, "каталог підозріло малий: {} ключів", uk.len());
    }

    /// Двічі оголошений ключ — тихо перемагає останній.
    #[test]
    fn у_каталозі_немає_повторів() {
        for (мова, джерело) in [("uk", UK_FTL), ("en", EN_FTL)] {
            let усі = ключі(джерело);
            let унікальні: std::collections::BTreeSet<_> = усі.iter().copied().collect();
            assert_eq!(
                усі.len(),
                унікальні.len(),
                "{мова}.ftl: ключ оголошено двічі, діє лише останній"
            );
        }
    }

    #[test]
    fn немає_файла_ru() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let root = root.canonicalize().unwrap();
        let extra = шукати_заборонені_ru_файли(&root);
        assert!(
            extra.is_empty(),
            "у репозиторії заборонені російські каталоги: {extra:?}"
        );
    }

    #[test]
    fn запобіжник_проти_ru_фіксує_спробу_додати_російську_локаль() {
        let temp_dir = std::env::temp_dir().join(format!("test_ru_guard_{}", std::process::id()));
        drop(std::fs::create_dir_all(&temp_dir));

        let fake_ru = temp_dir.join("ru.ftl");
        std::fs::write(&fake_ru, b"test = 123\n").expect("запис fake ru");

        let found = шукати_заборонені_ru_файли(&temp_dir);
        drop(std::fs::remove_file(&fake_ru));
        drop(std::fs::remove_dir(&temp_dir));

        assert!(
            !found.is_empty(),
            "запобіжник зобов'язаний виявити ru.ftl при спробі його підкинути!"
        );
    }

    #[test]
    fn спроба_вказати_російську_завжди_повертає_українську() {
        assert_eq!(обрати_мову(Some("ru"), None), Lang::Uk);
        assert_eq!(обрати_мову(Some("ru-RU"), None), Lang::Uk);
        assert_eq!(обрати_мову(Some("RU"), Some("ru")), Lang::Uk);
        assert_eq!(обрати_мову(None, Some("ru_RU.UTF-8")), Lang::Uk);

        // Навіть якщо підсунути ru.ftl у пошукові шляхи:
        let temp_dir = std::env::temp_dir().join(format!("test_ru_block_{}", std::process::id()));
        drop(std::fs::create_dir_all(&temp_dir));
        let fake_ru = temp_dir.join("ru.ftl");
        std::fs::write(&fake_ru, b"cli-about = test\n").expect("fake ru");

        let paths = vec![temp_dir.clone()];
        assert_eq!(обрати_мову_в_шляхах(Some("ru"), None, &paths), Lang::Uk);
        assert_eq!(знайти_джерело_локалі_в_шляхах("ru", &paths), None);
        assert!(!доступні_локалі_в_шляхах(&paths).contains(&"ru".to_string()));

        drop(std::fs::remove_file(&fake_ru));
        drop(std::fs::remove_dir(&temp_dir));
    }

    #[test]
    fn динамічна_локаль_xx_підхоплюється_селектором_без_правок_коду() {
        let temp_dir = std::env::temp_dir().join(format!("test_xx_dyn_{}", std::process::id()));
        drop(std::fs::create_dir_all(&temp_dir));
        let fake_xx = temp_dir.join("xx.ftl");
        std::fs::write(
            &fake_xx,
            "custom-text = Вітання мовою XX\nneed-url = Рядок XX: вкажіть посилання\n",
        )
        .expect("fake xx");

        let paths = vec![temp_dir.clone()];

        // Селектор виявляє нову мову:
        let lang = обрати_мову_в_шляхах(Some("xx"), None, &paths);
        assert_eq!(lang, Lang::custom("xx"));
        assert_eq!(lang.code(), "xx");

        // Переклад працює для нового ключа:
        let text = t_for_lang(&lang, "custom-text");
        assert_eq!(text, "Вітання мовою XX");

        // Для ключів, яких немає в xx.ftl, працює fallback на uk:
        let fallback = t_for_lang(&lang, "cli-about");
        assert!(fallback.contains("сегментоване"), "має спрацювати fallback: {fallback}");

        drop(std::fs::remove_file(&fake_xx));
        drop(std::fs::remove_dir(&temp_dir));
    }

    #[test]
    fn uk_і_en_мають_cli_about() {
        let uk = спробувати(&Lang::Uk, "cli-about", &FluentArgs::new()).expect("uk");
        let en = спробувати(&Lang::En, "cli-about", &FluentArgs::new()).expect("en");
        assert!(uk.contains("завантажень"), "{uk}");
        assert!(en.to_ascii_lowercase().contains("download"), "{en}");
    }
}
