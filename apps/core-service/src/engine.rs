//! Ядро: список завдань, качання, події.
//!
//! # Ядро не знає, що таке HTTP
//!
//! Тут немає жодної згадки про `proto-http` — лише [`Registry`] і контракт
//! [`Protocol`]. Модулі реєструються **ззовні**, у `main.rs`; ядро питає
//! реєстр «хто візьме це посилання» і далі говорить із переможцем через
//! контракт.
//!
//! Так виглядає обіцянка «торент доточиться новим модулем» у робочому
//! вигляді. Варто раз викликати рушій навпростець — і обіцянка стає
//! неправдою, причому тихо: усе працюватиме й далі.
//!
//! # Модель подій
//!
//! Прогрес не розсилається на кожен записаний байт. Раз на тік ядро складає
//! **один знімок усього списку** й віддає його всім підписаним. П'ятдесят
//! завдань по чотири рази на секунду дали б двісті повідомлень щосекунди —
//! на цьому захлинається будь-який UI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use downloader_core::protocol::{
    Cancel, PlannedFile, Progress, ProgressSink, RateLimitSupport, Registry, RunContext, Session,
};
use downloader_core::store::{NewFile, NewTask, Status, Store};
use downloader_core::{Settings, SettingsPatch};

use crate::after::AfterQueue;
use downloader_ipc::protocol::{Event, PartView, TaskView, VariantView};
use downloader_winutil::{motw, names, paths, вистачить_місця};
use tokio::sync::broadcast;

/// Як часто розсилати знімок списку.
const TICK: std::time::Duration = std::time::Duration::from_millis(250);

/// Скільки подій тримати в буфері для повільного клієнта.
///
/// Клієнт, який не встигає, втратить найстаріші знімки — і це правильно:
/// знімок самодостатній, тож відставання лікується наступним, а не чергою,
/// що росте без меж.
const EVENT_BUFFER: usize = 64;

/// Живе завдання в пам'яті ядра.
#[derive(Debug, Clone)]
struct Live {
    id: i64,
    url: String,
    name: String,
    status: Status,
    done: u64,
    total: Option<u64>,
    segments: usize,
    error: Option<String>,
    /// Байтів на попередньому тіку — для обчислення швидкості.
    prev_done: u64,
    speed: u64,
    /// Cookies / Referer цього завдання. Лише в пам'яті: пауза/продовження
    /// в тому самому процесі їх зберігає; після рестарту ядра — порожньо.
    session: Session,
    protocol: String,
    targets: Vec<PathBuf>,
    /// Обраний варіант якості, як його назвав модуль.
    ///
    /// Живе поруч із завданням, бо має пережити паузу: після «продовжити»
    /// качати треба ту саму якість, а не ту, яку модуль вибере наново.
    variant: Option<String>,
    /// Розкладка частин — для «вікна сегментів».
    ///
    /// Живе лише в пам'яті й лише для активних завдань: після завершення
    /// вона нікому не потрібна, а в базі роздувала б рядок.
    parts: Vec<downloader_core::protocol::PartProgress>,
    /// Бажана паралельність саме цього завдання.
    ///
    /// Живе лише в пам'яті, як `session`, `max_duration` і `rewind`: після
    /// рестарту повертається до типової настройки модуля.
    parallelism: Option<usize>,
    /// Іменована черга.
    queue: String,
    /// Обмеження тривалості (для live-потоків).
    max_duration: Option<std::time::Duration>,
    /// Вимагати запис від початку live-буфера.
    rewind: bool,
}

impl Live {
    fn to_view(&self) -> TaskView {
        TaskView {
            id: self.id,
            url: self.url.clone(),
            name: self.name.clone(),
            status: self.status.as_str().to_owned(),
            done: self.done,
            total: self.total,
            speed: self.speed,
            eta_secs: self.eta(),
            segments: self.segments,
            error: self.error.clone(),
            dest: self.targets.first().map(|p| p.display().to_string()),
            queue: self.queue.clone(),
        }
    }

    /// Скільки секунд лишилось.
    ///
    /// `None`, коли рахувати нема з чого: невідомий розмір або нульова
    /// швидкість. «Залишилось 0 с» на завданні, яке стоїть, гірше, ніж
    /// нічого.
    fn eta(&self) -> Option<u64> {
        let total = self.total?;
        if self.speed == 0 || self.done >= total {
            return None;
        }
        Some((total - self.done) / self.speed.max(1))
    }
}

/// Приймач прогресу від модуля.
///
/// Модуль знає про байти, ядро — про список і клієнтів. Зустрічаються вони
/// тут, і ця зустріч — єдине місце, де вони одне про одного знають.
struct LiveSink {
    id: i64,
    live: Arc<Mutex<HashMap<i64, Live>>>,
}

impl ProgressSink for LiveSink {
    fn report(&self, progress: Progress) {
        let Ok(mut live) = self.live.lock() else {
            tracing::error!("список завдань отруєно, прогрес утрачено");
            return;
        };

        let Some(task) = live.get_mut(&self.id) else {
            // Завдання прибрали, поки модуль качав — нормальна ситуація.
            return;
        };

        match progress {
            Progress::TotalKnown { total } => task.total = Some(total),
            Progress::Advanced { done } => task.done = done,
            Progress::Segments { count } => task.segments = count,
            Progress::Layout { parts } => task.parts = parts,
            // Стан відновлення зберігає сам модуль поруч із файлом. Ядро
            // тримає його лише для протоколів, які не мають власного
            // sidecar — наразі таких немає.
            Progress::Checkpoint { .. } => {}
        }
    }
}

/// Ядро завантажень.
pub struct Engine {
    store: Mutex<Store>,
    live: Arc<Mutex<HashMap<i64, Live>>>,
    events: broadcast::Sender<Event>,
    /// Доступні способи завантаження. Заповнюється ззовні.
    registry: Registry,
    /// Прапорці зупинки активних завдань.
    ///
    /// Живуть окремо від `live`, бо мають переживати паузу: завдання вже не
    /// качається, але його прапорець ще потрібен, щоб відрізнити «зупинено
    /// людиною» від «упало».
    cancels: Mutex<HashMap<i64, Cancel>>,
    /// Тека за замовчуванням, коли клієнт не сказав, куди класти.
    downloads_dir: PathBuf,
    /// Правила ядра. Персистяться в SQLite; атомні дзеркала не потрібні —
    /// читаємо під час старту завдання, не на кожному байті.
    налаштування: Mutex<Settings>,
    /// Післядія порожньої черги — окремий модуль, не частина качання.
    after: Arc<AfterQueue>,
    /// Чи розклад дозволяв старт на попередньому тіку.
    вікно_було: std::sync::atomic::AtomicBool,
    /// Поточний активний ліміт швидкості для динамічного оновлення модулів на льоту.
    поточний_ліміт: std::sync::atomic::AtomicU64,
    /// Обмежувачі швидкості для окремих іменованих черг.
    queue_limiters: Mutex<HashMap<String, Arc<downloader_core::rate::RateLimiter>>>,
}

impl Engine {
    /// Підняти ядро на заданій базі з готовим реєстром модулів.
    pub fn new(
        db: &std::path::Path,
        downloads_dir: PathBuf,
        registry: Registry,
    ) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(
            !registry.is_empty(),
            "ядро запущене без жодного модуля завантаження — качати нічим"
        );

        let mut store = Store::open(db)?;
        store.seed_default_categories(&downloads_dir)?;
        let (events, _) = broadcast::channel(EVENT_BUFFER);

        tracing::info!(модулі = ?registry.names(), "реєстр протоколів");

