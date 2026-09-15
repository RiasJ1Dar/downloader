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
    Cancel, PlannedFile, Progress, ProgressSink, RateLimitSupport, Registry, RunContext,
    Session,
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
        if let Err(e) = store.seed_default_categories(&downloads_dir) {
            tracing::warn!(error = %e, "типові категорії не записались");
        }
        let (events, _) = broadcast::channel(EVENT_BUFFER);

        tracing::info!(модулі = ?registry.names(), "реєстр протоколів");

        let mut stored = store.settings().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "налаштування не прочитано, беру типові");
            Settings::default()
        });
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
            let done = targets.first().map_or(done, |p| {
                downloader_core::state::load(p)
                    .map(|s| s.downloaded())
                    .unwrap_or(done)
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
        self.налаштування
            .lock()
            .map(|s| usize::try_from(s.max_concurrent).unwrap_or(1).max(1))
            .unwrap_or(3)
    }

    fn вікно_відкрите(&self) -> bool {
        let now = хвилини_зараз();
        self.налаштування
            .lock()
            .map(|s| s.downloads_allowed(now))
            .unwrap_or(true)
    }

    /// Поточні правила для вікна й CLI.
    pub fn settings(&self) -> Settings {
        self.налаштування
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    /// Змінити правила ядра й записати їх у базу.
    pub fn configure(self: &Arc<Self>, patch: SettingsPatch) -> anyhow::Result<()> {
        let mut next = self.settings();
        next.apply_patch(patch)?;

        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
            store.save_settings(&next)?;
        }
        if let Ok(mut g) = self.налаштування.lock() {
            *g = next.clone();
        }
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
        _parts: Option<usize>,
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

        if let Some(id) = знайти_живе(&self.live, url) {
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

        let id = {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;

            store.add_task(&NewTask {
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
            })?
        };

        let name = targets
            .first()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| names::ЗАПАСНЕ_ІМʼЯ.to_owned());

        let start_now = {
            let Ok(live) = self.live.lock() else {
                anyhow::bail!("список завдань отруєно");
            };
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

        if let Ok(mut live) = self.live.lock() {
            live.insert(
                id,
                Live {
                    id,
                    url: probed.final_url.clone(),
                    name,
                    status: if start_now {
                        Status::Running
                    } else {
                        Status::Queued
                    },
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
                    queue: actual_queue.clone(),
                    max_duration,
                    rewind,
                },
            );
        }

        if start_now {
            self.set_status(id, Status::Running, None);
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
            );
        } else {
            self.set_status(id, Status::Queued, None);
            if self.вікно_відкрите() {
                tracing::info!(id, "завдання в черзі (стеля {n} одночасних)", n = self.стеля());
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
            if let Ok(mut c) = self.cancels.lock() {
                c.insert(id, cancel.clone());
            }

            let queue_rate_limit = {
                let store = self.store.lock().ok();
                store.and_then(|s| s.queue(&queue_name).ok().flatten()).map(|q| q.rate_limit).unwrap_or(0)
            };

            let limiter = if queue_rate_limit > 0 {
                let mut limiters = self.queue_limiters.lock().ok();
                if let Some(ref mut map) = limiters {
                    let entry = map.entry(queue_name.clone()).or_insert_with(|| {
                        Arc::new(downloader_core::rate::RateLimiter::new(queue_rate_limit))
                    });
                    Some(entry.clone())
                } else {
                    None
                }
            } else {
                None
            };

            let global_limit = self
                .налаштування
                .lock()
                .map(|s| s.effective_rate(хвилини_зараз()))
                .unwrap_or(0);
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

            match module.run(ctx.clone(), &sink).await {
                // Непорожня відповідь означає «зупинено на прохання»: це не
                // завершення й не помилка.
                Ok(Some(_resume)) => {
                    self.pause_finished(id);
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

                    let bytes = self.finish(id);
                    if let Err(e) = self.зафіксувати_перевірку(id, bytes) {
                        self.fail(id, &e.to_string());
                        return;
                    }
                    let path = ctx
                        .targets
                        .first()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    let _ = self.events.send(Event::Finished { id, path, bytes });
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
            let Ok(store) = self.store.lock() else {
                return;
            };
            store.queues().unwrap_or_default()
        };

        loop {
            let job = {
                let Ok(mut live) = self.live.lock() else {
                    return;
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
                    .filter(|t| t.status == Status::Queued && eligible_queues.contains(t.queue.as_str()))
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
            ) = job;
            self.set_status(id, Status::Running, None);
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

        if let Ok(mut live) = self.live.lock()
            && let Some(task) = live.get_mut(&id)
        {
            task.status = Status::Queued;
            task.error = None;
            task.protocol = protocol;
            task.targets = targets;
            task.url = url;
        }

        self.set_status(id, Status::Queued, None);
        self.clone().спробувати_наступне();
        self.clone().після_зміни_черги();
        Ok(())
    }

    /// Модуль спинився на прохання — перевести завдання в паузу.
    fn pause_finished(&self, id: i64) {
        if let Ok(mut live) = self.live.lock()
            && let Some(task) = live.get_mut(&id)
        {
            task.status = Status::Paused;
            task.speed = 0;
        }

        if let Ok(mut c) = self.cancels.lock() {
            c.remove(&id);
        }

        self.set_status(id, Status::Paused, None);
        tracing::info!(id, "завдання зупинено на прохання");
    }

    /// Позначити завдання завершеним і повернути, скільки завантажено.
    fn finish(&self, id: i64) -> u64 {
        let bytes = if let Ok(mut live) = self.live.lock() {
            live.get_mut(&id).map_or(0, |task| {
                task.status = Status::Done;
                task.speed = 0;
                if task.total.is_none() {
                    task.total = Some(task.done);
                }
                task.done
            })
        } else {
            0
        };

        if let Ok(mut c) = self.cancels.lock() {
            c.remove(&id);
        }

        self.set_status(id, Status::Done, None);
        bytes
    }

    /// Позначити завдання невдалим.
    fn fail(&self, id: i64, message: &str) {
        tracing::warn!(id, error = %message, "завдання впало");

        if let Ok(mut live) = self.live.lock()
            && let Some(task) = live.get_mut(&id)
        {
            task.status = Status::Failed;
            task.error = Some(message.to_owned());
            task.speed = 0;
        }

        if let Ok(mut c) = self.cancels.lock() {
            c.remove(&id);
        }

        self.set_status(id, Status::Failed, Some(message));
        let _ = self.events.send(Event::Failed {
            id,
            message: message.to_owned(),
        });
    }

    /// Прибрати завдання зі списку.
    pub fn remove(self: &Arc<Self>, id: i64, with_file: bool) -> anyhow::Result<()> {
        let path = {
            let store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
            store.files(id)?.first().map(|f| f.path.clone())
        };

        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
            store.remove_task(id)?;
        }

        if let Ok(mut live) = self.live.lock() {
            live.remove(&id);
        }

        if with_file && let Some(p) = path {
            // Разом із файлом прибираємо і його стан: інакше наступне
            // завантаження того самого посилання спробує «докачати» те,
            // чого вже немає.
            let _ = std::fs::remove_file(&p);
            let _ = downloader_core::state::remove(&p);
        }

        self.clone().після_зміни_черги();
        Ok(())
    }

    /// Записати стан у базу, не валячи ядро через помилку сховища.
    fn set_status(&self, id: i64, status: Status, error: Option<&str>) {
        let Ok(mut store) = self.store.lock() else {
            tracing::error!("сховище отруєно, стан не збережено");
            return;
        };

        if let Err(e) = store.set_status(id, status, error) {
            tracing::warn!(id, error = %e, "не вдалося зберегти стан завдання");
        }
    }

    /// Оновити швидкість живих завдань.
    fn refresh_progress(&self) {
        let Ok(mut live) = self.live.lock() else {
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
                    let has_queued = self
                        .live
                        .lock()
                        .map(|l| l.values().any(|t| t.status == Status::Queued))
                        .unwrap_or(false);
                    if has_queued {
                        self.clone().спробувати_наступне();
                    }
                }

                let limit = self
                    .налаштування
                    .lock()
                    .map(|s| s.effective_rate(хвилини_зараз()))
                    .unwrap_or(0);
                let prev_limit = self.поточний_ліміт.swap(limit, Ordering::Relaxed);
                if limit != prev_limit {
                    self.registry.set_rate_limit(limit);
                }

                // Нема кому слухати — нема чого й складати знімок.
                if self.events.receiver_count() == 0 {
                    continue;
                }

                let _ = self.events.send(Event::Snapshot {
                    tasks: self.list(),
                });
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
fn знайти_живе(live: &Mutex<HashMap<i64, Live>>, url: &str) -> Option<i64> {
    let Ok(guard) = live.lock() else {
        return None;
    };
    guard.values().find_map(|t| {
        if t.url == url
            && matches!(
                t.status,
                Status::Running | Status::Queued | Status::Paused
            )
        {
            Some(t.id)
        } else {
            None
        }
    })
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
        self.live
            .lock()
            .map(|g| черга_спорожніла(&g))
            .unwrap_or(true)
    }

    fn після_зміни_черги(self: Arc<Self>) {
        let idle = self.черга_порожня();
        let mut action = self.settings().post_action;
        if action == downloader_core::post_action::PostAction::None
            && let Ok(store) = self.store.lock()
            && let Ok(queues) = store.queues()
        {
            for q in queues {
                if q.post_action != downloader_core::post_action::PostAction::None {
                    action = q.post_action;
                    break;
                }
            }
        }
        let me = Arc::clone(&self);
        self.after
            .notify(idle, action, move || me.черга_порожня());
    }

    /// Отримати список усіх черг з лічильниками завдань.
    pub fn queues(&self) -> Vec<downloader_ipc::protocol::QueueView> {
        let rows = {
            let Ok(store) = self.store.lock() else {
                return Vec::new();
            };
            store.queues().unwrap_or_default()
        };
        let live = match self.live.lock() {
            Ok(l) => l,
            Err(_) => return Vec::new(),
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
        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
            store.update_queue(name, &patch)?;
        }
        if let Some(new_rate) = patch.rate_limit
            && let Ok(mut limiters) = self.queue_limiters.lock()
        {
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
        let running_ids: Vec<i64> = {
            let live = self
                .live
                .lock()
                .map_err(|_| anyhow::anyhow!("список завдань отруєно"))?;
            live.values()
                .filter(|t| t.queue == name && t.status == Status::Running)
                .map(|t| t.id)
                .collect()
        };
        for id in running_ids {
            let _ = self.pause(id);
        }
        self.clone().після_зміни_черги();
        Ok(())
    }

    /// Відновити роботу черги: зняти паузу й перевести її paused-завдання в queued.
    pub fn resume_queue(self: &Arc<Self>, name: &str) -> anyhow::Result<()> {
        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
            store.update_queue(
                name,
                &downloader_core::store::QueuePatch {
                    paused: Some(false),
                    ..Default::default()
                },
            )?;
            let mut live = self
                .live
                .lock()
                .map_err(|_| anyhow::anyhow!("список завдань отруєно"))?;
            for task in live.values_mut() {
                if task.queue == name && task.status == Status::Paused {
                    task.status = Status::Queued;
                    task.error = None;
                    let _ = store.set_status(task.id, Status::Queued, None);
                }
            }
        }
        self.clone().спробувати_наступне();
        self.clone().після_зміни_черги();
        Ok(())
    }

    /// Перейменувати чергу.
    pub fn rename_queue(&self, old_name: &str, new_name: &str) -> anyhow::Result<()> {
        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
            store.rename_queue(old_name, new_name)?;
        }
        if let Ok(mut live) = self.live.lock() {
            for task in live.values_mut() {
                if task.queue == old_name {
                    task.queue = new_name.to_owned();
                }
            }
        }
        if let Ok(mut limiters) = self.queue_limiters.lock()
            && let Some(lim) = limiters.remove(old_name)
        {
            limiters.insert(new_name.to_owned(), lim);
        }
        Ok(())
    }

    /// Видалити чергу (перевівши її завдання в default).
    pub fn delete_queue(self: &Arc<Self>, name: &str) -> anyhow::Result<()> {
        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
            store.delete_queue(name)?;
        }
        if let Ok(mut live) = self.live.lock() {
            for task in live.values_mut() {
                if task.queue == name {
                    task.queue = downloader_core::store::DEFAULT_QUEUE.to_owned();
                }
            }
        }
        if let Ok(mut limiters) = self.queue_limiters.lock() {
            limiters.remove(name);
        }
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
        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;
            store.set_task_queue(task_id, new_queue)?;
        }
        if let Ok(mut live) = self.live.lock()
            && let Some(task) = live.get_mut(&task_id)
        {
            task.queue = new_queue.to_owned();
        }
        self.clone().спробувати_наступне();
        self.clone().після_зміни_черги();
        Ok(())
    }
}

#[cfg(test)]
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
            queue: downloader_core::store::DEFAULT_QUEUE.to_owned(),
            max_duration: None,
            rewind: false,
        }
    }

    #[test]
    fn знайти_живе_бачить_running_і_ігнорує_done() {
        let live = Mutex::new(HashMap::from([
            (
                1,
                зразок(1, "https://a", Status::Done),
            ),
            (
                2,
                зразок(2, "https://b", Status::Running),
            ),
        ]));
        assert_eq!(знайти_живе(&live, "https://b"), Some(2));
        assert_eq!(знайти_живе(&live, "https://a"), None);
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
        live.insert(
            5,
            зразок(5, "https://c", Status::Queued),
        );
        live.insert(
            3,
            зразок(3, "https://d", Status::Queued),
        );
        live.insert(
            4,
            зразок(4, "https://e", Status::Running),
        );
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
