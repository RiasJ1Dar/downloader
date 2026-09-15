//! Контракт, за яким ядро розмовляє з будь-яким способом завантаження.
//!
//! # Навіщо це потрібне
//!
//! Обіцянка проєкту звучить так: торент (або Site Grabber, або debrid-сервіс)
//! доточується **новим модулем**, а планувальник, черги, статистика й UI не
//! змінюються жодним рядком.
//!
//! Обіцянка виконується лише за однієї умови: **ядро не згадує жодного
//! протоколу на ім'я**. Щойно в планувальнику з'явиться `if url.ends_with(
//! ".m3u8")`, межа зламана — і зламана тихо, бо все продовжить працювати.
//! Тому на це є тест із протоколом-пустушкою, який нічого не качає: якщо
//! ядро вміє працювати з ним, воно не знає про HTTP.
//!
//! # Чому контракт саме такий
//!
//! Кожна з вимог нижче взята не з міркувань краси, а з конкретного випадку,
//! на якому спрощений контракт розвалився б:
//!
//! * **Одне завдання — набір файлів.** Торент це набір за визначенням; DASH
//!   дає окремо відео, аудіо й субтитри. Модель «завдання = файл» довелося б
//!   ламати з міграцією бази на дисках у людей.
//! * **Розмір наперед невідомий.** HLS live не має кінця, поки ефір триває;
//!   у торента розмір залежить від того, які файли обрали.
//! * **Стан відновлення непрозорий.** У HTTP це `(offset, len)`, у торента —
//!   бітова карта частин плюс сесія. Ядро зберігає [`ResumeBlob`], не
//!   намагаючись його тлумачити.
//! * **Ліміт швидкості з чесною відмовою.** Зовнішній модуль може не вміти
//!   себе дроселювати. Тоді він так і каже — і «нічний профіль» чесно
//!   попереджає, що на це завдання не діє, замість тихо його не застосувати.
//! * **Байти не течуть через контракт.** Модуль пише у файл сам, ядро дає
//!   шлях. Інакше зовнішній плагін гнав би гігабайти через IPC.

use std::path::PathBuf;

use crate::error::Result;

/// Непрозорий для ядра стан відновлення.
///
/// HTTP кладе сюди таблицю сегментів, торент — бітову карту частин. Ядро
/// зберігає це поруч із завданням і повертає модулю при наступному запуску,
/// не заглядаючи всередину.
pub type ResumeBlob = Vec<u8>;

/// Cookies і Referer для одного завдання.
///
/// Формат cookies — як заголовок `Cookie`: `n=v; n2=v2`. Це поле на
/// завданні, не «магія» HTTP-клієнта: інакше HLS із браузера знову буде 403.
///
/// ⚠️ [`Debug`] **не** друкує значень — лише «задано». Інакше журнал і
/// `?ctx` світили б сесію.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Session {
    /// Заголовок `Cookie`, якщо є.
    pub cookies: Option<String>,
    /// Заголовок `Referer`, якщо є.
    pub referer: Option<String>,
}

impl Session {
    /// Зібрати сесію. Порожній або пробільний рядок = відсутність.
    #[must_use]
    pub fn from_parts(cookies: Option<String>, referer: Option<String>) -> Self {
        Self {
            cookies: nonempty(cookies),
            referer: nonempty(referer),
        }
    }

    /// Чи немає ні cookies, ні referer.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cookies.is_none() && self.referer.is_none()
    }
}

fn nonempty(v: Option<String>) -> Option<String> {
    v.and_then(|s| {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_owned())
        }
    })
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("cookies", &self.cookies.as_ref().map(|_| "задано"))
            .field("referer", &self.referer.as_ref().map(|_| "задано"))
            .finish()
    }
}

/// Що модуль дізнався про посилання.
#[derive(Debug, Clone, Default)]
pub struct Probed {
    /// Остаточна адреса після редиректів.
    pub final_url: String,
    /// Сумарний розмір, якщо відомий.
    pub total_size: Option<u64>,
    /// Чи можна продовжити з місця обриву.
    pub resumable: bool,
    /// Ознака версії ресурсу очима модуля (`ETag`, хеш торента тощо).
    pub fingerprint: Option<String>,
    /// Файли, які дасть це завдання.
    pub files: Vec<PlannedFile>,
    /// Варіанти якості того самого вмісту.
    ///
    /// Порожньо — вибирати нема з чого: звичайний файл має один вигляд.
    pub variants: Vec<Variant>,
}

