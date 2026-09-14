//! YouTube через зовнішній `yt-dlp`, не свій екстрактор.
//!
//! Бінарник шукається в PATH. Немає — чесна помилка, не тиха заглушка.
//! JSON `-J` не пишемо в журнал: там прямі URL.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use downloader_core::error::{Error, Result};
use downloader_core::protocol::{
    PlannedFile, Probed, ProgressSink, Protocol, RateLimitSupport, ResumeBlob, RunContext, Variant,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

/// Модуль YouTube / yt-dlp.
pub struct YtdlpProtocol {
    /// Ім'я або шлях бінарника. У тестах підміняється.
    bin: PathBuf,
}

/// Прибрати з рядка адреси й значення токенів, **лишивши текст**.
///
/// ⚠️ Раніше тут ховався весь рядок, у якому трапилось `://` або слово
/// «token». Наслідок бачили на живому прикладі: yt-dlp сказав
/// «Requested format is not available», а людина прочитала «[приховано]» —
/// тобто програма забрала в неї саме те, заради чого повідомлення й існує.
///
/// Тому ріжемо не рядок, а підрядок: адресу до першого пробілу й значення
/// після `token=`. Решта тексту доходить до людини цілою.
fn без_секретів_у_рядку(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut решта = line;

    loop {
        let Some(секрет) = знайти_секрет(решта) else {
            out.push_str(решта);
            return out;
        };

        out.push_str(&решта[..секрет.початок]);
        out.push_str("[приховано]");
        решта = &решта[секрет.кінець..];
    }
}

struct Секрет {
    початок: usize,
    кінець: usize,
}

/// Найближчий секрет у рядку: адреса або значення токена.
fn знайти_секрет(s: &str) -> Option<Секрет> {
    let адреса = s.find("://").map(|i| {
        // Схема стоїть перед `://` — відступаємо до початку слова.
        let початок = s[..i]
            .rfind([' ', '\t', '"', '\''])
            .map_or(0, |j| j + 1);
        let кінець = s[i..]
            .find([' ', '\t', '"'])
            .map_or(s.len(), |j| i + j);
        Секрет { початок, кінець }
    });

    let токен = знайти_значення_токена(s);

    match (адреса, токен) {
        (Some(a), Some(t)) if t.початок < a.початок => Some(t),
        (Some(a), _) => Some(a),
        (None, t) => t,
    }
}

/// Значення після `token=` — саме значення, а не слово «token» у тексті.
fn знайти_значення_токена(s: &str) -> Option<Секрет> {
    let нижній = s.to_ascii_lowercase();
    let i = нижній.find("token")?;
    let після = i + "token".len();

    let хвіст = s.get(після..)?;
    let зсув = хвіст.find(['=', ':'])?;
    if !хвіст[..зсув].trim().is_empty() {
        return None;
    }

    let значення = після + зсув + 1;
    let значення = значення + (s.len() - значення) - s[значення..].trim_start().len();
    let кінець = s[значення..]
        .find([' ', '\t', '"', '&'])
        .map_or(s.len(), |j| значення + j);

    (кінець > значення).then_some(Секрет {
        початок: значення,
        кінець,
    })
}

impl YtdlpProtocol {
    /// Шукати `yt-dlp` у PATH.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bin: PathBuf::from("yt-dlp"),
        }
    }

    #[must_use]
    pub fn with_bin(bin: impl Into<PathBuf>) -> Self {
        Self { bin: bin.into() }
    }

    /// Самооновлення бінарника: `yt-dlp -U`. Текст без URL і токенів.
    pub async fn self_update(&self) -> Result<String> {
        let out = Command::new(&self.bin)
            .args(["-U", "--no-warnings"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|_| Error::Store("yt-dlp не знайдено в PATH".to_owned()))?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        if text.trim().is_empty() {
            text = String::from_utf8_lossy(&out.stderr).into_owned();
        }
        let text = обрізати_секрети(&text);
        if !out.status.success() && text.contains("not found") {
            return Err(Error::Store("yt-dlp не знайдено в PATH".to_owned()));
        }
        if !out.status.success() {
            return Err(Error::Store(format!("yt-dlp -U: {text}")));
        }
        Ok(text)
    }
}

