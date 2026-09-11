//! Безпечні імена файлів для Windows.
//!
//! Ім'я приходить із мережі — з `Content-Disposition` або з URL, тобто його
//! складала стороння людина, іноді зумисне. Windows же має цілий набір
//! правил, порушення яких дає не помилку, а **тихо інший файл**: кінцеву
//! крапку система з'їдає мовчки, `CON.txt` створити неможливо взагалі, а
//! двокрапка перетворює решту імені на альтернативний потік даних.
//!
//! Тому ім'я з мережі ніколи не йде на диск як є.

/// Символи, заборонені у файлових іменах Windows.
///
/// Двокрапка найпідступніша: `звіт:секрет.txt` — це не ім'я з двокрапкою, а
/// файл `звіт` з альтернативним потоком `секрет.txt`. Дані ляжуть у потік і
/// стануть невидимими у провіднику.
const ЗАБОРОНЕНІ: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Імена пристроїв DOS. Зайняті на рівні системи в **будь-якій** теці.
///
/// Пастка в тому, що правило діє і з розширенням: `CON.txt` так само
/// неможливий, як `CON`.
const ПРИСТРОЇ: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Запасне ім'я, коли від запропонованого нічого не лишилось.
pub const ЗАПАСНЕ_ІМʼЯ: &str = "downloaded.bin";

/// Розширення за `Content-Type`, якщо його взагалі варто вгадувати.
///
/// `application/octet-stream` свідомо пропускаємо: це «байти без типу»,
/// гадати `.bin` за ним — підміняти ім'я. Параметри після `;` ігноруємо.
#[must_use]
pub fn extension_for_mime(mime: &str) -> Option<&'static str> {
    let main = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    if main.is_empty() || main == "application/octet-stream" {
        return None;
    }
    Some(match main.as_str() {
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        "video/x-matroska" => "mkv",
        "video/mp2t" => "ts",
        "video/x-msvideo" => "avi",
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "audio/ogg" => "ogg",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/aac" => "aac",
        "audio/flac" => "flac",
        "audio/webm" => "weba",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        "application/gzip" => "gz",
        "application/json" => "json",
        "application/dash+xml" => "mpd",
        "application/x-subrip" => "srt",
        "application/x-7z-compressed" => "7z",
        "application/x-tar" => "tar",
        "application/wasm" => "wasm",
        "application/xml" | "text/xml" => "xml",
        "application/javascript" | "text/javascript" => "js",
        "text/plain" => "txt",
        "text/html" => "html",
        "text/csv" => "csv",
        "text/vtt" => "vtt",
        "application/vnd.apple.mpegurl" | "application/x-mpegurl" => "m3u8",
        _ => return None,
    })
}

/// Дописати розширення з MIME, якщо в імені його ще немає.
#[must_use]
pub fn з_розширенням_mime(raw: &str, mime: Option<&str>) -> String {
    let cleaned = sanitize(raw);
    let Some(mime) = mime else {
        return cleaned;
    };
    let Some(ext) = extension_for_mime(mime) else {
        return cleaned;
    };
    if розділити(&cleaned).1.is_some() {
        return cleaned;
    }
    format!("{cleaned}.{ext}")
}

/// Скільки байтів дозволяємо імені файла.
///
/// Windows рахує ліміт у 255 символів UTF-16 на компонент шляху. Беремо з
/// запасом: кириличне ім'я в UTF-8 удвічі довше за латинське, і різатимемо
/// ми саме байти.
const МАКС_БАЙТІВ: usize = 200;

