//! Що саме клієнти й ядро одне одному кажуть.
//!
//! Цей протокол — **єдина межа** між ядром і всім іншим. По ньому говорять
//! CLI, вікно і native messaging host; ним же спілкуватимуться зовнішні
//! плагіни-протоколи. Один контракт замість трьох — саме заради цього ядро
//! й винесено в окремий процес.
//!
//! # Модель подій
//!
//! Прогрес приходить **агрегованим снапшотом на тік**, а не окремою подією
//! на кожне завдання. П'ятдесят завдань по чотири рази на секунду — це
//! двісті повідомлень щосекунди, і будь-який UI на цьому захлинеться.
//! Снапшот усього списку — десяток кілобайтів, і надсилає його ядро тоді,
//! коли вважає за потрібне.
//!
//! Клієнт **ніколи не опитує** ядро в циклі: він підписується й чекає.

use serde::{Deserialize, Serialize};

/// Версія протоколу.
///
/// Зростає при несумісній зміні. Клієнт зі старшою версією отримує чесну
/// відмову з поясненням, а не загадкову поведінку через півгодини роботи.
pub const PROTOCOL_VERSION: u32 = 1;

/// Ім'я каналу, на якому слухає ядро.
#[cfg(windows)]
pub const PIPE_NAME: &str = r"\\.\pipe\downloader-core";

/// Шлях сокета на Unix-системах.
#[cfg(not(windows))]
pub const PIPE_NAME: &str = "/tmp/downloader-core.sock";

/// Запит від клієнта до ядра.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// Рукостискання. Має бути першим повідомленням.
    Hello {
        /// Хто прийшов: `cli`, `ui`, `nmhost`. Для журналу.
        client: String,
        protocol_version: u32,
    },

    /// Додати завантаження.
    Add {
        url: String,
        /// Куди зберегти. `None` — ядро вирішує саме за іменем із мережі.
        dest: Option<String>,
        /// Скільки з'єднань. `None` — за налаштуваннями ядра.
        parts: Option<usize>,
        /// Заголовок `Cookie`: `n=v; n2=v2`.
        ///
        /// `#[serde(default)]` — старі клієнти без цього поля не падають.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cookies: Option<String>,
        /// Заголовок `Referer`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        referer: Option<String>,
        /// Обраний варіант якості з `Response::Variants`.
        ///
        /// Рядок непрозорий: його видав модуль, йому ж він і повернеться.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        variant: Option<String>,
        /// Черга завантаження. `None` — типова черга 'default'.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queue: Option<String>,
    },

    /// Список завдань.
    List,

    /// Зупинити завдання.
    Pause { id: i64 },

    /// Продовжити зупинене.
    Resume { id: i64 },

    /// Прибрати завдання зі списку.
    Remove {
        id: i64,
        /// Чи видаляти вже завантажені байти з диска.
        with_file: bool,
    },

    /// Розкладка частин одного завдання.
    ///
    /// ⚠️ Запитується **лише для відкритого** завдання, а не для всього
    /// списку щотіку (Р-11). Це десятки чисел на завдання: у знімку вони
    /// роздули б кожне повідомлення в рази, а бачить їх людина тільки в
    /// одному рядку за раз.
    Details { id: i64 },

    /// Які варіанти якості має це посилання.
    ///
    /// Окремий запит, а не частина `Add`: спершу людина дивиться перелік,
    /// і лише потім вирішує. Проба коштує мережевого звернення, тож робити
    /// її на кожне додавання, коли вибір нікому не потрібен, не варто.
    Variants { url: String },

    /// Підписатись на потік подій.
    Subscribe,

    /// Чи живе ядро.
    Ping,

    /// Змінити правила ядра. Порожні поля — не чіпати.
    ///
    /// Для часу порожній рядок означає «прибрати вікно», а не «лишити як є».
    Configure {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_concurrent: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_limit: Option<u64>,
        /// `none` / `sleep` / `shutdown`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        post_action: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schedule_from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schedule_to: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quiet_from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quiet_to: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quiet_rate: Option<u64>,
    },

    /// Поточні правила ядра.
    Settings,

    /// Перелік черг.
    Queues,

    /// Створити нову чергу.
    QueueCreate {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_concurrent: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_limit: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        post_action: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schedule_from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schedule_to: Option<String>,
    },

    /// Змінити параметри існуючої черги.
    QueueConfigure {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_concurrent: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rate_limit: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        post_action: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schedule_from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schedule_to: Option<String>,
    },

    /// Зупинити чергу.
    QueuePause { name: String },

    /// Відновити чергу.
    QueueResume { name: String },

    /// Перейменувати чергу.
    QueueRename { old_name: String, new_name: String },

    /// Видалити чергу (її завдання повертаються в 'default').
    QueueDelete { name: String },

    /// Перенести завдання у вказану чергу.
    MoveToQueue { id: i64, queue: String },
}