        let mut stored = store.settings()?;
        if let Some(n) = std::env::var("DOWNLOADER_MAX_CONCURRENT")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n: &u32| n >= 1)
        {
            stored.max_concurrent = n;
        }
        if let Some(r) = std::env::var("DOWNLOADER_RATE_LIMIT")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            stored.rate_limit = r;
        }

        let вікно = stored.downloads_allowed(хвилини_зараз());
        let початковий_ліміт = stored.effective_rate(хвилини_зараз());
        registry.set_rate_limit(початковий_ліміт);

        let engine = Arc::new(Self {
            store: Mutex::new(store),
            live: Arc::new(Mutex::new(HashMap::new())),
            events,
            registry,
            cancels: Mutex::new(HashMap::new()),
            downloads_dir,
            налаштування: Mutex::new(stored),
            after: Arc::new(AfterQueue::default()),
            вікно_було: std::sync::atomic::AtomicBool::new(вікно),
            поточний_ліміт: std::sync::atomic::AtomicU64::new(початковий_ліміт),
            queue_limiters: Mutex::new(HashMap::new()),
        });

        engine.відновити_з_бази()?;
        engine.clone().start_ticker();
        engine.clone().спробувати_наступне();
        Ok(engine)
    }

    /// Підняти список із бази після рестарту процесу ядра.
    ///
    /// `running` у момент падіння стає `queued`: sidecar поруч із файлом
    /// докачає, а два планувальники на один файл не з'являться.
    fn відновити_з_бази(self: &Arc<Self>) -> anyhow::Result<()> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
        let tasks = store.tasks()?;
        let mut loaded = Vec::new();
        for t in tasks {
            let files = store.files(t.id)?;
            let targets: Vec<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
            let done: u64 = files.iter().map(|f| f.done).sum();
            let total: Option<u64> = {
                let sizes: Vec<u64> = files.iter().filter_map(|f| f.size).collect();
                if sizes.len() == files.len() && !sizes.is_empty() {
                    Some(sizes.iter().sum())
                } else {
                    None
                }
            };
            let done =
                targets
                    .first()
                    .map_or(done, |path| match downloader_core::state::load(path) {
                        Ok(state) => state.downloaded(),
                        Err(downloader_core::state::LoadError::Absent) => done,
                        Err(error) => {
                            tracing::warn!(
                                task_id = t.id,
                                path = %path.display(),
                                %error,
                                "стан докачування не прочитано; використано прогрес із бази"
                            );
                            done
                        }
                    });
            let mut status = t.status;
            if status == Status::Running {
                store.set_status(t.id, Status::Queued, None)?;
                status = Status::Queued;
            }
            let name = t.title.clone().unwrap_or_else(|| {
                targets
                    .first()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| names::ЗАПАСНЕ_ІМʼЯ.to_owned())
            });
            loaded.push(Live {
                id: t.id,
                variant: t.variant.clone(),
                url: t.url,
                name,
                status,
                done,
                total,
                segments: 0,
                error: t.error,
                prev_done: done,
                speed: 0,
                session: Session::default(),
                protocol: t.protocol,
                targets,
                parts: Vec::new(),
                parallelism: None,
                queue: t.queue,
                max_duration: None,
                rewind: false,
            });
        }
        drop(store);
        let mut live = self
            .live
            .lock()
            .map_err(|_| anyhow::anyhow!("список завдань отруєно"))?;
        for row in loaded {
            live.insert(row.id, row);
        }
        Ok(())
    }

    fn стеля(&self) -> usize {
        match self.налаштування.lock() {
            Ok(settings) => match usize::try_from(settings.max_concurrent) {
                Ok(limit) => limit.max(1),
                Err(error) => {
                    tracing::error!(%error, "стеля одночасних завантажень не вмістилась у usize; беру 1");
                    1
                }
            },
            Err(_) => {
                tracing::error!("налаштування отруєно; стелю одночасних завантажень знижено до 1");
                1
            }
        }
    }

    fn вікно_відкрите(&self) -> bool {
        let now = хвилини_зараз();
        match self.налаштування.lock() {
            Ok(settings) => settings.downloads_allowed(now),
            Err(_) => {
                tracing::error!("налаштування отруєно; нові завантаження не стартуватимуть");
                false
            }
        }
    }

    /// Поточні правила для вікна й CLI.
    pub fn settings(&self) -> Settings {
        match self.налаштування.lock() {
            Ok(settings) => settings.clone(),
            Err(_) => {
                tracing::error!("налаштування отруєно; клієнтові повернуто типові значення");
                Settings::default()
            }
        }
    }

    /// Змінити правила ядра й записати їх у базу.
    pub fn configure(self: &Arc<Self>, patch: SettingsPatch) -> anyhow::Result<()> {
        let mut current = self
            .налаштування
            .lock()
            .map_err(|_| anyhow::anyhow!("налаштування отруєно; зміни не застосовано"))?;
        let mut next = current.clone();
        next.apply_patch(patch)?;

        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
            store.save_settings(&next)?;
        }
        *current = next.clone();
        drop(current);
        let effective_rate = next.effective_rate(хвилини_зараз());
        self.поточний_ліміт.store(effective_rate, Ordering::Relaxed);
        self.registry.set_rate_limit(effective_rate);

        self.вікно_було
            .store(next.downloads_allowed(хвилини_зараз()), Ordering::Relaxed);
        self.clone().спробувати_наступне();
        self.clone().після_зміни_черги();
        Ok(())
    }

    /// Підписатись на потік подій.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Поточний список завдань.
    pub fn list(&self) -> Vec<TaskView> {
        let Ok(live) = self.live.lock() else {
            tracing::error!("список завдань отруєно");
            return Vec::new();
        };

        let mut out: Vec<TaskView> = live.values().map(Live::to_view).collect();
        // Найновіші зверху — так само, як у базі.
        out.sort_by_key(|b| std::cmp::Reverse(b.id));
        out
    }

    /// Розкладка частин одного завдання.
    ///
    /// Порожньо — коли завдання не качається або качається одним потоком:
    /// малювати «один сегмент на всю ширину» те саме, що звичайна смужка
    /// прогресу, лише дорожче.
    ///
    /// Це запит **на вимогу**, а не частина знімка. Розкладка цікава тільки
    /// для одного розгорнутого завдання, а в знімку вона помножилась би на
    /// весь список — п'ятдесят завдань по шістнадцять частин чотири рази на
    /// секунду.
    pub fn details(&self, id: i64) -> Vec<PartView> {
        let Ok(live) = self.live.lock() else {
            tracing::error!("список завдань отруєно");
            return Vec::new();
        };

        live.get(&id)
            .map(|task| {
                task.parts
                    .iter()
                    .map(|p| PartView {
                        start: p.start,
                        end: p.end,
                        done: p.done,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Які варіанти якості пропонує це посилання.
    ///
    /// Порожній перелік — вибирати нема з чого, і це нормальний випадок:
    /// звичайний файл має один вигляд.
    ///
    /// ⚠️ Ядро не заглядає в `id` варіанта. Узяло в модуля, віддало клієнту,
    /// поверне модулю — і не знає, що за ним стоїть. Варто раз розібрати цей
    /// рядок тут, і обіцянка «новий модуль доточується без правок ядра»
    /// стане неправдою.
    pub async fn variants(&self, url: &str) -> anyhow::Result<Vec<VariantView>> {
        let Some(protocol) = self.registry.find(url) else {
            anyhow::bail!("не знайшлося модуля для {url}");
        };

        let probed = protocol.probe(url).await?;

        Ok(probed
            .variants
            .into_iter()
            .map(|v| VariantView {
                id: v.id,
                label: v.label,
                height: v.height,
                size: v.size,
                note: v.note,
            })
            .collect())
    }

    /// Додати завантаження й одразу почати його.
    #[allow(clippy::too_many_arguments)]
    pub async fn add(
        self: &Arc<Self>,
        url: &str,
        dest: Option<PathBuf>,
        parts: Option<usize>,
        session: Session,
        variant: Option<String>,
        queue: Option<String>,
        max_duration: Option<std::time::Duration>,
        rewind: bool,
    ) -> anyhow::Result<i64> {
        // Хто це качатиме, вирішує реєстр, а не ядро. Саме тут і живе межа.
        let Some(protocol) = self.registry.find(url) else {
            anyhow::bail!(
                "жоден модуль не впізнав це посилання: {url}. Доступні: {:?}",
                self.registry.names()
            );
        };

        let protocol_name = protocol.name().to_owned();

        if parts == Some(0) {
            anyhow::bail!("кількість з'єднань для {url} не може бути нулем");
        }
        if parts.is_some() && !protocol.supports_parallelism() {
            anyhow::bail!(
                "модуль {protocol_name} не підтримує задану кількість з'єднань для {url}"
            );
        }

        if let Some(id) = знайти_живе(&self.live, url)? {
            anyhow::bail!("це посилання вже качається як завдання {id}");
        }

        let (actual_queue, q_row) = {
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
            let q_name = queue.unwrap_or_else(|| downloader_core::store::DEFAULT_QUEUE.to_owned());
            let row = store
                .queue(&q_name)?
                .ok_or_else(|| anyhow::anyhow!("черги '{q_name}' не існує"))?;
            (q_name, row)
        };

        // Сесія до проби: інакше `/auth` і сесійне HLS знову дадуть 403.
        protocol.set_session(session.clone());
        if session.cookies.is_some() {
            tracing::info!("cookie задано");
        }
        if session.referer.is_some() {
            tracing::info!("referer задано");
        }

        // Проба до запису в базу: інакше в списку з'явиться завдання, про яке
        // нічого не відомо — ні розміру, ні імені, ні чи воно взагалі існує.
        let probed = protocol.probe(url).await?;

        if probed.files.is_empty() {
            anyhow::bail!("модуль {protocol_name} не назвав жодного файла для {url}");
        }

        let (base_dir, category_id) = {
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
            if dest.is_some() {
                (self.downloads_dir.clone(), None)
            } else {
                let name = probed
                    .files
                    .iter()
                    .find(|f| f.selected)
                    .map(|f| f.suggested_name.as_str())
                    .unwrap_or("");
                match store.category_for(name)? {
                    Some(c) => (c.folder, Some(c.id)),
                    None => (self.downloads_dir.clone(), None),
                }
            }
        };

        if dest.is_none()
            && let Err(e) = std::fs::create_dir_all(&base_dir)
        {
            anyhow::bail!(
                "не вдалося створити теку категорії {}: {e}",
                base_dir.display()
            );
        }

        let planned = paths_for_selected(&probed.files, dest.as_deref(), &base_dir);
        if planned.is_empty() {
            anyhow::bail!("модуль {protocol_name} не обрав жодного файла для {url}");
        }

        let targets: Vec<PathBuf> = planned.iter().map(|(p, _)| p.clone()).collect();
        let need: u64 = planned.iter().map(|(_, size)| size.unwrap_or(0)).sum();
        if let Some(first) = targets.first() {
            вистачить_місця(first, need)?;
        }

        let mut live = self
            .live
            .lock()
            .map_err(|_| anyhow::anyhow!("список завдань отруєно; завдання не додано"))?;
        if let Some(id) = live.values().find_map(|task| {
            (task.url == url
                && matches!(
                    task.status,
                    Status::Running | Status::Queued | Status::Paused
                ))
            .then_some(task.id)
        }) {
            anyhow::bail!("це посилання вже качається як завдання {id}");
        }

        let start_now = {
            let running_total = live
                .values()
                .filter(|t| t.status == Status::Running)
                .count();
            let running_in_q = live
                .values()
                .filter(|t| t.queue == actual_queue && t.status == Status::Running)
                .count();
            self.вікно_відкрите()
                && q_row.downloads_allowed(хвилини_зараз())
                && running_total < self.стеля()
                && running_in_q < q_row.max_concurrent as usize
        };
        let initial_status = if start_now {
            Status::Running
        } else {
            Status::Queued
        };

        let id = {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;

            let id = store.add_task(&NewTask {
                url: probed.final_url.clone(),
                protocol: protocol_name.clone(),
                title: None,
                category_id,
                variant: variant.clone(),
                queue: Some(actual_queue.clone()),
                files: planned
                    .iter()
                    .map(|(path, size)| NewFile {
                        path: path.clone(),
                        size: *size,
                        fingerprint: probed.fingerprint.clone(),
                        selected: true,
                    })
                    .collect(),
            })?;
            if let Err(status_error) = store.set_status(id, initial_status, None) {
                match store.remove_task(id) {
                    Ok(()) => anyhow::bail!(
                        "стан нового завдання {id} не збережено; запис відкочено: {status_error}"
                    ),
                    Err(rollback_error) => anyhow::bail!(
                        "стан нового завдання {id} не збережено ({status_error}), а запис не відкочено ({rollback_error})"
                    ),
                }
            }
            id
        };

        let name = targets
            .first()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| names::ЗАПАСНЕ_ІМʼЯ.to_owned());

        live.insert(
            id,
            Live {
                id,
                url: probed.final_url.clone(),
                name,
                status: initial_status,
                done: 0,
                total: probed.total_size,
                segments: 0,
                error: None,
                prev_done: 0,
                speed: 0,
                session: session.clone(),
                protocol: protocol_name.clone(),
                targets: targets.clone(),
                variant: variant.clone(),
                parts: Vec::new(),
                parallelism: parts,
                queue: actual_queue.clone(),
                max_duration,
                rewind,
            },
        );
        drop(live);

        if start_now {
            self.clone().spawn_download(
                id,
                protocol_name,
                probed.final_url,
                targets,
                session,
                variant,
                actual_queue,
                max_duration,
                rewind,
                parts,
            );
        } else {
            if self.вікно_відкрите() {
                tracing::info!(
                    id,
                    "завдання в черзі (стеля {n} одночасних)",
                    n = self.стеля()
                );
            } else {
                tracing::info!(id, "завдання в черзі (поза розкладом)");
            }
        }
        self.clone().після_зміни_черги();
        Ok(id)
    }

    /// Запустити качання окремою задачею.
    #[allow(clippy::too_many_arguments)]
    fn spawn_download(
        self: Arc<Self>,
        id: i64,
        protocol: String,
        source: String,
        targets: Vec<PathBuf>,
        session: Session,
        variant: Option<String>,
        queue_name: String,
        max_duration: Option<std::time::Duration>,
        rewind: bool,
        parallelism: Option<usize>,
    ) {
        tokio::spawn(async move {
            let Some(module) = self.registry.by_name(&protocol) else {
                let message = format!("модуль {protocol} зник із реєстру");
                self.fail(id, &message);
                return;
            };

            let sink = LiveSink {
                id,
                live: Arc::clone(&self.live),
            };

            let cancel = Cancel::new();
            match self.cancels.lock() {
                Ok(mut cancels) => {
                    cancels.insert(id, cancel.clone());
                }
                Err(_) => {
                    self.fail(
                        id,
                        "список прапорців зупинки отруєно; завантаження не запущено",
                    );
                    return;
                }
            }

            let queue_rate_result = (|| -> Result<u64, String> {
                let store = self
                    .store
                    .lock()
                    .map_err(|_| "сховище отруєно; ліміт черги не прочитано".to_owned())?;
                let queue = store
                    .queue(&queue_name)
                    .map_err(|error| format!("ліміт черги '{queue_name}' не прочитано: {error}"))?
                    .ok_or_else(|| format!("черга '{queue_name}' зникла перед стартом завдання"))?;
                Ok(queue.rate_limit)
            })();
            let queue_rate_limit = match queue_rate_result {
                Ok(limit) => limit,
                Err(message) => {
                    self.fail(id, &message);
                    return;
                }
            };

            let limiter = if queue_rate_limit > 0 {
                let mut limiters = match self.queue_limiters.lock() {
                    Ok(limiters) => limiters,
                    Err(_) => {
                        self.fail(
                            id,
                            "обмежувачі черг отруєно; завантаження не запущено без ліміту",
                        );
                        return;
                    }
                };
                let entry = limiters.entry(queue_name.clone()).or_insert_with(|| {
                    Arc::new(downloader_core::rate::RateLimiter::new(queue_rate_limit))
                });
                Some(entry.clone())
            } else {
                None
            };

            let global_limit = match self.налаштування.lock() {
                Ok(settings) => settings.effective_rate(хвилини_зараз()),
                Err(_) => {
                    let last_known = self.поточний_ліміт.load(Ordering::Relaxed);
                    tracing::error!(
                        id,
                        last_known,
                        "налаштування отруєно; використано останній відомий ліміт швидкості"
                    );
                    last_known
                }
            };
            if global_limit > 0 {
                match module.set_rate_limit(global_limit) {
                    RateLimitSupport::Applied => {}
                    RateLimitSupport::Unsupported => {
                        tracing::debug!(id, "модуль не вміє ліміт швидкості");
                    }
                }
            }

            module.set_session(session.clone());
            let ctx = RunContext {
                task_id: id,
                source,
                targets,
                resume: None,
                cancel,
                session,
                variant,
                limiter,
                max_duration,
                rewind,
            };

            match module
                .run_with_parallelism(ctx.clone(), parallelism, &sink)
                .await
            {
                // Непорожня відповідь означає «зупинено на прохання»: це не
                // завершення й не помилка.
                Ok(Some(_resume)) => {
                    if let Err(error) = self.pause_finished(id) {
                        self.fail(id, &format!("не вдалося завершити паузу: {error}"));
                    }
                    self.clone().спробувати_наступне();
                    self.clone().після_зміни_черги();
                }

                Ok(None) => {
                    if let Err(e) = module.verify(&ctx).await {
                        self.fail(id, &e.to_string());
                        self.clone().спробувати_наступне();
                        self.clone().після_зміни_черги();
                        return;
                    }

                    // Mark-of-the-Web ставиться **до** того, як завдання стане
                    // «завершеним»: інакше людина встигне відкрити файл, який
                    // ще не позначено як отриманий з мережі.
                    //
                    // Це теж робота ядра, а не модуля: правило однакове для
                    // будь-якого джерела.
                    for path in &ctx.targets {
                        if let Err(e) = motw::mark(path, Some(&ctx.source), None) {
                            tracing::warn!(error = %e, "не вдалося позначити файл MotW");
                        }
                    }

                    let bytes = match self.finish(id) {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            self.fail(id, &format!("завершення не зафіксовано: {error}"));
                            return;
                        }
                    };
                    if let Err(e) = self.зафіксувати_перевірку(id, bytes) {
                        self.fail(id, &e.to_string());
                        return;
                    }
                    let path = ctx
                        .targets
                        .first()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    self.розіслати(Event::Finished { id, path, bytes }, "завершення");
                    self.clone().спробувати_наступне();
                    self.clone().після_зміни_черги();
                }

                Err(e) => {
                    self.fail(id, &e.to_string());
                    self.clone().спробувати_наступне();
                    self.clone().після_зміни_черги();
                }
            }
        });
    }

    /// Якщо є вільний слот — знайти черги, які дозволені й не вичерпали ліміт,
    /// обрати найстаріше завдання й запустити.
    fn спробувати_наступне(self: Arc<Self>) {
        if !self.вікно_відкрите() {
            return;
        }

        let now_min = хвилини_зараз();

        let queues = {
            let store = match self.store.lock() {
                Ok(store) => store,
                Err(_) => {
                    tracing::error!("сховище отруєно; планувальник не прочитав черги");
                    return;
                }
            };
            match store.queues() {
                Ok(queues) => queues,
                Err(error) => {
                    tracing::error!(%error, "планувальник не прочитав черги зі сховища");
                    return;
                }
            }
        };

        loop {
            let job = {
                let mut live = match self.live.lock() {
                    Ok(live) => live,
                    Err(_) => {
                        tracing::error!("список завдань отруєно; планувальник зупинено");
                        return;
                    }
                };

                // Глобальна стеля
                let running_total = live
                    .values()
                    .filter(|t| t.status == Status::Running)
                    .count();
                if running_total >= self.стеля() {
                    return;
                }

                // Список черг, у яких зараз дозволено брати нові завдання
                let mut eligible_queues = std::collections::HashSet::new();
                for q in &queues {
                    if !q.downloads_allowed(now_min) {
                        continue;
                    }
                    let running_in_q = live
                        .values()
                        .filter(|t| t.queue == q.name && t.status == Status::Running)
                        .count();
                    if running_in_q < q.max_concurrent as usize {
                        eligible_queues.insert(q.name.as_str());
                    }
                }

                if eligible_queues.is_empty() {
                    return;
                }

                // Найстаріше завдання серед черг, що проходять перевірку
                let next_id = live
                    .values()
                    .filter(|t| {
                        t.status == Status::Queued && eligible_queues.contains(t.queue.as_str())
                    })
                    .map(|t| t.id)
                    .min();

                let Some(id) = next_id else {
                    return;
                };

                let Some(task) = live.get_mut(&id) else {
                    return;
                };

                task.status = Status::Running;
                task.error = None;

                (
                    task.id,
                    task.protocol.clone(),
                    task.url.clone(),
                    task.targets.clone(),
                    task.session.clone(),
                    task.variant.clone(),
                    task.queue.clone(),
                    task.max_duration,
                    task.rewind,
                    task.parallelism,
                )
            };

            let (
                id,
                protocol,
                url,
                targets,
                session,
                variant,
                queue_name,
                max_duration,
                rewind,
                parallelism,
            ) = job;
            if let Err(error) = self.set_status(id, Status::Running, None) {
                tracing::error!(id, %error, "завдання не запущено: стан running не збережено");
                match self.live.lock() {
                    Ok(mut live) => {
                        if let Some(task) = live.get_mut(&id) {
                            task.status = Status::Queued;
                        }
                    }
                    Err(_) => tracing::error!(
                        id,
                        "список завдань отруєно; не вдалося повернути завдання в queued"
                    ),
                }
                return;
            }
            self.clone().spawn_download(
                id,
                protocol,
                url,
                targets,
                session,
                variant,
                queue_name,
                max_duration,
                rewind,
                parallelism,
            );
        }
    }

    /// Зупинити завдання на прохання людини.
    ///
    /// Прапорець лише **просить** модуль спинитись; сам перехід у `Paused`
    /// відбувається тоді, коли модуль справді завершив роботу. Інакше в
    /// списку з'явилося б «зупинено» на завданні, яке ще пише в файл.
    pub fn pause(&self, id: i64) -> anyhow::Result<()> {
        let Ok(cancels) = self.cancels.lock() else {
            anyhow::bail!("список прапорців отруєно");
        };

        let Some(cancel) = cancels.get(&id) else {
            anyhow::bail!("завдання {id} не качається — нема чого зупиняти");
        };

        cancel.cancel();
        Ok(())
    }

    /// Продовжити зупинене завдання.
    pub async fn resume(self: &Arc<Self>, id: i64) -> anyhow::Result<()> {
        let (url, protocol, targets) = {
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;

            let task = store
                .task(id)?
                .ok_or_else(|| anyhow::anyhow!("завдання {id} не знайдено"))?;

            let files = store.files(id)?;
            let targets: Vec<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
            if targets.is_empty() {
                anyhow::bail!("у завдання {id} немає файлів");
            }

            (task.url, task.protocol, targets)
        };

        let mut live = self
            .live
            .lock()
            .map_err(|_| anyhow::anyhow!("список завдань отруєно; завдання {id} не відновлено"))?;
        let task = live
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("завдання {id} є в базі, але відсутнє в пам'яті"))?;
        self.set_status(id, Status::Queued, None)?;
        task.status = Status::Queued;
        task.error = None;
        task.protocol = protocol;
        task.targets = targets;
        task.url = url;
        drop(live);
        self.clone().спробувати_наступне();
        self.clone().після_зміни_черги();
        Ok(())
    }

    /// Модуль спинився на прохання — перевести завдання в паузу.
    fn pause_finished(&self, id: i64) -> anyhow::Result<()> {
        let mut live = self
            .live
            .lock()
            .map_err(|_| anyhow::anyhow!("список завдань отруєно"))?;
        let task = live
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("завдання {id} відсутнє в пам'яті"))?;
        self.set_status(id, Status::Paused, None)?;
        task.status = Status::Paused;
        task.speed = 0;
        drop(live);
        self.прибрати_прапорець(id, "після паузи");
        tracing::info!(id, "завдання зупинено на прохання");
        Ok(())
    }

    /// Позначити завдання завершеним і повернути, скільки завантажено.
    fn finish(&self, id: i64) -> anyhow::Result<u64> {
        let mut live = self
            .live
            .lock()
            .map_err(|_| anyhow::anyhow!("список завдань отруєно"))?;
        let task = live
            .get_mut(&id)
            .ok_or_else(|| anyhow::anyhow!("завдання {id} відсутнє в пам'яті"))?;
        self.set_status(id, Status::Done, None)?;
        task.status = Status::Done;
        task.speed = 0;
        if task.total.is_none() {
            task.total = Some(task.done);
        }
        let bytes = task.done;
        drop(live);
        self.прибрати_прапорець(id, "після завершення");
        Ok(bytes)
    }

    /// Позначити завдання невдалим.
    fn fail(&self, id: i64, message: &str) {
        tracing::warn!(id, error = %message, "завдання впало");

        let transition = (|| -> anyhow::Result<()> {
            let mut live = self
                .live
                .lock()
                .map_err(|_| anyhow::anyhow!("список завдань отруєно"))?;
            let task = live
                .get_mut(&id)
                .ok_or_else(|| anyhow::anyhow!("завдання {id} відсутнє в пам'яті"))?;
            let persisted = self.set_status(id, Status::Failed, Some(message));
            task.status = Status::Failed;
            task.error = Some(message.to_owned());
            task.speed = 0;
            drop(live);
            self.прибрати_прапорець(id, "після збою");
            persisted
        })();
        if let Err(error) = transition {
            tracing::error!(id, %error, "помилку завдання не вдалося узгодити між пам'яттю та базою");
        }
        self.розіслати(
            Event::Failed {
                id,
                message: message.to_owned(),
            },
            "помилку",
        );
    }

    /// Прибрати вже непотрібний прапорець, не скасовуючи завершений перехід стану.
    ///
    /// Отруєний mutex тут не має права повернути `Done`, `Paused` або `Failed`
    /// назад у `Running`: стан уже зафіксовано в пам'яті та SQLite. Помилка
    /// очищення не замовчується — вона лишається в журналі з ідентифікатором.
    fn прибрати_прапорець(&self, id: i64, context: &str) {
        match self.cancels.lock() {
            Ok(mut cancels) => {
                cancels.remove(&id);
            }
            Err(_) => tracing::error!(
                id,
                context,
                "список прапорців зупинки отруєно; завершений стан збережено, але прапорець не прибрано"
            ),
        }
    }

    /// Прибрати завдання зі списку.
    pub fn remove(self: &Arc<Self>, id: i64, with_file: bool) -> anyhow::Result<()> {
        let mut live = self
            .live
            .lock()
            .map_err(|_| anyhow::anyhow!("список завдань отруєно; завдання {id} не видалено"))?;
        let mut cancels = self.cancels.lock().map_err(|_| {
            anyhow::anyhow!("список прапорців зупинки отруєно; завдання {id} не видалено")
        })?;
        let path = {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно; завдання {id} не видалено"))?;
            let path = store.files(id)?.first().map(|f| f.path.clone());
            store.remove_task(id)?;
            path
        };
        live.remove(&id);
        let cancel = cancels.remove(&id);
        drop(cancels);
        drop(live);
        if let Some(cancel) = cancel {
            cancel.cancel();
        }

        if with_file && let Some(p) = path {
            // Разом із файлом прибираємо і його стан: інакше наступне
            // завантаження того самого посилання спробує «докачати» те,
            // чого вже немає.
            match std::fs::remove_file(&p) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => anyhow::bail!(
                    "завдання {id} видалено, але файл {} лишився: {error}",
                    p.display()
                ),
            }
            downloader_core::state::remove(&p).map_err(|error| {
                anyhow::anyhow!(
                    "завдання {id} і файл {} видалено, але стан докачування лишився: {error}",
                    p.display()
                )
            })?;
        }

        self.clone().після_зміни_черги();
        Ok(())
    }

    /// Записати стан у базу й віддати помилку викликачеві.
    fn set_status(&self, id: i64, status: Status, error: Option<&str>) -> anyhow::Result<()> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("сховище отруєно; стан завдання {id} не збережено"))?;
        store.set_status(id, status, error)?;
        Ok(())
    }

    /// Розіслати подію; відсутність підписників є нормальною, але не мовчазною.
    fn розіслати(&self, event: Event, kind: &str) {
        match self.events.send(event) {
            Ok(_) => {}
            Err(_) => tracing::trace!(kind, "подію не розіслано: немає підписників"),
        }
    }

    /// Оновити швидкість живих завдань.
    fn refresh_progress(&self) {
        let Ok(mut live) = self.live.lock() else {
            tracing::error!("список завдань отруєно; швидкість не оновлено");
            return;
        };

        for task in live.values_mut() {
            if task.status != Status::Running {
                continue;
            }

            // Швидкість рахує ядро, а не клієнт: інакше два різні клієнти
            // покажуть різні числа для того самого завдання.
            let delta = task.done.saturating_sub(task.prev_done);
            let per_sec = delta * 1000 / TICK.as_millis().max(1) as u64;

            // Згладжування: без нього число стрибає на кожному тіку й читати
            // його неможливо.
            task.speed = (task.speed * 2 + per_sec) / 3;
            task.prev_done = task.done;
        }
    }

    /// Раз на тік розсилати знімок списку й відкривати вікно розкладу.
    fn start_ticker(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(TICK);
            loop {
                tick.tick().await;
                self.refresh_progress();

                let open = self.вікно_відкрите();
                let was = self.вікно_було.swap(open, Ordering::Relaxed);
                if open && !was {
                    tracing::info!("розклад: вікно відкрилось, стартую чергу");
                    self.clone().спробувати_наступне();
                } else if open {
                    let has_queued = match self.live.lock() {
                        Ok(live) => live.values().any(|t| t.status == Status::Queued),
                        Err(_) => {
                            tracing::error!(
                                "список завдань отруєно; тік не перевірив queued-завдання"
                            );
                            false
                        }
                    };
                    if has_queued {
                        self.clone().спробувати_наступне();
                    }
                }

                let limit = match self.налаштування.lock() {
                    Ok(settings) => settings.effective_rate(хвилини_зараз()),
                    Err(_) => {
                        let last_known = self.поточний_ліміт.load(Ordering::Relaxed);
                        tracing::error!(
                            last_known,
                            "налаштування отруєно; тік зберіг останній відомий ліміт швидкості"
                        );
                        last_known
                    }
                };
                let prev_limit = self.поточний_ліміт.swap(limit, Ordering::Relaxed);
                if limit != prev_limit {
                    self.registry.set_rate_limit(limit);
                }

                // Нема кому слухати — нема чого й складати знімок.
                if self.events.receiver_count() == 0 {
                    continue;
                }

                self.розіслати(Event::Snapshot { tasks: self.list() }, "знімок");
            }
        });
    }
}