/// Один варіант якості.
///
/// ⚠️ Це **не** те саме, що [`PlannedFile`]. Файли — набір, де кожен пункт
/// незалежний: із торента можна зняти галочки з половини роздачі. Варіанти
/// взаємовиключні: одне й те саме відео у 720p **або** у 360p, третього не
/// дано.
///
/// # Ядро не розуміє `id`
///
/// Для ядра це непрозорий рядок: узяло в модуля, віддало людині, повернуло
/// модулю. Що за ним стоїть — `format_id` yt-dlp, URI плейлиста HLS чи
/// `Representation@id` у DASH — знає лише той модуль, який його видав.
/// Варто ядру раз заглянути всередину — і обіцянка «новий модуль
/// доточується без правок ядра» стане неправдою.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    /// Ідентифікатор, зрозумілий модулю, який його видав.
    pub id: String,
    /// Підпис для людини: «720p», «лише аудіо».
    pub label: String,
    /// Висота кадру, якщо це відео. Потрібна, щоб упорядкувати перелік.
    pub height: Option<u32>,
    /// Розмір, якщо джерело його називає. Часто невідомий наперед.
    pub size: Option<u64>,
    /// Дрібний уточнювальний рядок: кодек, бітрейт.
    pub note: Option<String>,
}

/// Файл, який завдання створить на диску.
#[derive(Debug, Clone)]
pub struct PlannedFile {
    /// Ім'я, як його бачить джерело. Санітизацію робить ядро.
    pub suggested_name: String,
    /// Розмір, якщо відомий.
    pub size: Option<u64>,
    /// Чи качати його за замовчуванням.
    ///
    /// Для торента людина може зняти галочки з половини роздачі; для DASH
    /// субтитри можуть бути не потрібні.
    pub selected: bool,
}

/// Прапорець зупинки.
///
/// Ядро ставить його, коли людина натиснула «пауза» або «скасувати»; модуль
/// перевіряє між шматками роботи й чемно згортається.
///
/// # Чому атомарний прапорець, а не канал
///
/// Воркер перевіряє зупинку **між кожним записом** — тобто десятки разів на
/// секунду на кожне з'єднання. Канал або м'ютекс тут коштували б більше, ніж
/// сама перевірка; атомарне читання — це одна інструкція.
///
/// Так само зроблено в DLMan, і це одне з небагатьох їхніх рішень, яке варто
/// було перейняти без змін.
#[derive(Debug, Clone, Default)]
pub struct Cancel(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Cancel {
    /// Новий прапорець, ще не піднятий.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Попросити зупинитись.
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }

    /// Чи просили зупинитись.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Куди модулю писати й що ядро вже знає.
#[derive(Debug, Clone)]
pub struct RunContext {
    /// Ідентифікатор завдання — для подій і журналу.
    pub task_id: i64,
    /// Що качаємо.
    pub source: String,
    /// Готові шляхи, у тому самому порядку, що й [`Probed::files`].
    ///
    /// Ядро вже зробило імена безпечними, розвело збіги й створило теки.
    /// Модуль просто пише у ці шляхи.
    pub targets: Vec<PathBuf>,
    /// Стан із попереднього запуску, якщо він був і підійшов.
    pub resume: Option<ResumeBlob>,
    /// Прапорець зупинки.
    ///
    /// ⚠️ Модуль зобов'язаний його перевіряти. Інакше «пауза» в UI стане
    /// кнопкою, яка нічого не робить, — а це гірше за її відсутність.
    pub cancel: Cancel,
    /// Cookies і Referer цього завдання.
    ///
    /// `probe` їх не бачить (немає контексту) — ядро кличе
    /// [`Protocol::set_session`] **перед** пробою і перед `run`. У `run`
    /// модуль бере сесію звідси, щоб паралельні завдання не ділили Mutex.
    pub session: Session,
    /// Обраний варіант якості — той самий рядок, який модуль дав у пробі.
    ///
    /// `None` — людина не вибирала, модуль вирішує сам.
    pub variant: Option<String>,
    /// Обмежувач швидкості для черги або завдання.
    ///
    /// Якщо задано, модуль використовує його замість власного внутрішнього лімітера.
    pub limiter: Option<std::sync::Arc<crate::rate::RateLimiter>>,
}

/// Що модуль повідомляє ядру під час роботи.
///
/// Навмисно вузько: жодних байтів, лише числа й факти. Усе, що тут можна
/// сказати, однаково осмислене і для HTTP, і для торента.
#[derive(Debug, Clone)]
pub enum Progress {
    /// Стало відомо більше про розмір — наприклад, після першої відповіді.
    TotalKnown { total: u64 },
    /// Просунулись на стільки байтів **сумарно** по всьому завданню.
    Advanced { done: u64 },
    /// Змінилась кількість частин, на які поділено роботу.
    Segments { count: usize },
    /// Розкладка роботи по частинах — те, що малює «вікно сегментів».
    ///
    /// Шлеться **рідше** за [`Progress::Advanced`]: це десятки чисел, а не
    /// одне, і оновлювати їх щочверть секунди для всіх завдань немає сенсу.
    /// Модуль може не слати цього взагалі — тоді вікно покаже лише
    /// кількість частин.
    Layout { parts: Vec<PartProgress> },
    /// Модуль просить зберегти свій стан відновлення.
    ///
    /// ⚠️ Ядро зобов'язане записати його **після** того, як дані вже на
    /// диску. Стан, що випереджає байти, дає докачування з дірки.
    Checkpoint { resume: ResumeBlob },
}

/// Одна частина роботи: звідки, доки й скільки вже зроблено.
///
/// Для HTTP це сегмент файла, для торента — діапазон частин. Ядро не
/// тлумачить ці числа, лише передає далі: вікно малює з них смужку.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartProgress {
    /// Початок у байтах від початку файла.
    pub start: u64,
    /// Кінець, не включно.
    pub end: u64,
    /// Скільки байтів від `start` уже на диску.
    pub done: u64,
}

