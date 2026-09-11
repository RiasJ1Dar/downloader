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
use std::sync::{Arc, Mutex};

use downloader_core::protocol::{
    Cancel, PlannedFile, Progress, ProgressSink, Registry, RunContext,
};
use downloader_core::store::{NewFile, NewTask, Status, Store};
use downloader_ipc::protocol::{Event, TaskView};
use downloader_winutil::{motw, names, paths};
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

        let store = Store::open(db)?;
        let (events, _) = broadcast::channel(EVENT_BUFFER);

        tracing::info!(модулі = ?registry.names(), "реєстр протоколів");

        let engine = Arc::new(Self {
            store: Mutex::new(store),
            live: Arc::new(Mutex::new(HashMap::new())),
            events,
            registry,
            cancels: Mutex::new(HashMap::new()),
            downloads_dir,
        });

        engine.clone().start_ticker();
        Ok(engine)
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

    /// Додати завантаження й одразу почати його.
    pub async fn add(
        self: &Arc<Self>,
        url: &str,
        dest: Option<PathBuf>,
        _parts: Option<usize>,
    ) -> anyhow::Result<i64> {
        // Хто це качатиме, вирішує реєстр, а не ядро. Саме тут і живе межа.
        let Some(protocol) = self.registry.find(url) else {
            anyhow::bail!(
                "жоден модуль не впізнав це посилання: {url}. Доступні: {:?}",
                self.registry.names()
            );
        };

        let protocol_name = protocol.name().to_owned();

        // Проба до запису в базу: інакше в списку з'явиться завдання, про яке
        // нічого не відомо — ні розміру, ні імені, ні чи воно взагалі існує.
        let probed = protocol.probe(url).await?;

        if probed.files.is_empty() {
            anyhow::bail!("модуль {protocol_name} не назвав жодного файла для {url}");
        }

        let planned = paths_for_selected(&probed.files, dest.as_deref(), &self.downloads_dir);
        if planned.is_empty() {
            anyhow::bail!("модуль {protocol_name} не обрав жодного файла для {url}");
        }

        let targets: Vec<PathBuf> = planned.iter().map(|(p, _)| p.clone()).collect();

        let id = {
            let mut store = self
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("сховище отруєно"))?;

            store.add_task(&NewTask {
                url: probed.final_url.clone(),
                protocol: protocol_name.clone(),
                title: None,
                category_id: None,
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

        if let Ok(mut live) = self.live.lock() {
            live.insert(
                id,
                Live {
                    id,
                    url: probed.final_url.clone(),
                    name,
                    status: Status::Running,
                    done: 0,
                    total: probed.total_size,
                    segments: 0,
                    error: None,
                    prev_done: 0,
                    speed: 0,
                },
            );
        }

        self.set_status(id, Status::Running, None);
        self.clone()
            .spawn_download(id, protocol_name, probed.final_url, targets);
        Ok(id)
    }

    /// Запустити качання окремою задачею.
    fn spawn_download(
        self: Arc<Self>,
        id: i64,
        protocol: String,
        source: String,
        targets: Vec<PathBuf>,
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

            let ctx = RunContext {
                task_id: id,
                source,
                targets,
                resume: None,
                cancel,
            };

            match module.run(ctx.clone(), &sink).await {
                // Непорожня відповідь означає «зупинено на прохання»: це не
                // завершення й не помилка.
                Ok(Some(_resume)) => {
                    self.pause_finished(id);
                }

                Ok(None) => {
                    if let Err(e) = module.verify(&ctx).await {
                        self.fail(id, &e.to_string());
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
                    let path = ctx
                        .targets
                        .first()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default();
                    let _ = self.events.send(Event::Finished { id, path, bytes });
                }

                Err(e) => self.fail(id, &e.to_string()),
            }
        });
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
            task.status = Status::Running;
            task.error = None;
        }

        self.set_status(id, Status::Running, None);
        self.clone().spawn_download(id, protocol, url, targets);
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
    pub fn remove(&self, id: i64, with_file: bool) -> anyhow::Result<()> {
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

    /// Раз на тік розсилати знімок списку.
    fn start_ticker(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(TICK);
            loop {
                tick.tick().await;
                self.refresh_progress();

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
}