impl Default for YtdlpProtocol {
    fn default() -> Self {
        Self::new()
    }
}

fn хост(source: &str) -> Option<String> {
    let rest = source
        .strip_prefix("https://")
        .or_else(|| source.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    Some(host.trim_start_matches("www.").to_ascii_lowercase())
}

/// Чи це адреса YouTube.
#[must_use]
pub fn схожий_на_youtube(source: &str) -> bool {
    matches!(
        хост(source).as_deref(),
        Some("youtube.com" | "youtu.be" | "music.youtube.com" | "m.youtube.com")
    )
}

fn імʼя_з_title(title: &str) -> String {
    let mut s: String = title
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    s.truncate(180);
    let s = s.trim().trim_end_matches('.');
    if s.is_empty() {
        "video".to_owned()
    } else {
        s.to_owned()
    }
}

fn обрізати_секрети(stderr: &str) -> String {
    stderr
        .lines()
        .map(без_секретів_у_рядку)
        .collect::<Vec<_>>()
        .join("\n")
}

impl YtdlpProtocol {
    /// Один виклик `yt-dlp -J` на всі потреби проби.
    ///
    /// `variant` змінює не запит, а те, що ми з відповіді беремо: розмір і
    /// розширення залежать від обраної якості, перелік варіантів — ні.
    async fn опитати(&self, source: &str, variant: Option<&str>) -> Result<Probed> {
        let out = Command::new(&self.bin)
            .args(["-J", "--no-download", "--no-warnings", source])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|_| Error::Store("yt-dlp не знайдено в PATH".to_owned()))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if err.contains("not found") || err.contains("не знайдено") {
                return Err(Error::Store("yt-dlp не знайдено в PATH".to_owned()));
            }
            return Err(Error::Store(format!(
                "yt-dlp -J завершився з помилкою: {}",
                обрізати_секрети(&err)
            )));
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout)
            .map_err(|e| Error::Store(format!("yt-dlp віддав не JSON: {e}")))?;
        let title = v
            .get("title")
            .and_then(|x| x.as_str())
            .unwrap_or("video");
        let ext = v.get("ext").and_then(|x| x.as_str()).unwrap_or("mp4");
        let name = format!("{}.{}", імʼя_з_title(title), ext);
        let variants = варіанти_з_json(&v);

        // Розмір беремо з обраного варіанта: 720p важить не стільки,
        // скільки 360p, і показати одне число на всі якості означало б
        // збрехати в усіх випадках, крім одного.
        let розмір_обраного = variant
            .and_then(|id| variants.iter().find(|x| x.id == id))
            .and_then(|x| x.size)
            .or_else(|| v.get("filesize").and_then(|x| x.as_u64()));

        Ok(Probed {
            final_url: source.to_owned(),
            total_size: розмір_обраного,
            resumable: false,
            fingerprint: Some("ytdlp".to_owned()),
            files: vec![PlannedFile {
                suggested_name: name,
                size: розмір_обраного,
                selected: true,
            }],
            variants,
        })
    }
}

#[async_trait]
impl Protocol for YtdlpProtocol {
    fn name(&self) -> &'static str {
        "ytdlp"
    }

    fn handles(&self, source: &str) -> bool {
        схожий_на_youtube(source)
    }

    async fn probe(&self, source: &str) -> Result<Probed> {
        self.опитати(source, None).await
    }

    async fn probe_variant(&self, source: &str, variant: Option<&str>) -> Result<Probed> {
        self.опитати(source, variant).await
    }

    async fn run(&self, ctx: RunContext, _sink: &dyn ProgressSink) -> Result<Option<ResumeBlob>> {
        let Some(dest) = ctx.targets.first() else {
            return Err(Error::Store(
                "ядро не дало жодного шляху для запису".to_owned(),
            ));
        };
        let шаблон = dest_шаблон(dest);

        // Без `-f` yt-dlp бере найкраще, що знайде, — і вибір людини не
        // мав би жодного значення.
        let формат = селектор(ctx.variant.as_deref(), є_ffmpeg());

        let mut child = Command::new(&self.bin)
            .args([
                "--no-progress",
                "-f",
                &формат,
                "-o",
                &шаблон,
                &ctx.source,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| Error::Store("yt-dlp не знайдено в PATH".to_owned()))?;

        let stderr = child.stderr.take();
        let читання = async {
            let mut tail = String::new();
            if let Some(pipe) = stderr {
                let mut lines = BufReader::new(pipe).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if tail.len() < 4096 {
                        tail.push_str(&line);
                        tail.push('\n');
                    }
                }
            }
            tail
        };

        let wait = async {
            loop {
                if ctx.cancel.is_cancelled() {
                    if let Err(e) = child.start_kill() {
                        tracing::warn!("не вбити yt-dlp: {e}");
                    }
                    return Ok(None);
                }
                match child.try_wait() {
                    Ok(Some(status)) => return Ok(Some(status)),
                    Ok(None) => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
                    Err(e) => return Err(Error::Store(e.to_string())),
                }
            }
        };

        let (tail, status) = tokio::join!(читання, wait);
        let Some(status) = status? else {
            return Ok(Some(Vec::new()));
        };
        if !status.success() {
            return Err(пояснити_невдачу(&tail));
        }

        перевірити_результат(dest)?;
        Ok(None)
    }

    fn set_rate_limit(&self, _bytes_per_sec: u64) -> RateLimitSupport {
        RateLimitSupport::Unsupported
    }
}