#[cfg(test)]
fn id_наступного_в_черзі(live: &HashMap<i64, Live>) -> Option<i64> {
    live.values()
        .filter(|t| t.status == Status::Queued)
        .map(|t| t.id)
        .min()
}

/// Живе завдання з тим самим URL: running / queued / paused.
fn знайти_живе(
    live: &Mutex<HashMap<i64, Live>>,
    url: &str,
) -> anyhow::Result<Option<i64>> {
    let guard = live
        .lock()
        .map_err(|_| anyhow::anyhow!("список завдань отруєно; дубль URL не перевірено"))?;
    Ok(guard.values().find_map(|t| {
        if t.url == url && matches!(t.status, Status::Running | Status::Queued | Status::Paused) {
            Some(t.id)
        } else {
            None
        }
    }))
}

/// Шляхи для файлів із `selected == true`.
///
/// Якщо людина дала `dest` і обраний рівно один файл — беремо `dest` як є.
/// Якщо обраних кілька — `dest` це перший, решта з `suggested_name` у тій
/// самій теці. Необрані сюди не потрапляють і в базу не пишуться.
fn paths_for_selected(
    files: &[PlannedFile],
    dest: Option<&Path>,
    downloads_dir: &Path,
) -> Vec<(PathBuf, Option<u64>)> {
    let mut reserved = Vec::new();
    let mut out = Vec::new();

    for (i, file) in files.iter().filter(|f| f.selected).enumerate() {
        let path = match dest {
            Some(given) if i == 0 => given.to_path_buf(),
            Some(given) => {
                let dir = match given.parent() {
                    Some(p) if !p.as_os_str().is_empty() => p,
                    _ => downloads_dir,
                };
                let safe = names::sanitize(&file.suggested_name);
                unique_among(&dir.join(safe), &reserved)
            }
            None => {
                let safe = names::sanitize(&file.suggested_name);
                unique_among(&downloads_dir.join(safe), &reserved)
            }
        };
        reserved.push(path.clone());
        out.push((path, file.size));
    }
    out
}

