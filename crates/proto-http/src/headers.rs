//! Розбір HTTP-заголовків, від яких залежить цілісність файла.
//!
//! Тут немає мережі — лише текст. Тому все, що нижче, доводиться тестами
//! до кінця: саме дрібниці цього рівня («слабкий `ETag`», «ім'я в
//! `filename*`», «`Content-Range` без загального розміру») дають потім
//! тихо биті файли.

/// Що сервер каже про підтримку часткових запитів.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSupport {
    /// `Accept-Ranges: bytes` — можна різати на сегменти.
    Bytes,
    /// `Accept-Ranges: none` — явна відмова.
    None,
    /// Заголовка немає. Не «ні», а «невідомо»: багато серверів мовчать, але
    /// `Range` тримають. Перевіряється пробним запитом, а не здогадкою.
    Unknown,
}

impl RangeSupport {
    /// Розібрати значення `Accept-Ranges`.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            None => Self::Unknown,
            Some(v) if v.eq_ignore_ascii_case("bytes") => Self::Bytes,
            Some(v) if v.eq_ignore_ascii_case("none") => Self::None,
            Some(_) => Self::Unknown,
        }
    }
}

/// Розібраний `Content-Range: bytes 0-99/12345`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentRange {
    /// Перший байт діапазону.
    pub start: u64,
    /// Останній байт діапазону, **включно** — так у HTTP.
    pub end: u64,
    /// Повний розмір ресурсу. `None`, якщо сервер написав `*`.
    pub total: Option<u64>,
}

impl ContentRange {
    /// Розібрати значення `Content-Range`.
    ///
    /// Повертає `None` на будь-чому незрозумілому: краще вважати відповідь
    /// непридатною, ніж гадати про межі й записати байти не туди.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let rest = value.trim().strip_prefix("bytes")?.trim_start();

        let (range, total) = rest.split_once('/')?;
        let (start, end) = range.trim().split_once('-')?;

        let start = start.trim().parse().ok()?;
        let end = end.trim().parse().ok()?;
        if end < start {
            return None;
        }

        let total = match total.trim() {
            "*" => None,
            n => Some(n.parse().ok()?),
        };

        // Сервер, який каже «діапазон 0-99 із 50 байтів», бреше — не віримо.
        if let Some(t) = total
            && end >= t
        {
            return None;
        }

        Some(Self { start, end, total })
    }

    /// Скільки байтів у діапазоні.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.end - self.start + 1
    }

    /// Чи діапазон порожній. Завжди `false`: у HTTP межі включні.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }
}

/// Ознака, за якою ми впізнаємо «це той самий файл» при докачуванні.
///
/// Іде в `If-Range`. Якщо ресурс змінився, сервер відповість `200` з повним
/// тілом замість `206` — і це єдиний надійний сигнал, що докачувати не можна.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Validator {
    /// Сильний `ETag`. Найкраще, що може бути.
    Strong(String),
    /// `Last-Modified`. Гірше за сильний `ETag`, але придатне.
    LastModified(String),
    /// Нічого придатного немає — докачування без гарантій.
    None,
}

impl Validator {
    /// Обрати ознаку з наявних заголовків.
    ///
    /// ⚠️ **Слабкий `ETag` (`W/"..."`) у `If-Range` використовувати не можна.**
    /// Він означає лише «семантично те саме» — вміст міг змінитись побайтово.
    /// Сервер віддасть `206`, ми доклеїмо шматок від іншої версії й отримаємо
    /// файл, який ніде не зламається явно. Тому слабкий `ETag` відкидаємо й
    /// падаємо на `Last-Modified`.
    #[must_use]
    pub fn choose(etag: Option<&str>, last_modified: Option<&str>) -> Self {
        if let Some(tag) = etag.map(str::trim)
            && !tag.is_empty()
            && !is_weak_etag(tag)
        {
            return Self::Strong(tag.to_owned());
        }

        if let Some(lm) = last_modified.map(str::trim)
            && !lm.is_empty()
        {
            return Self::LastModified(lm.to_owned());
        }

        Self::None
    }