/// Відповідь ядра на запит.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// Відповідь на рукостискання.
    Hello {
        /// Версія самого ядра, для журналу й діагностики.
        server_version: String,
        protocol_version: u32,
    },

    /// Завдання прийнято.
    Added { id: i64 },

    /// Список завдань.
    Tasks { tasks: Vec<TaskView> },

    /// Розкладка частин завдання.
    Details { id: i64, parts: Vec<PartView> },

    /// Перелік варіантів якості. Порожньо — вибирати нема з чого.
    Variants { variants: Vec<VariantView> },

    /// Поточні правила ядра.
    Settings {
        max_concurrent: u32,
        rate_limit: u64,
        #[serde(default)]
        post_action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schedule_from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schedule_to: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quiet_from: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quiet_to: Option<String>,
        #[serde(default)]
        quiet_rate: u64,
    },

    /// Перелік черг із лімітами та статистикою.
    Queues { queues: Vec<QueueView> },

    /// Зроблено.
    Ok,

    /// Не зроблено — і ось чому.
    ///
    /// Текст призначений людині: його покажуть у вікні або надрукують у
    /// консолі. Код потрібен програмі, щоб відрізнити «не знайдено» від
    /// «не на часі».
    Error { code: ErrorCode, message: String },
}

/// Причина відмови.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Клієнт говорить іншою версією протоколу.
    VersionMismatch,
    /// Першим повідомленням має бути `Hello`.
    HandshakeRequired,
    /// Такого завдання немає.
    NotFound,
    /// Запит правильний, але зараз недоречний.
    InvalidState,
    /// Щось пішло не так усередині ядра.
    Internal,
}

/// Подія від ядра до підписаних клієнтів.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// Повний знімок списку — основний спосіб оновлення UI.
    Snapshot { tasks: Vec<TaskView> },

    /// Завдання завершилось. Окремою подією, бо на неї реагують
    /// післядіями: відкрити теку, вимкнути комп'ютер.
    Finished {
        id: i64,
        path: String,
        bytes: u64,
    },

    /// Завдання впало.
    Failed { id: i64, message: String },
}

/// Інформація про чергу для показу клієнту (CLI/UI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueView {
    pub id: i64,
    pub name: String,
    pub max_concurrent: u32,
    pub rate_limit: u64,
    pub paused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_to: Option<String>,
    pub post_action: String,
    pub total_tasks: usize,
    pub running_tasks: usize,
}

/// Одна частина роботи очима клієнта — те, з чого малюється смужка сегментів.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariantView {
    /// Непрозорий ідентифікатор від модуля.
    pub id: String,
    /// Підпис для людини: «720p», «лише аудіо».
    pub label: String,
    /// Висота кадру, якщо це відео.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Розмір, якщо джерело його називає.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Кодек, бітрейт — дрібний уточнювальний рядок.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PartView {
    /// Початок у байтах.
    pub start: u64,
    /// Кінець, не включно.
    pub end: u64,
    /// Скільки від початку вже на диску.
    pub done: u64,
}