/// `unique_path` дивиться лише диск; уже призначені шляхи можуть ще не існувати.
fn unique_among(desired: &Path, reserved: &[PathBuf]) -> PathBuf {
    let candidate = paths::unique_path(desired);
    if !reserved.contains(&candidate) {
        return candidate;
    }

    let parent = desired.parent().unwrap_or_else(|| Path::new("."));
    let name = desired
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| names::ЗАПАСНЕ_ІМʼЯ.to_owned());
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && !e.is_empty() => (s.to_owned(), Some(e.to_owned())),
        _ => (name, None),
    };

    for n in 1..=1000 {
        let next = match &ext {
            Some(e) => parent.join(format!("{stem} ({n}).{e}")),
            None => parent.join(format!("{stem} ({n})")),
        };
        let unique = paths::unique_path(&next);
        if !reserved.contains(&unique) {
            return unique;
        }
    }

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    match ext {
        Some(e) => parent.join(format!("{stem} ({stamp}).{e}")),
        None => parent.join(format!("{stem} ({stamp})")),
    }
}

fn хвилини_зараз() -> u16 {
    use chrono::Timelike;
    let t = chrono::Local::now();
    let total = t.hour().saturating_mul(60).saturating_add(t.minute());
    u16::try_from(total).unwrap_or(0)
}