impl PartProgress {
    /// Довжина частини.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.end - self.start
    }

    /// Чи частина порожня.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// Чи вдалося застосувати ліміт швидкості.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitSupport {
    /// Модуль дроселює себе сам.
    Applied,
    /// Не вміє. Ядро має сказати про це людині, а не вдавати, що ліміт діє.
    Unsupported,
}

/// Куди модуль шле прогрес.
pub trait ProgressSink: Send + Sync {
    /// Повідомити про поступ.
    ///
    /// Викликається часто — реалізація мусить бути дешевою і не блокувати.
    fn report(&self, progress: Progress);
}

/// Спосіб завантаження: HTTP, HLS, торент, зовнішній плагін.
///
/// Реалізації живуть в окремих крейтах. Ядро знає лише цей контракт.
#[async_trait::async_trait]
pub trait Protocol: Send + Sync {
    /// Коротке ім'я для журналу й файла стану: `http`, `torrent`.
    ///
    /// Ядро використовує його лише як мітку — звіряє, що стан писав той
    /// самий модуль, який тепер його читає.
    fn name(&self) -> &'static str;

    /// Чи це посилання для цього модуля.
    ///
    /// Саме тут, а не в ядрі, живе знання «magnet — це торент».
    fn handles(&self, source: &str) -> bool;

    /// Дізнатись усе можливе, нічого не завантажуючи.
    async fn probe(&self, source: &str) -> Result<Probed>;

    /// Те саме, але для вже обраного варіанта якості.
    ///
    /// Розмір і склад файлів залежать від вибору: 720p важить не стільки,
    /// скільки 360p, а «лише аудіо» дає інше розширення. Ядро кличе цей
    /// метод, коли людина вже щось обрала.
    ///
    /// Типова реалізація ігнорує вибір — модулям без варіантів (звичайний
    /// HTTP) перевизначати нічого.
    async fn probe_variant(&self, source: &str, variant: Option<&str>) -> Result<Probed> {
        let _ = variant;
        self.probe(source).await
    }

    /// Завантажити. Модуль пише у файли сам.
    ///
    /// Повертає стан відновлення, якщо роботу перервано ([`RunContext::cancel`]),
    /// або `None`, коли завдання завершене повністю.
    ///
    /// ⚠️ Зупинка на прохання — **не помилка**. Модуль має повернути `Ok`
    /// зі станом, а не `Err`: інакше в списку з'явиться червоне завдання
    /// там, де людина просто натиснула паузу.
    async fn run(&self, ctx: RunContext, sink: &dyn ProgressSink) -> Result<Option<ResumeBlob>>;