/// Зробити з мережевого імені таке, яке Windows точно прийме.
///
/// Що робиться:
/// * лишається лише базове ім'я — жодних `/` і `\`;
/// * заборонені символи й керуючі байти замінюються на `_`;
/// * кінцеві крапки й пробіли зрізаються;
/// * ім'я пристрою отримує підкреслення: `CON.txt` → `CON_.txt`;
/// * задовге ім'я вкорочується **зі збереженням розширення**.
#[must_use]
pub fn sanitize(raw: &str) -> String {
    // Шлях із заголовка не має вести за межі теки.
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);

    let mut cleaned: String = base
        .chars()
        .map(|c| {
            if ЗАБОРОНЕНІ.contains(&c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();

    // Windows мовчки прибирає кінцеві крапки й пробіли: попросиш «звіт.» —
    // отримаєш «звіт», і подальший пошук файла за іменем не знайде нічого.
    cleaned = cleaned.trim_end_matches(['.', ' ']).trim_start().to_owned();

    if cleaned.is_empty() {
        return ЗАПАСНЕ_ІМʼЯ.to_owned();
    }

    cleaned = обійти_імʼя_пристрою(&cleaned);
    вкоротити(&cleaned)
}

/// `CON.txt` → `CON_.txt`.
fn обійти_імʼя_пристрою(name: &str) -> String {
    let (stem, ext) = розділити(name);

    let is_device = ПРИСТРОЇ.iter().any(|d| stem.eq_ignore_ascii_case(d));

    if is_device {
        match ext {
            Some(e) => format!("{stem}_.{e}"),
            None => format!("{stem}_"),
        }
    } else {
        name.to_owned()
    }
}

/// Вкоротити ім'я, не втративши розширення.
///
/// Розширення важливіше за середину імені: за ним система обирає програму,
/// а обрізане `.pdf` перетворює документ на невідомий файл.
fn вкоротити(name: &str) -> String {
    if name.len() <= МАКС_БАЙТІВ {
        return name.to_owned();
    }

    let (stem, ext) = розділити(name);
    let ext_len = ext.map_or(0, |e| e.len() + 1);

    // Якщо саме «розширення» довше за ліміт, це не розширення, а сміття —
    // ріжемо все ім'я цілком.
    if ext_len >= МАКС_БАЙТІВ {
        return обрізати_по_символах(name, МАКС_БАЙТІВ);
    }

    let stem_room = МАКС_БАЙТІВ - ext_len;
    let short_stem = обрізати_по_символах(stem, stem_room);

    match ext {
        Some(e) => format!("{short_stem}.{e}"),
        None => short_stem,
    }
}

/// Обрізати до `limit` байтів, не розрубавши символ навпіл.
fn обрізати_по_символах(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.to_owned();
    }

    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// Розділити на основу й розширення.
///
/// ⚠️ Не все після останньої крапки є розширенням, і плутанина тут дорого
/// коштує. `archive.001` — це перший том розбитого архіву, а не файл типу
/// `001`; `app.v1.11.1` — версія в імені. Якщо вважати їх розширеннями, то
/// при вкороченні довгого імені ми «дбайливо збережемо» `.001`, а категорія
/// шукатиме тип, якого не існує.
///
/// Тому розширенням вважається лише хвіст, який на нього схожий: від одного
/// до восьми алфавітно-цифрових символів і **не самі цифри**.
fn розділити(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('.') {
        // Крапка на початку — це прихований файл, а не розширення.
        Some((stem, ext)) if !stem.is_empty() && схоже_на_розширення(ext) => {
            (stem, Some(ext))
        }
        _ => (name, None),
    }
}

/// Чи цей хвіст справді схожий на розширення файла.
fn схоже_на_розширення(ext: &str) -> bool {
    let довжина_ок = (1..=8).contains(&ext.chars().count());
    let лише_букви_й_цифри = ext.chars().all(|c| c.is_alphanumeric());
    let самі_цифри = ext.chars().all(|c| c.is_ascii_digit());

    довжина_ок && лише_букви_й_цифри && !самі_цифри
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]
mod tests {
    use super::*;

    #[test]
    fn звичайне_імʼя_не_чіпається() {
        assert_eq!(sanitize("звіт за 2026.pdf"), "звіт за 2026.pdf");
        assert_eq!(sanitize("archive.tar.gz"), "archive.tar.gz");
    }

    #[test]
    fn шлях_зрізається_до_базового_імені() {
        assert_eq!(sanitize("..\\..\\windows\\evil.exe"), "evil.exe");
        assert_eq!(sanitize("/etc/passwd"), "passwd");
    }

    #[test]
    fn двокрапка_не_створює_прихованого_потоку() {
        // `звіт:секрет.txt` на NTFS — це файл `звіт` з альтернативним
        // потоком: дані стали б невидимими у провіднику.
        assert_eq!(sanitize("звіт:секрет.txt"), "звіт_секрет.txt");
    }

    #[test]
    fn заборонені_символи_замінюються() {
        assert_eq!(sanitize("що<>|?*\"це.bin"), "що_____\u{5f}це.bin");
    }

    #[test]
    fn керуючі_символи_прибираються() {
        let name = sanitize("файл\u{0}\u{1}\u{1f}.txt");
        assert!(
            !name.chars().any(char::is_control),
            "керуючі байти в імені: {name:?}"
        );
    }

    #[test]
    fn кінцева_крапка_зрізається() {
        // Windows з'їдає її мовчки — файл на диску називався б інакше, ніж
        // ми думаємо, і пошук за іменем нічого б не знайшов.
        assert_eq!(sanitize("report.bin."), "report.bin");
        assert_eq!(sanitize("report.bin   "), "report.bin");
        assert_eq!(sanitize("report. . ."), "report");
    }

    #[test]
    fn імʼя_пристрою_знешкоджується() {
        assert_eq!(sanitize("CON.txt"), "CON_.txt");
        assert_eq!(sanitize("con.txt"), "con_.txt", "регістр не має значення");
        assert_eq!(sanitize("NUL"), "NUL_");
        assert_eq!(sanitize("COM1.log"), "COM1_.log");
        assert_eq!(sanitize("LPT9"), "LPT9_");
    }

    #[test]
    fn схоже_на_пристрій_але_не_воно() {
        assert_eq!(sanitize("CONSOLE.txt"), "CONSOLE.txt");
        assert_eq!(
            sanitize("COM10.log"),
            "COM10.log",
            "COM10 не зарезервований"
        );
    }

    #[test]
    fn порожнє_імʼя_замінюється_запасним() {
        assert_eq!(sanitize(""), ЗАПАСНЕ_ІМʼЯ);
        assert_eq!(sanitize("   "), ЗАПАСНЕ_ІМʼЯ);
        assert_eq!(sanitize("..."), ЗАПАСНЕ_ІМʼЯ);
        assert_eq!(sanitize("/"), ЗАПАСНЕ_ІМʼЯ);
    }

    #[test]
    fn задовге_імʼя_вкорочується_зі_збереженням_розширення() {
        let long = format!("{}.pdf", "я".repeat(300));
        let out = sanitize(&long);

        assert!(
            out.len() <= МАКС_БАЙТІВ,
            "ім'я довше за ліміт: {}",
            out.len()
        );
        assert!(
            out.ends_with(".pdf"),
            "розширення втрачено — система не знатиме, чим відкривати: {out}"
        );
    }

    #[test]
    fn вкорочення_не_рубає_символ_навпіл() {
        // Кирилична літера — два байти; зріз посеред неї дав би недійсний UTF-8.
        let long = "ї".repeat(300);
        let out = sanitize(&long);

        assert!(out.len() <= МАКС_БАЙТІВ);
        assert!(
            out.chars().all(|c| c == 'ї'),
            "у вкороченому імені з'явилось сміття: {out:?}"
        );
    }

    #[test]
    fn цифровий_хвіст_не_вважається_розширенням() {
        // `.001` — том розбитого архіву. Вкорочення з «збереженням
        // розширення» зіпсувало б ім'я, а категорія шукала б тип «001».
        let (stem, ext) = розділити("archive.001");
        assert_eq!(stem, "archive.001");
        assert_eq!(ext, None);

        let (stem, ext) = розділити("app.v1.11.1");
        assert_eq!(stem, "app.v1.11.1");
        assert_eq!(ext, None, "версія в імені — не розширення");
    }

    #[test]
    fn задовгий_хвіст_не_розширення() {
        let (_, ext) = розділити("файл.цедужедовгийхвіст");
        assert_eq!(
            ext, None,
            "дев'ять і більше символів — це вже не розширення"
        );
    }

    #[test]
    fn хвіст_із_символами_не_розширення() {
        let (_, ext) = розділити("звіт.за-2026");
        assert_eq!(ext, None);
    }

    #[test]
    fn octet_stream_не_вгадується() {
        assert_eq!(extension_for_mime("application/octet-stream"), None);
        assert_eq!(
            extension_for_mime("application/octet-stream; charset=binary"),
            None
        );
        assert_eq!(extension_for_mime(""), None);
        assert_eq!(extension_for_mime("application/binary"), None);
    }

    #[test]
    fn відомі_mime_дають_розширення() {
        assert_eq!(extension_for_mime("video/mp4"), Some("mp4"));
        assert_eq!(extension_for_mime("VIDEO/MP4; codecs=avc1"), Some("mp4"));
        assert_eq!(extension_for_mime("image/jpeg"), Some("jpg"));
        assert_eq!(
            extension_for_mime("application/vnd.apple.mpegurl"),
            Some("m3u8")
        );
        assert_eq!(extension_for_mime("application/x-невідоме"), None);
        assert_eq!(extension_for_mime("application/dash+xml"), Some("mpd"));
        assert_eq!(extension_for_mime("text/vtt"), Some("vtt"));
        assert_eq!(extension_for_mime("application/x-subrip"), Some("srt"));
        assert_eq!(extension_for_mime("audio/aac"), Some("aac"));
        assert_eq!(extension_for_mime("audio/flac"), Some("flac"));
        assert_eq!(extension_for_mime("audio/webm"), Some("weba"));
        assert_eq!(extension_for_mime("video/x-msvideo"), Some("avi"));
        assert_eq!(
            extension_for_mime("application/x-7z-compressed"),
            Some("7z")
        );
        assert_eq!(extension_for_mime("application/x-tar"), Some("tar"));
        assert_eq!(extension_for_mime("application/wasm"), Some("wasm"));
        assert_eq!(extension_for_mime("image/svg+xml"), Some("svg"));
        assert_eq!(extension_for_mime("application/xml"), Some("xml"));
        assert_eq!(extension_for_mime("text/xml"), Some("xml"));
        assert_eq!(extension_for_mime("application/javascript"), Some("js"));
        assert_eq!(extension_for_mime("text/javascript"), Some("js"));
    }

    #[test]
    fn параметр_після_крапки_з_комою_ігнорується_для_vtt() {
        assert_eq!(extension_for_mime("text/vtt; charset=utf-8"), Some("vtt"));
    }

    #[test]
    fn розширення_дописується_лише_коли_його_немає() {
        assert_eq!(з_розширенням_mime("фільм", Some("video/mp4")), "фільм.mp4");
        assert_eq!(
            з_розширенням_mime("фільм.webm", Some("video/mp4")),
            "фільм.webm",
            "існуюче розширення не підміняємо"
        );
        assert_eq!(
            з_розширенням_mime("дані", Some("application/octet-stream")),
            "дані"
        );
    }

    #[test]
    fn звичайні_розширення_розпізнаються() {
        assert_eq!(розділити("звіт.pdf").1, Some("pdf"));
        assert_eq!(розділити("архів.tar.gz").1, Some("gz"));
        assert_eq!(розділити("відео.mp4").1, Some("mp4"));
        assert_eq!(розділити("книга.epub").1, Some("epub"));
        assert_eq!(
            розділити("шрифт.woff2").1,
            Some("woff2"),
            "цифра всередині — це нормально"
        );
    }

    #[test]
    fn вкорочення_не_чіпає_цифровий_хвіст() {
        // Якби `.001` вважався розширенням, ім'я вийшло б безглуздим.
        let long = format!("{}.001", "я".repeat(300));
        let out = sanitize(&long);

        assert!(out.len() <= МАКС_БАЙТІВ);
        assert!(
            !out.ends_with(".001"),
            "цифровий хвіст не мав зберігатись як розширення: {out}"
        );
    }

    #[test]
    fn прихований_файл_не_втрачає_крапку_на_початку() {
        assert_eq!(sanitize(".gitignore"), ".gitignore");
    }

    #[test]
    fn санітизоване_імʼя_завжди_придатне() {
        // Що б не прийшло — результат мусить бути придатним іменем.
        for raw in [
            "CON",
            "  ",
            "..\\..\\..\\CON.txt",
            "a:b:c",
            &"x".repeat(500),
            "файл\u{7}.bin",
            "...",
        ] {
            let out = sanitize(raw);
            assert!(!out.is_empty(), "порожній результат для {raw:?}");
            assert!(out.len() <= МАКС_БАЙТІВ, "задовгий результат для {raw:?}");
            assert!(
                !out.contains(ЗАБОРОНЕНІ),
                "заборонений символ лишився: {out:?}"
            );
            assert!(
                !out.ends_with('.') && !out.ends_with(' '),
                "кінцева крапка або пробіл: {out:?}"
            );
        }
    }
}