fn черга_спорожніла(live: &HashMap<i64, Live>) -> bool {
    !live
        .values()
        .any(|t| matches!(t.status, Status::Running | Status::Queued))
}

fn черга_порожня_під_замком(
    live: &Mutex<HashMap<i64, Live>>,
) -> anyhow::Result<bool> {
    let live = live
        .lock()
        .map_err(|_| anyhow::anyhow!("список завдань отруєно; порожнечу черги не визначено"))?;
    Ok(черга_спорожніла(&live))
}

impl Engine {
    /// Звірити лічильник із диском і записати SHA-256. Не довіряти RAM.
    fn зафіксувати_перевірку(&self, id: i64, bytes: u64) -> anyhow::Result<()> {
        let files = {
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
            store.files(id)?
        };
        if files.len() == 1 {
            let f = &files[0];
            downloader_core::verify::length(&f.path, bytes)?;
            let hash = downloader_core::verify::sha256(&f.path)?;
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
            store.set_file_checksum(f.id, &hash)?;
        } else {
            for f in &files {
                let hash = downloader_core::verify::sha256(&f.path)?;
                let mut store = self
                    .store
                    .lock()
                    .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
                store.set_file_checksum(f.id, &hash)?;
            }
        }
        Ok(())
    }

    fn черга_порожня(&self) -> bool {
        match черга_порожня_під_замком(&self.live) {
            Ok(empty) => empty,
            Err(error) => {
                tracing::error!(%error, "черга вважається непорожньою");
                false
            }
        }
    }