    /// Обмежити швидкість цього завдання.
    ///
    /// За замовчуванням — чесне «не вмію»: краще, ніж мовчки проігнорувати.
    fn set_rate_limit(&self, _bytes_per_sec: u64) -> RateLimitSupport {
        RateLimitSupport::Unsupported
    }

    /// Сесія (cookies, referer) для наступних `probe` / `run`.
    ///
    /// Не async: реалізація кладе значення в `Mutex`. Типово порожньо —
    /// не кожен протокол ходить по HTTP (торент, зовнішній exe).
    fn set_session(&self, _session: Session) {}

    /// Перевірити результат після завершення.
    ///
    /// Місце для звірки хешів і довжин. За замовчуванням — нічого, бо
    /// більшість модулів перевіряють себе по ходу.
    async fn verify(&self, _ctx: &RunContext) -> Result<()> {
        Ok(())
    }
}

/// Реєстр доступних способів завантаження.
///
/// ⚠️ Заповнюється **ззовні** — ядром-сервісом при старті. Сам крейт ядра не
/// знає жодного модуля й не може його створити: саме це й тримає межу.
#[derive(Default)]
pub struct Registry {
    protocols: Vec<Box<dyn Protocol>>,
}

impl Registry {
    /// Порожній реєстр.
    #[must_use]
    pub fn new() -> Self {
        Self {
            protocols: Vec::new(),
        }
    }

    /// Додати модуль.
    ///
    /// Порядок має значення: перший, хто впізнав посилання, його й бере.
    /// Тому спеціалізовані модулі (торент, HLS) реєструються **перед**
    /// загальними (HTTP), інакше HTTP забирав би собі все, що починається
    /// з `https://`.
    pub fn register(&mut self, protocol: Box<dyn Protocol>) {
        self.protocols.push(protocol);
    }

    /// Знайти модуль для посилання.
    #[must_use]
    pub fn find(&self, source: &str) -> Option<&dyn Protocol> {
        self.protocols
            .iter()
            .find(|p| p.handles(source))
            .map(AsRef::as_ref)
    }

    /// Модуль за іменем — потрібно, щоб продовжити збережене завдання саме
    /// тим модулем, який його починав.
    #[must_use]
    pub fn by_name(&self, name: &str) -> Option<&dyn Protocol> {
        self.protocols
            .iter()
            .find(|p| p.name() == name)
            .map(AsRef::as_ref)
    }

    /// Скільки модулів зареєстровано.
    #[must_use]
    pub fn len(&self) -> usize {
        self.protocols.len()
    }