/// Завдання очима клієнта.
///
/// Навмисно пласка структура з готовими до показу полями: клієнт не має
/// нічого дораховувати. Швидкість і залишок часу рахує ядро — інакше два
/// різні клієнти покажуть різні числа для того самого завдання.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskView {
    pub id: i64,
    pub url: String,
    /// Ім'я файла для показу.
    pub name: String,
    /// Стан: `queued`, `running`, `paused`, `done`, `failed`.
    pub status: String,
    /// Скільки завантажено.
    pub done: u64,
    /// Скільки всього, якщо відомо.
    pub total: Option<u64>,
    /// Байтів за секунду, згладжено.
    pub speed: u64,
    /// Скільки секунд лишилось, якщо можна оцінити.
    pub eta_secs: Option<u64>,
    /// На скільки сегментів поділено зараз.
    pub segments: usize,
    /// Текст помилки, якщо завдання впало.
    pub error: Option<String>,
    /// Куди пишеться файл, якщо відомо.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest: Option<String>,
    /// Іменована черга, до якої належить завдання.
    #[serde(default = "default_queue_name")]
    pub queue: String,
}

fn default_queue_name() -> String {
    "default".to_owned()
}

impl TaskView {
    /// Частка виконаного від 0 до 1.
    ///
    /// `None`, коли розмір невідомий: показувати «0 %» при качанні на
    /// повному ходу — гірше, ніж не показувати нічого.
    #[must_use]
    pub fn progress(&self) -> Option<f64> {
        match self.total {
            Some(total) if total > 0 => Some((self.done as f64 / total as f64).min(1.0)),
            _ => None,
        }
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

    #[test]
    fn запит_переживає_серіалізацію() {
        let req = Request::Add {
            url: "https://e.com/звіт.pdf".to_owned(),
            dest: Some("D:/dl/звіт.pdf".to_owned()),
            parts: Some(8),
            cookies: None,
            referer: None,
            variant: None,
            queue: None,
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"kind\":\"add\""), "{json}");
        assert!(
            !json.contains("cookies"),
            "порожні cookies не мають потрапляти в JSON: {json}"
        );

        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::Add { url, parts, cookies, referer, .. } => {
                assert_eq!(url, "https://e.com/звіт.pdf");
                assert_eq!(parts, Some(8));
                assert!(cookies.is_none());
                assert!(referer.is_none());
            }
            other => panic!("розібралось не в те: {other:?}"),
        }
    }

    #[test]
    fn add_без_нових_полів_десеріалізується() {
        // Старий клієнт не знає cookies/referer — поле має мати serde default.
        let json = r#"{"kind":"add","url":"https://e.com/a.bin"}"#;
        let back: Request = serde_json::from_str(json).unwrap();
        match back {
            Request::Add {
                url,
                dest,
                parts,
                cookies,
                referer,
                variant,
                queue,
            } => {
                assert!(variant.is_none(), "старий клієнт варіанта не шле");
                assert_eq!(url, "https://e.com/a.bin");
                assert!(dest.is_none());
                assert!(parts.is_none());
                assert!(cookies.is_none());
                assert!(referer.is_none());
                assert!(queue.is_none());
            }
            other => panic!("розібралось не в те: {other:?}"),
        }
    }

    #[test]
    fn add_з_cookie_і_referer_переживає_серіалізацію() {
        let req = Request::Add {
            url: "https://e.com/a.bin".to_owned(),
            dest: None,
            parts: None,
            cookies: Some("n=v; n2=v2".to_owned()),
            referer: Some("https://e.com/page".to_owned()),
            variant: None,
            queue: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::Add {
                cookies, referer, ..
            } => {
                assert_eq!(cookies.as_deref(), Some("n=v; n2=v2"));
                assert_eq!(referer.as_deref(), Some("https://e.com/page"));
            }
            other => panic!("розібралось не в те: {other:?}"),
        }
    }

    #[test]
    fn configure_без_полів_десеріалізується() {
        let back: Request = serde_json::from_str(r#"{"kind":"configure"}"#).unwrap();
        match back {
            Request::Configure {
                max_concurrent,
                rate_limit,
                post_action,
                schedule_from,
                schedule_to,
                quiet_from,
                quiet_to,
                quiet_rate,
            } => {
                assert!(max_concurrent.is_none());
                assert!(rate_limit.is_none());
                assert!(post_action.is_none());
                assert!(schedule_from.is_none());
                assert!(schedule_to.is_none());
                assert!(quiet_from.is_none());
                assert!(quiet_to.is_none());
                assert!(quiet_rate.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn settings_без_нових_полів_десеріалізується() {
        let back: Response =
            serde_json::from_str(r#"{"kind":"settings","max_concurrent":3,"rate_limit":0}"#)
                .unwrap();
        match back {
            Response::Settings {
                max_concurrent,
                post_action,
                schedule_from,
                quiet_rate,
                ..
            } => {
                assert_eq!(max_concurrent, 3);
                assert!(post_action.is_empty());
                assert!(schedule_from.is_none());
                assert_eq!(quiet_rate, 0);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn невідома_команда_не_розбирається_мовчки() {
        let json = r#"{"kind":"самознищення"}"#;
        assert!(
            serde_json::from_str::<Request>(json).is_err(),
            "невідома команда мусить бути помилкою, а не тихим ігноруванням"
        );
    }

    #[test]
    fn помилка_несе_і_код_і_текст_для_людини() {
        let resp = Response::Error {
            code: ErrorCode::NotFound,
            message: "завдання 42 не існує".to_owned(),
        };

        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();

        match back {
            Response::Error { code, message } => {
                assert_eq!(code, ErrorCode::NotFound);
                assert!(message.contains("42"), "текст має називати завдання");
            }
            other => panic!("не та відповідь: {other:?}"),
        }
    }

    #[test]
    fn прогрес_невідомий_коли_розмір_невідомий() {
        let mut t = зразок();
        t.total = None;
        assert!(
            t.progress().is_none(),
            "«0 %» при качанні на повному ходу гірше, ніж нічого"
        );
    }

    #[test]
    fn прогрес_не_перевищує_одиниці() {
        let mut t = зразок();
        // Сервер міг збрехати про розмір — показувати 150 % не можна.
        t.done = 2000;
        t.total = Some(1000);
        assert_eq!(t.progress(), Some(1.0));
    }

    #[test]
    fn нульовий_розмір_не_дає_ділення_на_нуль() {
        let mut t = зразок();
        t.total = Some(0);
        assert!(t.progress().is_none());
    }

    fn зразок() -> TaskView {
        TaskView {
            id: 1,
            url: "https://e.com/f.bin".to_owned(),
            name: "f.bin".to_owned(),
            status: "running".to_owned(),
            done: 500,
            total: Some(1000),
            speed: 100,
            eta_secs: Some(5),
            segments: 4,
            error: None,
            dest: None,
            queue: "default".to_owned(),
        }
    }

    #[test]
    fn черги_запити_та_відповіді_серіалізуються() {
        let req = Request::QueueCreate {
            name: "night".to_owned(),
            max_concurrent: Some(4),
            rate_limit: Some(1024),
            post_action: Some("sleep".to_owned()),
            schedule_from: Some("22:00".to_owned()),
            schedule_to: Some("06:00".to_owned()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"kind\":\"queue_create\""));
        assert!(json.contains("\"name\":\"night\""));

        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::QueueCreate { name, max_concurrent, .. } => {
                assert_eq!(name, "night");
                assert_eq!(max_concurrent, Some(4));
            }
            other => panic!("{other:?}"),
        }

        let resp = Response::Queues {
            queues: vec![QueueView {
                id: 1,
                name: "default".to_owned(),
                max_concurrent: 3,
                rate_limit: 0,
                paused: false,
                schedule_from: None,
                schedule_to: None,
                post_action: "none".to_owned(),
                total_tasks: 5,
                running_tasks: 2,
            }],
        };
        let rjson = serde_json::to_string(&resp).unwrap();
        let rback: Response = serde_json::from_str(&rjson).unwrap();
        match rback {
            Response::Queues { queues } => {
                assert_eq!(queues.len(), 1);
                assert_eq!(queues[0].name, "default");
                assert_eq!(queues[0].running_tasks, 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn task_view_без_черги_отримує_default() {
        let json = r#"{
            "id": 1,
            "url": "https://e.com/test",
            "name": "test",
            "status": "queued",
            "done": 0,
            "speed": 0,
            "segments": 1
        }"#;
        let t: TaskView = serde_json::from_str(json).unwrap();
        assert_eq!(t.queue, "default");
    }
}