    fn після_зміни_черги(self: Arc<Self>) {
        let idle = self.черга_порожня();
        let mut action = self.settings().post_action;
        if action == downloader_core::post_action::PostAction::None {
            match self.store.lock() {
                Ok(store) => match store.queues() {
                    Ok(queues) => {
                        for queue in queues {
                            if queue.post_action != downloader_core::post_action::PostAction::None {
                                action = queue.post_action;
                                break;
                            }
                        }
                    }
                    Err(error) => tracing::error!(
                        %error,
                        "післядію черги не прочитано зі сховища"
                    ),
                },
                Err(_) => tracing::error!("сховище отруєно; післядію черги не прочитано"),
            }
        }
        if idle && action == downloader_core::post_action::PostAction::None {
            tracing::trace!("черга порожня, але післядію не налаштовано");
        }
        let me = Arc::clone(&self);
        self.after.notify(idle, action, move || me.черга_порожня());
    }

    /// Отримати список усіх черг з лічильниками завдань.
    pub fn queues(&self) -> Vec<downloader_ipc::protocol::QueueView> {
        let rows = {
            let store = match self.store.lock() {
                Ok(store) => store,
                Err(_) => {
                    tracing::error!("сховище отруєно; список черг не сформовано");
                    return Vec::new();
                }
            };
            match store.queues() {
                Ok(rows) => rows,
                Err(error) => {
                    tracing::error!(%error, "черги не прочитано зі сховища");
                    return Vec::new();
                }
            }
        };
        let live = match self.live.lock() {
            Ok(l) => l,
            Err(_) => {
                tracing::error!("список завдань отруєно; лічильники черг не сформовано");
                return Vec::new();
            }
        };
        rows.into_iter()
            .map(|q| {
                let total_tasks = live.values().filter(|t| t.queue == q.name).count();
                let running_tasks = live
                    .values()
                    .filter(|t| t.queue == q.name && t.status == Status::Running)
                    .count();
                downloader_ipc::protocol::QueueView {
                    id: q.id,
                    name: q.name,
                    max_concurrent: q.max_concurrent,
                    rate_limit: q.rate_limit,
                    paused: q.paused,
                    schedule_from: q.schedule_from.map(downloader_core::format_hhmm),
                    schedule_to: q.schedule_to.map(downloader_core::format_hhmm),
                    post_action: q.post_action.as_str().to_owned(),
                    total_tasks,
                    running_tasks,
                }
            })
            .collect()
    }