    /// Чи реєстр порожній.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.protocols.is_empty()
    }

    /// Імена всіх модулів — для журналу й діагностики.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.protocols.iter().map(|p| p.name()).collect()
    }

    /// Оновити ліміт швидкості для всіх модулів у реєстрі.
    pub fn set_rate_limit(&self, bytes_per_sec: u64) {
        for p in &self.protocols {
            p.set_rate_limit(bytes_per_sec);
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Протокол-пустушка: нічого не качає, лише вдає.
    ///
    /// ⚠️ **Це головний запобіжник межі.** Ядро вміє працювати з модулем,
    /// який не має жодного стосунку до HTTP, — отже, воно й справді не знає,
    /// що таке HTTP. Якщо колись у ядрі з'явиться `if url.starts_with("http")`
    /// або щось подібне, ці тести впадуть першими.
    struct Пустушка {
        name: &'static str,
        схема: &'static str,
        файлів: usize,
        обмеження: Mutex<Option<u64>>,
    }

    impl Пустушка {
        fn нова(name: &'static str, схема: &'static str, файлів: usize) -> Self {
            Self {
                name,
                схема,
                файлів,
                обмеження: Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl Protocol for Пустушка {
        fn name(&self) -> &'static str {
            self.name
        }

        fn handles(&self, source: &str) -> bool {
            source.starts_with(self.схема)
        }

        async fn probe(&self, source: &str) -> Result<Probed> {
            Ok(Probed {
                final_url: source.to_owned(),
                total_size: Some(1000),
                resumable: true,
                fingerprint: Some("пустушка-1".to_owned()),
                files: (0..self.файлів)
                    .map(|i| PlannedFile {
                        suggested_name: format!("файл-{i}.bin"),
                        size: Some(1000 / self.файлів as u64),
                        selected: i == 0,
                    })
                    .collect(),
                variants: Vec::new(),
            })
        }

        async fn run(
            &self,
            ctx: RunContext,
            sink: &dyn ProgressSink,
        ) -> Result<Option<ResumeBlob>> {
            sink.report(Progress::TotalKnown { total: 1000 });
            sink.report(Progress::Segments { count: 4 });
            sink.report(Progress::Advanced { done: 1000 });
            sink.report(Progress::Checkpoint {
                resume: format!("стан-{}", ctx.task_id).into_bytes(),
            });
            Ok(None)
        }

        fn set_rate_limit(&self, bytes_per_sec: u64) -> RateLimitSupport {
            if let Ok(mut g) = self.обмеження.lock() {
                *g = Some(bytes_per_sec);
                RateLimitSupport::Applied
            } else {
                RateLimitSupport::Unsupported
            }
        }
    }

    /// Збирач подій прогресу.
    #[derive(Default)]
    struct Збирач {
        події: Mutex<Vec<String>>,
    }

    impl ProgressSink for Збирач {
        fn report(&self, progress: Progress) {
            if let Ok(mut g) = self.події.lock() {
                g.push(match progress {
                    Progress::TotalKnown { total } => format!("total={total}"),
                    Progress::Advanced { done } => format!("done={done}"),
                    Progress::Segments { count } => format!("segments={count}"),
                    Progress::Layout { parts } => format!("layout={}", parts.len()),
                    Progress::Checkpoint { resume } => {
                        format!("checkpoint={}", String::from_utf8_lossy(&resume))
                    }
                });
            }
        }
    }

    #[test]
    fn порожній_реєстр_нікого_не_знаходить() {
        let r = Registry::new();
        assert!(r.is_empty());
        assert!(r.find("будь-що://адреса").is_none());
    }

    #[test]
    fn модуль_обирається_за_власним_рішенням_а_не_ядром() {
        let mut r = Registry::new();
        r.register(Box::new(Пустушка::нова("вигадка", "вигадка://", 1)));
        r.register(Box::new(Пустушка::нова("інша", "інша://", 1)));

        assert_eq!(r.find("вигадка://щось").unwrap().name(), "вигадка");
        assert_eq!(r.find("інша://щось").unwrap().name(), "інша");
        assert!(
            r.find("невідома://щось").is_none(),
            "ядро не має вгадувати за нікого"
        );
    }

    #[test]
    fn перший_зареєстрований_має_перевагу() {
        // Спеціалізовані модулі реєструються раніше за загальні: інакше
        // загальний забирав би все, що зовні схоже на його схему.
        let mut r = Registry::new();
        r.register(Box::new(Пустушка::нова("спеціальний", "тест://особливе", 1)));
        r.register(Box::new(Пустушка::нова("загальний", "тест://", 1)));

        assert_eq!(
            r.find("тест://особливе/файл").unwrap().name(),
            "спеціальний",
            "загальний модуль перехопив те, що належить спеціальному"
        );
    }

    #[test]
    fn модуль_знаходиться_за_імʼям_для_продовження_завдання() {
        let mut r = Registry::new();
        r.register(Box::new(Пустушка::нова("вигадка", "вигадка://", 1)));

        assert!(r.by_name("вигадка").is_some());
        assert!(
            r.by_name("торент").is_none(),
            "неіснуючий модуль не має підмінятись наявним"
        );
    }

    #[tokio::test]
    async fn проба_дає_набір_файлів_а_не_один() {
        // Заради цього контракт і зроблено таким: торент і DASH дають кілька
        // файлів на одне завдання.
        let p = Пустушка::нова("набір", "набір://", 3);
        let probed = p.probe("набір://роздача").await.unwrap();

        assert_eq!(probed.files.len(), 3);
        assert!(probed.files[0].selected);
        assert!(
            !probed.files[1].selected,
            "невибрані файли мають лишатись невибраними"
        );
    }

    #[tokio::test]
    async fn ядро_отримує_прогрес_не_знаючи_що_саме_качається() {
        let p = Пустушка::нова("вигадка", "вигадка://", 1);
        let збирач = Збирач::default();

        let resume = p
            .run(
                RunContext {
                    task_id: 42,
                    source: "вигадка://щось".to_owned(),
                    targets: vec![PathBuf::from("файл.bin")],
                    resume: None,
                    cancel: Cancel::new(),
                    session: Session::default(),
                    variant: None,
                    limiter: None,
                },
                &збирач,
            )
            .await
            .unwrap();

        assert!(resume.is_none(), "завершене завдання не лишає стану");

        let події = збирач.події.lock().unwrap().clone();
        assert_eq!(
            події,
            vec![
                "total=1000",
                "segments=4",
                "done=1000",
                "checkpoint=стан-42",
            ]
        );
    }

    #[tokio::test]
    async fn стан_відновлення_непрозорий_для_ядра() {
        let p = Пустушка::нова("вигадка", "вигадка://", 1);
        let збирач = Збирач::default();

        // Ядро передає назад те, що колись зберегло, не тлумачачи вміст.
        let чужий_стан: ResumeBlob = vec![0xDE, 0xAD, 0xBE, 0xEF];

        let результат = p
            .run(
                RunContext {
                    task_id: 1,
                    source: "вигадка://щось".to_owned(),
                    targets: vec![PathBuf::from("файл.bin")],
                    resume: Some(чужий_стан),
                    cancel: Cancel::new(),
                    session: Session::default(),
                    variant: None,
                    limiter: None,
                },
                &збирач,
            )
            .await;

        assert!(результат.is_ok(), "модуль сам вирішує, що робити зі станом");
    }

    #[test]
    fn модуль_може_чесно_відмовити_в_ліміті_швидкості() {
        struct БезЛіміту;

        #[async_trait::async_trait]
        impl Protocol for БезЛіміту {
            fn name(&self) -> &'static str {
                "без-ліміту"
            }
            fn handles(&self, _: &str) -> bool {
                true
            }
            async fn probe(&self, _: &str) -> Result<Probed> {
                Ok(Probed::default())
            }
            async fn run(&self, _: RunContext, _: &dyn ProgressSink) -> Result<Option<ResumeBlob>> {
                Ok(None)
            }
        }

        // Типова реалізація — «не вмію». Це краще за мовчазне ігнорування:
        // людина побачить, що нічний профіль на це завдання не діє.
        assert_eq!(
            БезЛіміту.set_rate_limit(1000),
            RateLimitSupport::Unsupported
        );

        let вміє = Пустушка::нова("вигадка", "вигадка://", 1);
        assert_eq!(вміє.set_rate_limit(5000), RateLimitSupport::Applied);
        assert_eq!(*вміє.обмеження.lock().unwrap(), Some(5000));
    }

    #[test]
    fn debug_сесії_не_світить_cookie() {
        let s = Session::from_parts(
            Some("session=secret-token".to_owned()),
            Some("https://example.test/page".to_owned()),
        );
        let text = format!("{s:?}");
        assert!(
            !text.contains("secret-token"),
            "значення cookie потрапило в Debug: {text}"
        );
        assert!(
            !text.contains("example.test"),
            "значення referer потрапило в Debug: {text}"
        );
        assert!(text.contains("задано"), "{text}");
    }

    #[test]
    fn порожній_рядок_це_відсутня_сесія() {
        let s = Session::from_parts(Some("  ".to_owned()), Some(String::new()));
        assert!(s.is_empty());
        assert!(s.cookies.is_none());
        assert!(s.referer.is_none());
    }

    #[test]
    fn set_session_за_замовчуванням_нічого_не_ламає() {
        let p = Пустушка::нова("x", "x://", 1);
        p.set_session(Session::from_parts(Some("a=b".to_owned()), None));
    }

    #[test]
    fn реєстр_перелічує_модулі_для_журналу() {
        let mut r = Registry::new();
        r.register(Box::new(Пустушка::нова("перший", "a://", 1)));
        r.register(Box::new(Пустушка::нова("другий", "b://", 1)));

        assert_eq!(r.names(), vec!["перший", "другий"]);
        assert_eq!(r.len(), 2);
    }
}
