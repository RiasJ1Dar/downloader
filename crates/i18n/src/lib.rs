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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Uk,
    En,
}

impl Lang {
    fn id(self) -> LanguageIdentifier {
        match self {
            Lang::Uk => unic_langid::langid!("uk"),
            Lang::En => unic_langid::langid!("en"),
        }
    }

    fn source(self) -> &'static str {
        match self {
            Lang::Uk => UK_FTL,
            Lang::En => EN_FTL,
        }
    }
}

static LANG: OnceLock<Lang> = OnceLock::new();

/// Обрати мову: прапорець/змінна, потім ОС, російська ОС → uk, інакше uk.
#[must_use]
pub fn обрати_мову(заявлене: Option<&str>, os: Option<&str>) -> Lang {
    if let Some(s) = заявлене {
        match s.trim().to_ascii_lowercase().as_str() {
            "en" | "en-us" | "en-gb" => return Lang::En,
            "uk" | "uk-ua" => return Lang::Uk,
            _ => {}
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
    }
    Lang::Uk
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
    *LANG.get().unwrap_or(&Lang::Uk)
}

fn bundle(lang: Lang) -> Option<FluentBundle<FluentResource>> {
    let res = FluentResource::try_new(lang.source().to_owned()).ok()?;
    let mut b = FluentBundle::new(vec![lang.id()]);
    b.set_use_isolating(false);
    b.add_resource(res).ok()?;
    Some(b)
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
    if let Some(s) = спробувати(lang, id, args) {
        return s;
    }
    if lang != Lang::Uk
        && let Some(s) = спробувати(Lang::Uk, id, args)
    {
        return s;
    }
    id.to_owned()
}

fn спробувати(lang: Lang, id: &str, args: &FluentArgs) -> Option<String> {
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
                if n == "ru.ftl" || n == "ru.json" || n.starts_with("ru.") && n.ends_with(".ftl")
                {
                    extra.push(p);
                }
            }
        }
        walk(&root, &mut extra);
        assert!(
            extra.is_empty(),
            "у репозиторії заборонені російські каталоги: {extra:?}"
        );
    }

    #[test]
    fn uk_і_en_мають_cli_about() {
        let uk = спробувати(Lang::Uk, "cli-about", &FluentArgs::new()).expect("uk");
        let en = спробувати(Lang::En, "cli-about", &FluentArgs::new()).expect("en");
        assert!(uk.contains("завантажень"), "{uk}");
        assert!(en.to_ascii_lowercase().contains("download"), "{en}");
    }
}