    /// Створити нову чергу.
    pub fn create_queue(
        &self,
        name: String,
        patch: downloader_core::store::QueuePatch,
    ) -> anyhow::Result<()> {
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
        store.create_queue(&name, &patch)?;
        Ok(())
    }

    /// Змінити параметри існуючої черги.
    pub fn configure_queue(
        self: &Arc<Self>,
        name: &str,
        patch: downloader_core::store::QueuePatch,
    ) -> anyhow::Result<()> {
        let mut limiters = match patch.rate_limit {
            Some(_) => Some(self.queue_limiters.lock().map_err(|_| {
                anyhow::anyhow!("обмежувачі черг отруєно; чергу '{name}' не змінено")
            })?),
            None => None,
        };
        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
            store.update_queue(name, &patch)?;
        }
        if let (Some(new_rate), Some(limiters)) = (patch.rate_limit, limiters.as_mut()) {
            if new_rate > 0 {
                limiters.insert(
                    name.to_owned(),
                    Arc::new(downloader_core::rate::RateLimiter::new(new_rate)),
                );
            } else {
                limiters.remove(name);
            }
        }
        self.clone().спробувати_наступне();
        self.clone().після_зміни_черги();
        Ok(())
    }

    /// Призупинити чергу: поставити прапорець і зупинити активні завдання черги.
    pub fn pause_queue(self: &Arc<Self>, name: &str) -> anyhow::Result<()> {
        let running_ids: Vec<i64> = {
            let live = self.live.lock().map_err(|_| {
                anyhow::anyhow!("список завдань отруєно; чергу '{name}' не призупинено")
            })?;
            live.values()
                .filter(|t| t.queue == name && t.status == Status::Running)
                .map(|t| t.id)
                .collect()
        };
        let cancels = self.cancels.lock().map_err(|_| {
            anyhow::anyhow!("список прапорців зупинки отруєно; чергу '{name}' не призупинено")
        })?;
        for id in &running_ids {
            anyhow::ensure!(
                cancels.contains_key(id),
                "завдання {id} позначене running, але не має прапорця зупинки; чергу '{name}' не призупинено"
            );
        }
        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
            store.update_queue(
                name,
                &downloader_core::store::QueuePatch {
                    paused: Some(true),
                    ..Default::default()
                },
            )?;
        }
        for id in &running_ids {
            if let Some(cancel) = cancels.get(id) {
                cancel.cancel();
            }
        }
        drop(cancels);
        self.clone().після_зміни_черги();
        Ok(())
    }

    /// Відновити роботу черги: зняти паузу й перевести її paused-завдання в queued.
    pub fn resume_queue(self: &Arc<Self>, name: &str) -> anyhow::Result<()> {
        let mut live = self
            .live
            .lock()
            .map_err(|_| anyhow::anyhow!("список завдань отруєно; чергу '{name}' не відновлено"))?;
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("сховище отруєно; чергу '{name}' не відновлено"))?;
        store.update_queue(
            name,
            &downloader_core::store::QueuePatch {
                paused: Some(false),
                ..Default::default()
            },
        )?;
        for task in live.values_mut() {
            if task.queue == name && task.status == Status::Paused {
                store.set_status(task.id, Status::Queued, None)?;
                task.status = Status::Queued;
                task.error = None;
            }
        }
        drop(store);
        drop(live);
        self.clone().спробувати_наступне();
        self.clone().після_зміни_черги();
        Ok(())
    }

    /// Перейменувати чергу.
    pub fn rename_queue(&self, old_name: &str, new_name: &str) -> anyhow::Result<()> {
        let mut live = self.live.lock().map_err(|_| {
            anyhow::anyhow!("список завдань отруєно; чергу '{old_name}' не перейменовано")
        })?;
        let mut limiters = self.queue_limiters.lock().map_err(|_| {
            anyhow::anyhow!("обмежувачі черг отруєно; чергу '{old_name}' не перейменовано")
        })?;
        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
            store.rename_queue(old_name, new_name)?;
        }
        for task in live.values_mut() {
            if task.queue == old_name {
                task.queue = new_name.to_owned();
            }
        }
        if let Some(lim) = limiters.remove(old_name) {
            limiters.insert(new_name.to_owned(), lim);
        }
        Ok(())
    }

    /// Видалити чергу (перевівши її завдання в default).
    pub fn delete_queue(self: &Arc<Self>, name: &str) -> anyhow::Result<()> {
        let mut live = self
            .live
            .lock()
            .map_err(|_| anyhow::anyhow!("список завдань отруєно; чергу '{name}' не видалено"))?;
        let mut limiters = self
            .queue_limiters
            .lock()
            .map_err(|_| anyhow::anyhow!("обмежувачі черг отруєно; чергу '{name}' не видалено"))?;
        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
            store.delete_queue(name)?;
        }
        for task in live.values_mut() {
            if task.queue == name {
                task.queue = downloader_core::store::DEFAULT_QUEUE.to_owned();
            }
        }
        limiters.remove(name);
        drop(limiters);
        drop(live);
        self.clone().спробувати_наступне();
        self.clone().після_зміни_черги();
        Ok(())
    }

    /// Перемістити завдання в іншу чергу.
    pub fn move_task_to_queue(
        self: &Arc<Self>,
        task_id: i64,
        new_queue: &str,
    ) -> anyhow::Result<()> {
        let mut live = self.live.lock().map_err(|_| {
            anyhow::anyhow!("список завдань отруєно; завдання {task_id} не переміщено")
        })?;
        anyhow::ensure!(
            live.contains_key(&task_id),
            "завдання {task_id} є в базі, але відсутнє в пам'яті"
        );
        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
            store.set_task_queue(task_id, new_queue)?;
        }
        if let Some(task) = live.get_mut(&task_id) {
            task.queue = new_queue.to_owned();
        }
        drop(live);
        self.clone().спробувати_наступне();
        self.clone().після_зміни_черги();
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn file(name: &str, selected: bool) -> PlannedFile {
        PlannedFile {
            suggested_name: name.to_owned(),
            size: Some(1),
            selected,
        }
    }

    fn absent_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("dl-e10-absent-{tag}"))
    }

    /// Зразок живого завдання для тестів.
    ///
    /// Поля перелічені **тут одного разу**, а не в кожному тесті: інакше
    /// кожне нове поле `Live` валить збірку в п'яти місцях і тест
    /// перетворюється на список присвоєнь, серед яких не видно, що саме
    /// перевіряється.
    fn зразок(id: i64, url: &str, status: Status) -> Live {
        Live {
            id,
            url: url.into(),
            name: url.into(),
            status,
            done: 0,
            total: None,
            segments: 0,
            error: None,
            variant: None,
            prev_done: 0,
            speed: 0,
            session: Session::default(),
            protocol: "http".into(),
            targets: vec![],
            parts: Vec::new(),
            parallelism: None,
            queue: downloader_core::store::DEFAULT_QUEUE.to_owned(),
            max_duration: None,
            rewind: false,
        }
    }

    fn рушій_зі_завданням(status: Status) -> (Arc<Engine>, i64) {
        let mut store = Store::in_memory().expect("тестова база створюється");
        let id = store
            .add_task(&NewTask {
                url: "https://example.com/file.bin".to_owned(),
                protocol: "test".to_owned(),
                title: None,
                category_id: None,
                variant: None,
                queue: Some(downloader_core::store::DEFAULT_QUEUE.to_owned()),
                files: vec![NewFile {
                    path: PathBuf::from("file.bin"),
                    size: Some(17),
                    fingerprint: None,
                    selected: true,
                }],
            })
            .expect("тестове завдання додається");
        store
            .set_status(id, status, None)
            .expect("початковий стан зберігається");

        let mut task = зразок(id, "https://example.com/file.bin", status);
        task.done = 17;
        task.total = Some(17);
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let engine = Arc::new(Engine {
            store: Mutex::new(store),
            live: Arc::new(Mutex::new(HashMap::from([(id, task)]))),
            events,
            registry: Registry::new(),
            cancels: Mutex::new(HashMap::from([(id, Cancel::new())])),
            downloads_dir: PathBuf::from("."),
            налаштування: Mutex::new(Settings::default()),
            after: Arc::new(AfterQueue::default()),
            вікно_було: std::sync::atomic::AtomicBool::new(true),
            поточний_ліміт: std::sync::atomic::AtomicU64::new(0),
            queue_limiters: Mutex::new(HashMap::new()),
        });
        (engine, id)
    }

    fn отруїти_прапорці(engine: &Arc<Engine>) {
        let engine = Arc::clone(engine);
        let outcome = std::thread::spawn(move || {
            let _guard = engine.cancels.lock().expect("mutex ще цілий");
            panic!("навмисно отруюємо mutex прапорців");
        })
        .join();
        assert!(outcome.is_err(), "тестовий потік мав завершитися панікою");
    }

    #[test]
    fn знайти_живе_бачить_running_і_ігнорує_done() {
        let live = Mutex::new(HashMap::from([
            (1, зразок(1, "https://a", Status::Done)),
            (2, зразок(2, "https://b", Status::Running)),
        ]));
        assert_eq!(
            знайти_живе(&live, "https://b").expect("mutex не отруєно"),
            Some(2)
        );
        assert_eq!(
            знайти_живе(&live, "https://a").expect("mutex не отруєно"),
            None
        );
    }

    #[test]
    fn знайти_живе_не_маскує_отруєний_mutex_як_відсутність() {
        let live = Arc::new(Mutex::new(HashMap::<i64, Live>::new()));
        let для_потоку = Arc::clone(&live);
        let outcome = std::thread::spawn(move || {
            let _guard = для_потоку.lock().expect("mutex ще цілий");
            panic!("навмисно отруюємо mutex");
        })
        .join();
        assert!(outcome.is_err(), "тестовий потік мав завершитися панікою");

        let error = знайти_живе(&live, "https://example.com")
            .expect_err("отруєння має бути помилкою, а не None");
        assert!(error.to_string().contains("дубль URL не перевірено"));
    }

    #[test]
    fn порожня_черга_не_вважає_отруєний_mutex_порожнім() {
        let live = Arc::new(Mutex::new(HashMap::<i64, Live>::new()));
        let для_потоку = Arc::clone(&live);
        let outcome = std::thread::spawn(move || {
            let _guard = для_потоку.lock().expect("mutex ще цілий");
            panic!("навмисно отруюємо mutex");
        })
        .join();
        assert!(outcome.is_err(), "тестовий потік мав завершитися панікою");

        let error =
            черга_порожня_під_замком(&live).expect_err("отруєння має бути помилкою, а не true");
        assert!(error.to_string().contains("порожнечу черги не визначено"));
    }

    #[test]
    fn fail_зберігає_failed_навіть_коли_mutex_прапорців_отруєно() {
        let (engine, id) = рушій_зі_завданням(Status::Running);
        отруїти_прапорці(&engine);

        engine.fail(id, "з'єднання обірвано");

        let live = engine.live.lock().expect("список завдань цілий");
        let task = live.get(&id).expect("завдання є в пам'яті");
        assert_eq!(task.status, Status::Failed);
        assert_eq!(task.error.as_deref(), Some("з'єднання обірвано"));
        drop(live);

        let store = engine.store.lock().expect("сховище ціле");
        let task = store
            .task(id)
            .expect("запит до бази проходить")
            .expect("завдання є в базі");
        assert_eq!(task.status, Status::Failed);
        assert_eq!(task.error.as_deref(), Some("з'єднання обірвано"));
    }

    #[test]
    fn finish_зберігає_done_навіть_коли_mutex_прапорців_отруєно() {
        let (engine, id) = рушій_зі_завданням(Status::Running);
        отруїти_прапорці(&engine);

        assert_eq!(engine.finish(id).expect("завершення не скасоване"), 17);

        let live = engine.live.lock().expect("список завдань цілий");
        assert_eq!(
            live.get(&id).expect("завдання є в пам'яті").status,
            Status::Done
        );
        drop(live);

        let store = engine.store.lock().expect("сховище ціле");
        assert_eq!(
            store
                .task(id)
                .expect("запит до бази проходить")
                .expect("завдання є в базі")
                .status,
            Status::Done
        );
    }

    #[test]
    fn порожня_черга_ігнорує_done_і_paused() {
        let mut live = HashMap::new();
        live.insert(1, зразок(1, "https://a", Status::Done));
        live.insert(2, зразок(2, "https://b", Status::Paused));
        live.insert(3, зразок(3, "https://c", Status::Failed));
        assert!(черга_спорожніла(&live));
        live.insert(4, зразок(4, "https://d", Status::Queued));
        assert!(!черга_спорожніла(&live));
    }

    #[test]
    fn черга_бере_найменший_id() {
        let mut live = HashMap::new();
        live.insert(5, зразок(5, "https://c", Status::Queued));
        live.insert(3, зразок(3, "https://d", Status::Queued));
        live.insert(4, зразок(4, "https://e", Status::Running));
        assert_eq!(id_наступного_в_черзі(&live), Some(3));
    }

    #[test]
    fn без_dest_лише_selected() {
        let dir = absent_dir("downloads");
        let files = vec![
            file("video.ts", true),
            file("audio-en.m3u8", true),
            file("subs-en.vtt", false),
        ];
        let out = paths_for_selected(&files, None, &dir);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, dir.join("video.ts"));
        assert_eq!(out[1].0, dir.join("audio-en.m3u8"));
    }

    #[test]
    fn dest_на_один_файл_йде_як_є() {
        let dir = absent_dir("one");
        let dest = dir.join("готове.bin");
        let files = vec![file("a.bin", true), file("b.bin", false)];
        let out = paths_for_selected(&files, Some(&dest), &dir);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, dest);
    }

    #[test]
    fn dest_на_кілька_перший_це_dest_решта_поруч() {
        let dir = absent_dir("movie-dir");
        let dest = dir.join("movie.ts");
        let files = vec![
            file("720p.ts", true),
            file("audio-en.m3u8", true),
            file("subs-en.vtt", false),
        ];
        let out = paths_for_selected(&files, Some(&dest), &dir);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, dest);
        assert_eq!(out[1].0, dir.join("audio-en.m3u8"));
    }

    #[test]
    fn однакові_suggested_name_розводяться() {
        let dir = absent_dir("dup");
        let files = vec![file("a.bin", true), file("a.bin", true)];
        let out = paths_for_selected(&files, None, &dir);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, dir.join("a.bin"));
        assert_eq!(out[1].0, dir.join("a (1).bin"));
    }

    #[test]
    fn жоден_selected_не_дає_шляхів() {
        let dir = absent_dir("empty");
        let files = vec![file("a.bin", false), file("b.bin", false)];
        let out = paths_for_selected(&files, None, &dir);
        assert!(out.is_empty());
    }

    #[test]
    fn to_view_містить_назву_черги() {
        let mut sample = зразок(42, "https://example.com/test", Status::Queued);
        sample.queue = "nightly".to_owned();
        let view = sample.to_view();
        assert_eq!(view.id, 42);
        assert_eq!(view.queue, "nightly");
    }
}