    /// Значення для заголовка `If-Range`, якщо є що надіслати.
    #[must_use]
    pub fn if_range_value(&self) -> Option<&str> {
        match self {
            Self::Strong(v) | Self::LastModified(v) => Some(v),
            Self::None => None,
        }
    }

    /// Чи можна довіряти докачуванню з цією ознакою.
    #[must_use]
    pub const fn is_reliable(&self) -> bool {
        matches!(self, Self::Strong(_) | Self::LastModified(_))
    }
}

/// Чи це слабкий `ETag` (`W/"..."`).
#[must_use]
fn is_weak_etag(tag: &str) -> bool {
    let t = tag.trim_start();
    t.starts_with("W/") || t.starts_with("w/")
}

/// Витягти ім'я файла з `Content-Disposition`.
///
/// Пріоритет за RFC 6266: `filename*` (з кодуванням) важливіший за `filename`.
/// Повертається **лише базове ім'я** — усе до останнього `/` або `\`
/// відкидається, щоб `..\..\evil.exe` не поїхав кудись за межі теки.
#[must_use]
pub fn filename_from_disposition(value: &str) -> Option<String> {
    let mut plain: Option<String> = None;
    let mut extended: Option<String> = None;

    for part in split_params(value) {
        let Some((key, raw)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let raw = raw.trim();

        match key.as_str() {
            "filename" => plain = Some(unquote(raw)),
            "filename*" => extended = parse_ext_value(raw),
            _ => {}
        }
    }

    let name = extended.or(plain)?;
    let name = base_name(&name);

    if name.is_empty() { None } else { Some(name) }
}

/// Ім'я файла з шляху URL — запасний варіант, коли заголовка немає.
#[must_use]
pub fn filename_from_url(url: &str) -> Option<String> {
    // Відрізаємо фрагмент і запит: у `?token=…` імені немає.
    let path = url.split(['?', '#']).next()?;
    let last = path.rsplit('/').next()?;

    let decoded = percent_decode(last);
    let name = base_name(&decoded);

    if name.is_empty() { None } else { Some(name) }
}

/// Чи наступний символ робить бекслеш екрануванням.
///
/// ⚠️ Тут ми свідомо відступаємо від букви RFC 6266. За стандартом у
/// `quoted-string` бекслеш екранує **будь-який** наступний символ, і тоді
/// `"..\..\windows\evil.exe"` перетворюється на `....windowsevil.exe` —
/// розділювачі зникають, і `base_name` уже не має чого відрізати.
///
/// Реальні сервери (особливо на Windows) шлють у `filename` звичайні шляхи
/// з бекслешами й нічого не екранують. Тому робимо як браузери: бекслеш
/// екранує лише лапку й сам себе, у решті випадків це літерал. Так шлях
/// лишається шляхом — і `base_name` чесно ріже його до імені файла.
const fn is_escapable(next: char) -> bool {
    matches!(next, '"' | '\\')
}

/// Розбити значення заголовка на параметри, не ріжучи `;` всередині лапок.
fn split_params(value: &str) -> Vec<String> {
    let chars: Vec<char> = value.chars().collect();
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];

        if ch == '\\'
            && in_quotes
            && chars.get(i + 1).copied().is_some_and(is_escapable)
        {
            // Екранована пара — переносимо обидва символи, знімемо в `unquote`.
            current.push(ch);
            current.push(chars[i + 1]);
            i += 2;
            continue;
        }

        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            ';' if !in_quotes => parts.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
        i += 1;
    }

    parts.push(current);
    parts
}

/// Зняти лапки й екранування.
fn unquote(raw: &str) -> String {
    let trimmed = raw.trim();
    let Some(inner) = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
    else {
        return trimmed.to_owned();
    };

    let chars: Vec<char> = inner.chars().collect();
    let mut out = String::with_capacity(inner.len());
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '\\'
            && let Some(&next) = chars.get(i + 1)
            && is_escapable(next)
        {
            out.push(next);
            i += 2;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }

    out
}