/// Перекласти відмову yt-dlp зрозумілою мовою.
///
/// ⚠️ «Requested format is not available» при відсутньому ffmpeg майже
/// завжди означає одне: ми попросили готовий файл, бо звести доріжки нічим,
/// а джерело роздає відео й звук **тільки** окремо. Людині корисно знати
/// причину, а не чужий текст: сучасний YouTube не має жодного суміщеного
/// формату навіть для 144p.
fn пояснити_невдачу(stderr: &str) -> Error {
    let чисто = обрізати_секрети(stderr);

    if чисто.contains("Requested format is not available") && !є_ffmpeg() {
        return Error::Store(
            "потрібен ffmpeg: це джерело роздає відео й звук окремими доріжками, а звести їх в один файл більше нічим"
                .to_owned(),
        );
    }

    Error::Store(format!("yt-dlp впав: {чисто}"))
}

/// Чи є ffmpeg, яким yt-dlp зводить відео зі звуком.
///
/// ⚠️ Для YouTube це не зручність, а умова роботи: сучасні відео роздаються
/// **роздільно** — доріжка відео окремо, доріжка звуку окремо, і навіть
/// 144p не існує у вигляді одного файла. Без ffmpeg качання закінчується
/// двома уламками замість відео.
fn є_ffmpeg() -> bool {
    let поруч = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(ffmpeg_імʼя())));

    if поруч.is_some_and(|p| p.is_file()) {
        return true;
    }

    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };

    std::env::split_paths(&path).any(|d| d.join(ffmpeg_імʼя()).is_file())
}

fn ffmpeg_імʼя() -> &'static str {
    if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" }
}

/// Чи справді з'явився файл, якого ми просили.
///
/// ⚠️ Нульовий код виходу yt-dlp цього **не** означає. Коли він завантажив
/// доріжки, але не зміг їх звести, він звітує про успіх і лишає поруч
/// уламки `назва.f251.webm`. Без цієї перевірки завдання рапортує «готово,
/// 0 байтів» — тобто програма бреше людині просто в очі.
fn перевірити_результат(dest: &Path) -> Result<()> {
    if dest.metadata().is_ok_and(|m| m.len() > 0) {
        return Ok(());
    }

    if уламки_поруч(dest) {
        return Err(Error::Store(format!(
            "yt-dlp завантажив відео й звук окремо, але не зміг звести їх в один              файл — для цього потрібен ffmpeg: {}",
            dest.display()
        )));
    }

    Err(Error::Store(format!(
        "yt-dlp завершився без помилки, але файла немає: {}",
        dest.display()
    )))
}

/// Чи лежать поруч незведені доріжки `назва.fNNN.ext`.
fn уламки_поруч(dest: &Path) -> bool {
    let (Some(dir), Some(основа)) = (dest.parent(), dest.file_name()) else {
        return false;
    };

    let основа = основа.to_string_lossy();
    let Ok(вміст) = std::fs::read_dir(dir) else {
        return false;
    };

    вміст.filter_map(std::result::Result::ok).any(|e| {
        let імʼя = e.file_name();
        let імʼя = імʼя.to_string_lossy();
        імʼя.starts_with(основа.as_ref()) && імʼя != основа
    })
}

fn dest_шаблон(dest: &Path) -> String {
    dest.to_string_lossy().into_owned()
}

/// Ідентифікатор варіанта «лише звук».
const ЛИШЕ_ЗВУК: &str = "audio";

/// Варіанти якості з відповіді `yt-dlp -J`.
///
/// # Чому не `format_id`
///
/// Спокуса віддати людині перелік форматів як є велика — і хибна. На одне
/// відео YouTube дає два десятки рядків, де на кожну висоту припадає
/// чотири-п'ять записів: avc1, vp9, av01, різні бітрейти. Ніхто не обирає
/// «vp09.00.20.0 проти av01.0.00M.0» — обирають «720p».
///
/// Тому згортаємо до висот, а вибір кодека лишаємо yt-dlp: він і так знає,
/// що доступне саме зараз. Додаткова вигода — стійкість: перелік форматів
/// між запусками змінюється, набір висот практично ні.
fn варіанти_з_json(v: &serde_json::Value) -> Vec<Variant> {
    let Some(formats) = v.get("formats").and_then(|x| x.as_array()) else {
        return Vec::new();
    };

    let тривалість = v.get("duration").and_then(serde_json::Value::as_f64);

    // Найкращий звук — ним доповнюється будь-яка висота: YouTube роздає
    // відео й звук окремо, і без цього доданку розмір був би заниженим.
    let звук = formats
        .iter()
        .filter(|f| має_звук(f) && !має_відео(f))
        .max_by(|a, b| бітрейт(a).total_cmp(&бітрейт(b)));

    let розмір_звуку = звук.and_then(|f| розмір(f, тривалість));

    let mut за_висотою: std::collections::BTreeMap<u32, &serde_json::Value> =
        std::collections::BTreeMap::new();

    for f in formats.iter().filter(|f| має_відео(f)) {
        let Some(h) = f.get("height").and_then(serde_json::Value::as_u64) else {
            continue;
        };
        let h = u32::try_from(h).unwrap_or(u32::MAX);

        за_висотою
            .entry(h)
            .and_modify(|краще| {
                if бітрейт(f) > бітрейт(краще) {
                    *краще = f;
                }
            })
            .or_insert(f);
    }

    let mut out: Vec<Variant> = за_висотою
        .into_iter()
        .rev()
        .map(|(h, f)| Variant {
            id: h.to_string(),
            label: format!("{h}p"),
            height: Some(h),
            size: розмір(f, тривалість)
                .map(|байтів| байтів + розмір_звуку.unwrap_or(0)),
            note: кодек(f),
        })
        .collect();

    if let Some(f) = звук {
        out.push(Variant {
            id: ЛИШЕ_ЗВУК.to_owned(),
            label: "лише звук".to_owned(),
            height: None,
            size: розмір_звуку,
            note: кодек(f),
        });
    }

    out
}

fn має_відео(f: &serde_json::Value) -> bool {
    !matches!(f.get("vcodec").and_then(|x| x.as_str()), None | Some("none"))
}

fn має_звук(f: &serde_json::Value) -> bool {
    !matches!(f.get("acodec").and_then(|x| x.as_str()), None | Some("none"))
}

fn бітрейт(f: &serde_json::Value) -> f64 {
    f.get("tbr")
        .or_else(|| f.get("abr"))
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0)
}

/// Розмір формату: точний, приблизний або порахований із бітрейта.
///
/// ⚠️ `filesize` у YouTube частіше відсутній, ніж присутній, а людина хоче
/// бачити, скільки качати. Порахувати з бітрейта й тривалості — не точно,
/// але на порядок правдивіше за прочерк.
fn розмір(f: &serde_json::Value, тривалість: Option<f64>) -> Option<u64> {
    if let Some(n) = f.get("filesize").and_then(serde_json::Value::as_u64) {
        return Some(n);
    }
    if let Some(n) = f.get("filesize_approx").and_then(serde_json::Value::as_u64) {
        return Some(n);
    }

    let секунд = тривалість?;
    let kbps = бітрейт(f);
    if kbps <= 0.0 || секунд <= 0.0 {
        return None;
    }

    // kbps — кілобіти за секунду; ділимо на 8 і множимо на 1000.
    Some((kbps * секунд * 125.0) as u64)
}