/// Розібрати `filename*=UTF-8''%D1%84%D0%B0%D0%B9%D0%BB.zip` (RFC 5987).
///
/// Кодування, відмінне від UTF-8 і ASCII, не підтримуємо: краще відкотитись
/// на звичайний `filename`, ніж вгадувати кодову сторінку й отримати
/// «Ð¿ÑÐ¸Ð²ÑÑ» в імені файла.
fn parse_ext_value(raw: &str) -> Option<String> {
    let raw = raw.trim().trim_matches('"');
    let mut parts = raw.splitn(3, '\'');

    let charset = parts.next()?.trim();
    let _language = parts.next()?;
    let encoded = parts.next()?;

    if !(charset.eq_ignore_ascii_case("utf-8") || charset.eq_ignore_ascii_case("us-ascii")) {
        return None;
    }

    let decoded = percent_decode(encoded);
    if decoded.is_empty() { None } else { Some(decoded) }
}

/// Розкодувати `%XX`. Байти збираються, тоді тлумачаться як UTF-8.
///
/// Побайтова збірка принципова: кирилична літера — це два відсоткові байти,
/// і декодувати їх поодинці не можна.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &input[i + 1..i + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }

    String::from_utf8_lossy(&out).into_owned()
}

/// Лишити тільки базове ім'я: усе до останнього `/` або `\` — геть.
fn base_name(name: &str) -> String {
    name.rsplit(['/', '\\']).next().unwrap_or(name).trim().to_owned()
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]
mod tests {
    use super::*;

    // --- Accept-Ranges ---

    #[test]
    fn відсутній_accept_ranges_це_невідомо_а_не_відмова() {
        assert_eq!(RangeSupport::parse(None), RangeSupport::Unknown);
        assert_eq!(RangeSupport::parse(Some("bytes")), RangeSupport::Bytes);
        assert_eq!(RangeSupport::parse(Some("BYTES")), RangeSupport::Bytes);
        assert_eq!(RangeSupport::parse(Some("none")), RangeSupport::None);
    }

    // --- Content-Range ---

    #[test]
    fn content_range_дає_повний_розмір() {
        let cr = ContentRange::parse("bytes 0-0/12345").unwrap();
        assert_eq!(cr.start, 0);
        assert_eq!(cr.end, 0);
        assert_eq!(cr.total, Some(12345));
        assert_eq!(cr.len(), 1, "межі в HTTP включні: 0-0 це один байт");
    }

    #[test]
    fn content_range_із_зірочкою_не_знає_розміру() {
        let cr = ContentRange::parse("bytes 100-199/*").unwrap();
        assert_eq!(cr.total, None);
        assert_eq!(cr.len(), 100);
    }

    #[test]
    fn брехливий_content_range_відхиляється() {
        // Діапазон виходить за оголошений розмір — сервер бреше.
        assert!(ContentRange::parse("bytes 0-99/50").is_none());
        // Кінець раніше за початок.
        assert!(ContentRange::parse("bytes 99-0/500").is_none());
        // Не bytes і просто сміття.
        assert!(ContentRange::parse("items 0-9/50").is_none());
        assert!(ContentRange::parse("bytes abc").is_none());
    }

    // --- Validator ---

    #[test]
    fn слабкий_etag_не_йде_в_if_range() {
        let v = Validator::choose(Some("W/\"abc\""), Some("Wed, 21 Oct 2026 07:28:00 GMT"));
        assert_eq!(
            v,
            Validator::LastModified("Wed, 21 Oct 2026 07:28:00 GMT".into()),
            "слабкий ETag означає «схоже те саме», а не «побайтово те саме»"
        );
    }

    #[test]
    fn сильний_etag_має_пріоритет() {
        let v = Validator::choose(Some("\"abc123\""), Some("Wed, 21 Oct 2026 07:28:00 GMT"));
        assert_eq!(v, Validator::Strong("\"abc123\"".into()));
        assert_eq!(v.if_range_value(), Some("\"abc123\""));
    }

    #[test]
    fn без_жодної_ознаки_докачування_ненадійне() {
        let v = Validator::choose(None, None);
        assert_eq!(v, Validator::None);
        assert!(!v.is_reliable());
        assert_eq!(v.if_range_value(), None);
    }