fn кодек(f: &serde_json::Value) -> Option<String> {
    let c = f
        .get("vcodec")
        .and_then(|x| x.as_str())
        .filter(|s| *s != "none")
        .or_else(|| f.get("acodec").and_then(|x| x.as_str()))?;

    // Повний рядок на кшталт `avc1.4d400c` нічого не додає: людині
    // потрібна родина кодека, а не профіль.
    Some(c.split('.').next().unwrap_or(c).to_owned())
}

/// Селектор `-f` для обраного варіанта.
///
/// `None` — вибору не було: хай yt-dlp бере найкраще, як робив досі.
///
/// ⚠️ `зі_злиттям` — не оптимізація, а межа можливого. Просити окремі
/// доріжки там, де їх нічим звести, означає завантажити два уламки й не
/// отримати відео: саме так поводиться yt-dlp, коли ffmpeg немає.
fn селектор(variant: Option<&str>, зі_злиттям: bool) -> String {
    match (variant, зі_злиттям) {
        (None, true) => "bv*+ba/b".to_owned(),
        (None, false) => "b".to_owned(),

        (Some(ЛИШЕ_ЗВУК), _) => "ba/b".to_owned(),

        // Не «рівно ця висота», а «не вище»: рівно цієї може не бути саме
        // зараз, і тоді качати було б нічого.
        (Some(висота), true) => {
            format!("bv*[height<={висота}]+ba/b[height<={висота}]")
        }
        (Some(висота), false) => format!("b[height<={висота}]"),
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "тест")]
mod tests {
    use super::*;

    #[test]
    fn впізнає_youtube() {
        let p = YtdlpProtocol::new();
        assert!(p.handles("https://www.youtube.com/watch?v=abc"));
        assert!(p.handles("https://youtu.be/abc"));
        assert!(p.handles("https://music.youtube.com/watch?v=abc"));
        assert!(!p.handles("https://example.com/watch?v=abc"));
        assert!(!p.handles("https://cdn.example/a.m3u8"));
    }

    /// Справжня відповідь `yt-dlp -J` для «Me at the zoo».
    ///
    /// Зразок узято з живого запиту й обрізано до потрібних полів. Вигадувати
    /// його не можна: саме на справжній формі — п'ять форматів на одну
    /// висоту, відсутній `filesize`, окремі доріжки відео й звуку — цей
    /// розбір і ламається.
    const ЗРАЗОК: &str = include_str!("../tests/fixtures/youtube-me-at-the-zoo.json");

    fn зразок() -> serde_json::Value {
        serde_json::from_str(ЗРАЗОК).expect("фікстура має бути валідним JSON")
    }

    #[test]
    fn два_десятки_форматів_згортаються_до_висот() {
        let варіанти = варіанти_з_json(&зразок());

        let висоти: Vec<&str> = варіанти
            .iter()
            .filter(|x| x.height.is_some())
            .map(|x| x.id.as_str())
            .collect();

        assert!(
            висоти.len() < 24,
            "згортання не спрацювало: {} варіантів на 24 формати",
            висоти.len()
        );

        let унікальні: std::collections::BTreeSet<_> = висоти.iter().collect();
        assert_eq!(унікальні.len(), висоти.len(), "висота повторилась: {висоти:?}");
    }

    #[test]
    fn найвища_якість_перша() {
        let варіанти = варіанти_з_json(&зразок());
        let висоти: Vec<u32> = варіанти.iter().filter_map(|x| x.height).collect();

        let mut спаданням = висоти.clone();
        спаданням.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(висоти, спаданням, "перелік має йти від найкращої якості");
    }

    #[test]
    fn є_варіант_лише_звук_і_він_останній() {
        let варіанти = варіанти_з_json(&зразок());
        let останній = варіанти.last().expect("перелік не порожній");

        assert_eq!(останній.id, ЛИШЕ_ЗВУК);
        assert!(останній.height.is_none(), "у звуку немає висоти");
    }