    #[test]
    fn слабкий_etag_без_last_modified_лишає_нас_ні_з_чим() {
        let v = Validator::choose(Some("w/\"abc\""), None);
        assert!(!v.is_reliable(), "нічого надійного не лишилось");
    }

    // --- Content-Disposition ---

    #[test]
    fn просте_імʼя_в_лапках() {
        let name = filename_from_disposition("attachment; filename=\"report.pdf\"").unwrap();
        assert_eq!(name, "report.pdf");
    }

    #[test]
    fn імʼя_без_лапок() {
        let name = filename_from_disposition("attachment; filename=report.pdf").unwrap();
        assert_eq!(name, "report.pdf");
    }

    #[test]
    fn кирилиця_через_filename_star() {
        let name = filename_from_disposition(
            "attachment; filename=\"file.zip\"; filename*=UTF-8''%D0%B7%D0%B2%D1%96%D1%82.zip",
        )
        .unwrap();
        assert_eq!(name, "звіт.zip", "filename* має бути важливішим за filename");
    }

    #[test]
    fn невідоме_кодування_відкочується_на_звичайне_імʼя() {
        let name = filename_from_disposition(
            "attachment; filename=\"fallback.zip\"; filename*=ISO-8859-1''%E9t%E9.zip",
        )
        .unwrap();
        assert_eq!(
            name, "fallback.zip",
            "краще ASCII-запасне ім'я, ніж вгадана кодова сторінка"
        );
    }

    #[test]
    fn крапка_з_комою_в_лапках_не_ріже_параметр() {
        let name =
            filename_from_disposition("attachment; filename=\"звіт; чернетка.pdf\"").unwrap();
        assert_eq!(name, "звіт; чернетка.pdf");
    }

    #[test]
    fn спроба_вийти_з_теки_обрізається_до_базового_імені() {
        let name =
            filename_from_disposition("attachment; filename=\"..\\..\\windows\\evil.exe\"")
                .unwrap();
        assert_eq!(name, "evil.exe", "шлях із заголовка не має вести за межі теки");

        let unix = filename_from_disposition("attachment; filename=\"../../etc/passwd\"").unwrap();
        assert_eq!(unix, "passwd");
    }

    #[test]
    fn літеральний_бекслеш_у_шляху_не_зʼїдається() {
        // Windows-сервер шле шлях без екранування — бекслеші мають дожити до
        // `base_name`, інакше різати буде нічого.
        let name = filename_from_disposition("attachment; filename=\"C:\\temp\\звіт.pdf\"")
            .unwrap();
        assert_eq!(name, "звіт.pdf");
    }

    #[test]
    fn екрановані_лапки_всередині_імені() {
        let name = filename_from_disposition("attachment; filename=\"він \\\"той\\\".txt\"").unwrap();
        assert_eq!(name, "він \"той\".txt");
    }

    #[test]
    fn порожнє_або_безіменне_дає_нічого() {
        assert!(filename_from_disposition("attachment").is_none());
        assert!(filename_from_disposition("attachment; filename=\"\"").is_none());
        assert!(filename_from_disposition("attachment; filename=\"/\"").is_none());
    }

    // --- ім'я з URL ---

    #[test]
    fn імʼя_з_url_без_запиту() {
        let name = filename_from_url("https://example.com/files/setup.exe?token=abc#frag").unwrap();
        assert_eq!(name, "setup.exe");
    }

    #[test]
    fn імʼя_з_url_розкодовує_відсотки() {
        let name = filename_from_url("https://example.com/%D0%B7%D0%B2%D1%96%D1%82.pdf").unwrap();
        assert_eq!(name, "звіт.pdf");
    }

    #[test]
    fn url_без_імені_дає_нічого() {
        assert!(filename_from_url("https://example.com/").is_none());
        assert!(filename_from_url("https://example.com/?a=1").is_none());
    }

    #[test]
    fn биті_відсотки_не_ламають_розбір() {
        // `%ZZ` — не шістнадцяткове; має лишитись як є, а не з'їсти рядок.
        let name = filename_from_url("https://example.com/file%ZZ.txt").unwrap();
        assert_eq!(name, "file%ZZ.txt");
    }
}