    /// ⚠️ У цьому зразку `filesize` є не в усіх форматів — і саме тому
    /// розмір рахується з бітрейта. Порожній розмір означає, що людина
    /// обирає якість наосліп.
    #[test]
    fn кожен_варіант_знає_свій_розмір() {
        let варіанти = варіанти_з_json(&зразок());
        let без_розміру: Vec<&str> = варіанти
            .iter()
            .filter(|x| x.size.is_none())
            .map(|x| x.label.as_str())
            .collect();

        assert!(без_розміру.is_empty(), "без розміру лишились: {без_розміру:?}");
    }

    #[test]
    fn розмір_відео_більший_за_звук() {
        let варіанти = варіанти_з_json(&зразок());
        let відео = варіанти
            .iter()
            .find(|x| x.height.is_some())
            .and_then(|x| x.size)
            .expect("є відео з розміром");
        let звук = варіанти
            .iter()
            .find(|x| x.id == ЛИШЕ_ЗВУК)
            .and_then(|x| x.size)
            .expect("є звук із розміром");

        assert!(відео > звук, "відео {відео} не більше за звук {звук}");
    }

    /// ⚠️ Найдорожча помилка цього модуля: сховати симптом разом із секретом.
    ///
    /// Бачили на живому прикладі — yt-dlp сказав «Requested format is not
    /// available», а людина прочитала «[приховано]». Повідомлення про
    /// помилку існує рівно для того, щоб його прочитали.
    #[test]
    fn текст_помилки_виживає_разом_із_секретом() {
        let рядок = "ERROR: [youtube] abc: Requested format is not available.                      Use --list-formats";
        assert_eq!(обрізати_секрети(рядок), рядок, "чистий текст не чіпаємо");

        let з_адресою = "ERROR: unable to download https://rr3.example.com/videoplayback?x=1 now";
        let вихід = обрізати_секрети(з_адресою);
        assert!(вихід.contains("unable to download"), "{вихід}");
        assert!(вихід.contains("now"), "текст після адреси теж потрібен: {вихід}");
        assert!(!вихід.contains("videoplayback"), "адреса мала зникнути: {вихід}");

        let з_токеном = "GET /api?token=s3cr3t failed with 403";
        let вихід = обрізати_секрети(з_токеном);
        assert!(вихід.contains("failed with 403"), "{вихід}");
        assert!(!вихід.contains("s3cr3t"), "значення токена мало зникнути: {вихід}");
    }

    #[test]
    fn селектор_не_вимагає_точної_висоти() {
        // Рівно цієї висоти може не бути саме зараз — тоді качати було б
        // нічого, тому «не вище», а не «рівно».
        assert!(селектор(Some("720"), true).contains("height<=720"));
        assert_eq!(селектор(Some(ЛИШЕ_ЗВУК), true), "ba/b");
        assert!(!селектор(None, true).contains("height"));

        // Без ffmpeg просимо один готовий файл, а не пару доріжок.
        assert!(!селектор(Some("720"), false).contains('+'));
        assert_eq!(селектор(None, false), "b");
    }

    #[tokio::test]
    async fn probe_без_бінарника_називає_path() {
        let p = YtdlpProtocol::with_bin("yt-dlp-немає-такого-бінарника-dl");
        let err = p
            .probe("https://www.youtube.com/watch?v=abc")
            .await
            .expect_err("має впасти");
        let msg = err.to_string();
        assert!(
            msg.contains("yt-dlp не знайдено в PATH"),
            "маємо: {msg}"
        );
    }

    #[tokio::test]
    async fn update_без_бінарника_називає_path() {
        let p = YtdlpProtocol::with_bin("yt-dlp-немає-такого-бінарника-dl");
        let err = p.self_update().await.expect_err("має впасти");
        assert!(
            err.to_string().contains("yt-dlp не знайдено в PATH"),
            "маємо: {err}"
        );
    }

    #[test]
    fn title_чиститься() {
        assert_eq!(імʼя_з_title("a/b:c*"), "a_b_c_");
        assert_eq!(імʼя_з_title("   "), "video");
    }

    #[test]
    fn секрети_з_stderr_ховаються() {
        let t = обрізати_секрети("ok\nhttp://evil/token=1\nfail");
        assert!(t.contains("ok"));
        assert!(t.contains("[приховано]"));
        assert!(!t.contains("evil"));
    }
}
