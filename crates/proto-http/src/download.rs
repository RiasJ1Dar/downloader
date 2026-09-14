//! Сегментоване качання: воркери, докачування, динамічний поділ.
//!
//! Складається з трьох готових частин: [`crate::probe`] каже, що це за
//! ресурс, `SegmentTable` роздає діапазони, `SparseFile` приймає байти.
//! Тут — лише те, що їх поєднує, і крайні випадки, на яких усе ламається.
//!
//! # Правила, які тут не можна порушувати
//!
//! * **`If-Range` на кожному запиті, а не лише на резюме.** Ресурс може
//!   змінитись посеред качання; без цього ми доклеїмо шматок іншої версії.
//! * **Перевіряти межу сегмента перед кожним записом.** Поки воркер качав,
//!   у нього могли вкрасти хвіст — і байти, які він уже тримає в руках,
//!   належать тепер іншому воркеру.
//! * **Кінець тіла до кінця сегмента — це помилка**, а не привід тихо
//!   завершитись. Саме так виглядає сервер, що збрехав у `Content-Length`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use downloader_core::state::{self, DownloadState, LoadError};
use downloader_core::{RateLimiter, SegmentId, SegmentTable, SparseFile};
use futures_util::StreamExt;
use reqwest::header::{IF_RANGE, RANGE};
use reqwest::{Client, StatusCode};

use crate::headers::Validator;
use crate::probe::{Probe, ProbeError, apply_session, probe_with_session};
use downloader_core::protocol::Session;

/// Налаштування качання.
#[derive(Clone)]
pub struct Options {
    /// Скільки з'єднань відкривати на файл.
    pub parts: usize,
    /// Найменший шматок, який має сенс качати окремим з'єднанням.
    pub min_chunk: u64,
    /// Скільки разів повторювати сегмент після обриву.
    pub max_retries: u32,
    /// Стеля швидкості в байтах за секунду. `0` — без обмежень.
    ///
    /// Ділиться на всі з'єднання одного завантаження: людина ставить ліміт
    /// на файл, а не на потік.
    pub rate_limit: u64,
    /// Прапорець зупинки від ядра.
    ///
    /// `None` — качання неперервне (звичайний `dl get`).
    pub cancel: Option<downloader_core::protocol::Cancel>,
    /// Кому повідомляти про поступ.
    ///
    /// Викликається з тіку чекпоінта, а не з гарячого шляху: рушій знає про
    /// байти, але не має знати ні про UI, ні про базу. Аргументи —
    /// завантажено всього, на скільки частин поділено і як саме вони лежать.
    #[allow(clippy::type_complexity)]
    pub on_progress: Option<
        std::sync::Arc<
            dyn Fn(u64, usize, Vec<downloader_core::protocol::PartProgress>) + Send + Sync,
        >,
    >,
    /// Як часто скидати стан на диск.
    ///
    /// Частіше — менше перекачувати після падіння живлення, але більше
    /// `sync` на диск. Дві секунди — компроміс: на 10 МБ/с це щонайбільше
    /// 20 МБ зайвої роботи в найгіршому випадку.
    pub checkpoint_every: std::time::Duration,
    /// Cookies і Referer цього завдання. Порожня сесія — звичайний запит.
    pub session: Session,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            parts: 8,
            // Менше мегабайта різати немає сенсу: виграш з'їдають
            // рукостискання.
            min_chunk: 1 << 20,
            max_retries: 5,
            rate_limit: 0,
            cancel: None,
            on_progress: None,
            checkpoint_every: std::time::Duration::from_secs(2),
            session: Session::default(),
        }
    }
}

/// Чим скінчилось качання.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// Куди лягло.
    pub path: PathBuf,
    /// Скільки байтів записано.
    pub bytes: u64,
    /// Скільки сегментів вийшло разом із украденими.
    pub segments: usize,
    /// Чи зупинено на прохання, не дотягнувши до кінця.
    ///
    /// ⚠️ Це **не помилка**. Людина натиснула паузу; файл і його стан
    /// лишаються на диску, наступний запуск продовжить.
    pub cancelled: bool,
}

/// Помилки качання.
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error(transparent)]
    Probe(#[from] ProbeError),

    #[error(transparent)]
    Core(#[from] downloader_core::Error),

    /// На докачування сервер віддав `200` замість `206`.
    ///
    /// Це або змінений ресурс (`If-Range` не збігся), або сервер, який
    /// узагалі не тримає `Range`. Розрізнити ззовні неможливо, та й не
    /// треба: обидва випадки означають, що тіло приїхало **з початку
    /// файла**, і писати його в поточний зсув не можна — вийде каша.
    #[error(
        "докачування неможливе для {url}: сервер віддав повний файл замість діапазону \
         (ресурс змінився або Range не підтримується)"
    )]
    ResourceChanged { url: String },

    /// Сервер обіцяв більше, ніж віддав.
    #[error(
        "сервер віддав менше, ніж обіцяв: сегмент {segment} чекав ще {missing} байтів від {url}"
    )]
    ShortBody {
        url: String,
        segment: SegmentId,
        missing: u64,
    },

    #[error("сервер відповів {status} на сегмент {segment}")]
    BadStatus { segment: SegmentId, status: u16 },

    #[error("сегмент {segment} обірвався {attempts} разів поспіль: {source}")]
    TooManyRetries {
        segment: SegmentId,
        attempts: u32,
        #[source]
        source: reqwest::Error,
    },

    #[error("внутрішня помилка: {0}")]
    Internal(String),
}

/// Ім'я протоколу у файлі стану.
///
/// Стан, писаний іншим протоколом, ми не читаємо: сегменти торента й
/// сегменти HTTP — різні речі, попри однакову форму.
const PROTOCOL: &str = "http";

/// Спільний стан усіх воркерів одного завантаження.
struct Shared {
    url: String,
    file: SparseFile,
    validator: Validator,
    /// Чи можна докачати з місця обриву.
    ///
    /// Від цього залежить, чи має сенс повторювати короткий відповідь:
    /// там, де `Range` не працює, повтор піде з нуля й нічого не полагодить.
    resumable: bool,
    /// Куди качаємо — потрібно, щоб класти файл стану поруч.
    dest: PathBuf,
    /// Ознака версії ресурсу для файла стану.
    fingerprint: Option<String>,
    /// Спільна на всі з'єднання стеля швидкості.
    limiter: RateLimiter,
    opts: Options,
    /// Таблиця сегментів і черга ще не роздертих.
    ///
    /// Один м'ютекс на обидва навмисно: «взяти завдання» має бути **однією**
    /// неподільною дією, інакше два воркери візьмуть той самий сегмент.
    state: Mutex<State>,
}

impl Shared {
    /// Чи просили зупинитись.
    fn is_cancelled(&self) -> bool {
        self.opts
            .cancel
            .as_ref()
            .is_some_and(downloader_core::protocol::Cancel::is_cancelled)
    }
}

struct State {
    table: SegmentTable,
    /// Сегменти, які ще ніхто не взяв.
    pending: Vec<SegmentId>,
}

/// Завантажити файл за URL.
///
/// Кількість з'єднань — це побажання: якщо сервер не тримає `Range` або не
/// каже розміру, качається в один потік, і це нормальний результат, а не збій.
pub async fn download(
    client: &Client,
    url: &str,
    dest: &Path,
    opts: &Options,
) -> Result<Outcome, DownloadError> {
    let info = probe_with_session(client, url, &opts.session).await?;
    download_with_probe(client, &info, dest, opts).await
}

/// Те саме, коли проба вже зроблена (наприклад, щоб показати людині розмір
/// і дати обрати теку до початку качання).
pub async fn download_with_probe(
    client: &Client,
    info: &Probe,
    dest: &Path,
    opts: &Options,
) -> Result<Outcome, DownloadError> {
    let parts = info.usable_parts(opts.parts);
    let fingerprint = info.validator.if_range_value().map(str::to_owned);

    let table = resume_or_start(info, dest, parts, opts, fingerprint.as_deref());

    let file = SparseFile::create(dest, info.size)?;
    // Незавершені сегменти — у чергу; завершені після відновлення чіпати не
    // треба.
    let pending: Vec<SegmentId> = table
        .segments()
        .iter()
        .filter(|s| !s.is_complete())
        .map(|s| s.id)
        .collect();

    let shared = Arc::new(Shared {
        url: info.final_url.clone(),
        file,
        validator: info.validator.clone(),
        resumable: info.resumable,
        dest: dest.to_path_buf(),
        fingerprint,
        limiter: RateLimiter::new(opts.rate_limit),
        opts: opts.clone(),
        state: Mutex::new(State { table, pending }),
    });

    // Чекпоінт живе окремою задачею: воркери зайняті мережею, і скидати стан
    // між чанками означало б синхронізувати диск на кожні 8 КБ.
    let ticker = tokio::spawn(checkpoint_loop(Arc::clone(&shared)));

    let mut workers = Vec::with_capacity(parts);
    for _ in 0..parts {
        let shared = Arc::clone(&shared);
        let client = client.clone();
        workers.push(tokio::spawn(async move { worker(shared, client).await }));
    }

    let mut failure = None;
    for w in workers {
        match w.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                // Першу помилку лишаємо як причину, решту не затираємо:
                // подальші зриви — це наслідки, а не діагноз.
                failure.get_or_insert(e);
            }
            Err(join) => {
                failure.get_or_insert(DownloadError::Internal(format!(
                    "воркер зірвався: {join}"
                )));
            }
        }
    }

    // ⚠️ `abort()` лише **просить** задачу зупинитись — вона завершиться на
    // найближчій точці очікування. Без `await` чекпоінт міг устигнути
    // записати файл стану вже після того, як `finish` його прибрав, і поруч
    // із готовим файлом лишався `.dlpart`. Саме на цьому моргав тест.
    ticker.abort();
    drop(ticker.await);

    if let Some(err) = failure {
        // Качання не вдалося — але те, що вже на диску, має вціліти разом зі
        // станом: наступна спроба продовжить, а не почне з нуля.
        checkpoint(&shared);
        return Err(err);
    }

    finish(&shared, info)
}

/// Відновити таблицю з файла стану або почати з чистого аркуша.
///
/// Кожна причина відмови від відновлення потрапляє в журнал. «Просто почалось
/// спочатку» — найгірше, що може побачити людина, яка чекала три години.
fn resume_or_start(
    info: &Probe,
    dest: &Path,
    parts: usize,
    opts: &Options,
    fingerprint: Option<&str>,
) -> SegmentTable {
    let fresh = || match info.size {
        Some(total) if info.resumable => SegmentTable::new(total, parts, opts.min_chunk),
        // Розмір невідомий — качаємо суцільно, кінець визначить сам сервер.
        Some(total) => SegmentTable::single(total),
        None => SegmentTable::single(u64::MAX),
    };

    if !info.resumable {
        return fresh();
    }

    match state::load(dest) {
        Ok(saved) => {
            if let Err(why) = saved.accepts(PROTOCOL, &info.final_url, info.size, fingerprint) {
                tracing::info!(%why, "стан не підійшов, качаємо з нуля");
                return fresh();
            }
            let already = saved.downloaded();
            match saved.into_table() {
                Ok(table) => {
                    tracing::info!(already, "докачуємо з файла стану");
                    table
                }
                Err(why) => {
                    tracing::warn!(%why, "стан пошкоджено, качаємо з нуля");
                    fresh()
                }
            }
        }
        Err(LoadError::Absent) => fresh(),
        Err(why) => {
            tracing::warn!(%why, "файл стану непридатний, качаємо з нуля");
            fresh()
        }
    }
}

/// Періодично скидати стан на диск.
async fn checkpoint_loop(shared: Arc<Shared>) {
    let mut tick = tokio::time::interval(shared.opts.checkpoint_every);
    // Перший тик спрацьовує негайно — пропускаємо, качати ще нічого.
    tick.tick().await;

    loop {
        tick.tick().await;
        checkpoint(&shared);
    }
}

/// Один чекпоінт: **спершу дані на диск, потім стан**.
///
/// ⚠️ Порядок не міняти. Якщо записати стан раніше за `sync`, після падіння
/// живлення стан казатиме «завантажено», а байтів не буде — і докачування
/// піде з дірки, якої ніхто не помітить.
fn checkpoint(shared: &Arc<Shared>) {
    // Про поступ повідомляємо завжди — навіть коли стан на диск не пишемо
    // (сервер без `Range`): людина однаково має бачити рух.
    if let Some(report) = &shared.opts.on_progress
        && let Ok(state) = shared.state.lock()
    {
        let done = state.table.downloaded();
        let segments = state.table.len();

        // Розкладку збираємо тут, під тим самим локом, що й решту чисел:
        // інакше смужка показувала б межі з одного моменту, а прогрес — з
        // іншого, і на очах у людини вони б не сходились.
        let parts: Vec<downloader_core::protocol::PartProgress> = state
            .table
            .segments()
            .iter()
            .map(|s| downloader_core::protocol::PartProgress {
                start: s.start,
                end: s.end,
                done: s.done,
            })
            .collect();

        drop(state);
        report(done, segments, parts);
    }

    if !shared.resumable {
        // Докачати все одно не вийде — файл стану лише збивав би з пантелику.
        return;
    }

    if let Err(e) = shared.file.sync() {
        tracing::warn!(error = %e, "не вдалося скинути дані на диск — стан не оновлюю");
        return;
    }

    let Ok(state) = shared.state.lock() else {
        tracing::error!("стан завантаження отруєно — чекпоінт пропущено");
        return;
    };

    let snapshot = DownloadState::from_table(
        PROTOCOL,
        &shared.url,
        &state.table,
        shared.fingerprint.clone(),
    );
    drop(state);

    if let Err(e) = state::save(&shared.dest, &snapshot) {
        tracing::warn!(error = %e, "не вдалося зберегти стан завантаження");
    }
}

/// Один воркер: бере завдання, доки вони є.
async fn worker(shared: Arc<Shared>, client: Client) -> Result<(), DownloadError> {
    while let Some(id) = take_job(&shared) {
        fetch_segment(&shared, &client, id).await?;
    }
    Ok(())
}

/// Взяти наступне завдання: спершу з черги, а коли вона порожня — **вкрасти**
/// половину залишку в найповільнішого.
///
/// Саме тут живе перевага над звичайною качалкою: воркер, що звільнився, не
/// стоїть без діла, поки останній тягне свій хвіст.
fn take_job(shared: &Arc<Shared>) -> Option<SegmentId> {
    // Зупинили — нових завдань не роздаємо. Ті, що вже в руках, воркери
    // завершать самі, побачивши прапорець.
    if shared.is_cancelled() {
        return None;
    }

    let mut state = shared.state.lock().ok()?;

    if let Some(id) = state.pending.pop() {
        return Some(id);
    }

    let min_chunk = shared.opts.min_chunk;
    state.table.steal(min_chunk)
}

/// Чи має сенс повторювати після цієї помилки.
///
/// Тимчасове — обрив, `503`, `429` — повторюємо. Зміна ресурсу або відмова
/// доступу не полагодяться від того, що ми спитаємо ще раз.
///
/// ⚠️ `resumable` тут вирішальний. Коротке тіло від сервера, який **не
/// тримає `Range`**, повторювати марно: наступний запит принесе файл із
/// початку, і замість чесного «сервер віддав менше, ніж обіцяв» людина
/// побачить туманне «докачування неможливе». Причину треба називати першу,
/// а не ту, на яку ми наштовхнулись, поки борсались.
const fn is_retryable(err: &DownloadError, resumable: bool) -> bool {
    match err {
        DownloadError::TooManyRetries { .. } => true,
        DownloadError::ShortBody { .. } => resumable,
        DownloadError::BadStatus { status, .. } => {
            matches!(*status, 408 | 425 | 429 | 500 | 502 | 503 | 504)
        }
        _ => false,
    }
}

/// Дотягнути один сегмент до кінця, переживаючи обриви.
///
/// ⚠️ Лічильник спроб скидається **лише тоді, коли прогрес справді зрушив**.
/// Без цього сервер, який щоразу віддає той самий шматок і рве з'єднання,
/// крутив би нас вічно: кожна спроба «частково успішна», а файл не росте.
async fn fetch_segment(
    shared: &Arc<Shared>,
    client: &Client,
    id: SegmentId,
) -> Result<(), DownloadError> {
    let mut attempt = 0u32;

    loop {
        let Some(seg) = snapshot(shared, id) else {
            // Сегмента вже немає — таке можливе лише при помилці логіки.
            return Err(DownloadError::Internal(format!(
                "сегмент {id} зник із таблиці"
            )));
        };
        if seg.is_complete() {
            return Ok(());
        }

        if shared.is_cancelled() {
            return Ok(());
        }

        let before = seg.done;

        match pull(shared, client, id, seg.cursor(), seg.end).await {
            Ok(()) => return Ok(()),

            Err(err) if is_retryable(&err, shared.resumable) => {
                let moved = snapshot(shared, id).is_some_and(|s| s.done > before);
                if moved {
                    // Щось таки завантажилось — даємо повний запас спроб знову.
                    attempt = 0;
                } else {
                    attempt += 1;
                }

                if attempt >= shared.opts.max_retries {
                    return Err(err);
                }

                let pause = std::time::Duration::from_millis(200 * u64::from(attempt + 1));
                tokio::time::sleep(pause).await;
            }

            Err(other) => return Err(other),
        }
    }
}

/// Один мережевий підхід до сегмента.
async fn pull(
    shared: &Arc<Shared>,
    client: &Client,
    id: SegmentId,
    from: u64,
    to: u64,
) -> Result<(), DownloadError> {
    let mut req = apply_session(client.get(&shared.url), &shared.opts.session);

    // Межа в HTTP включна, тому `to - 1`. Нескінченний хвіст (`to` = MAX)
    // просимо відкритим діапазоном.
    if to == u64::MAX {
        req = req.header(RANGE, format!("bytes={from}-"));
    } else {
        req = req.header(RANGE, format!("bytes={from}-{}", to - 1));
    }

    // `If-Range` на **кожному** запиті: ресурс міг змінитись і посеред
    // качання, не лише між сесіями.
    if let Some(v) = shared.validator.if_range_value() {
        req = req.header(IF_RANGE, v);
    }

    let resp = req
        .send()
        .await
        .map_err(|source| DownloadError::TooManyRetries {
            segment: id,
            attempts: 1,
            source,
        })?;

    let status = resp.status();

    // `200` там, де ми просили діапазон, означає одне з двох: сервер
    // ігнорує `Range` (тоді ми качаємо з нуля одним сегментом), або ресурс
    // змінився і `If-Range` не збігся. Друге — привід зупинитись.
    if status == StatusCode::OK && from > 0 {
        return Err(DownloadError::ResourceChanged {
            url: shared.url.clone(),
        });
    }
    if !(status.is_success() || status == StatusCode::PARTIAL_CONTENT) {
        return Err(DownloadError::BadStatus {
            segment: id,
            status: status.as_u16(),
        });
    }

    let mut offset = from;
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,

            // Обрив посеред тіла. Що з ним робити — залежить від того, чи
            // можна докачати:
            //
            // * `Range` працює → це тимчасова біда, повторимо з поточного
            //   зсуву, і все вже завантажене лишиться на диску;
            // * `Range` не працює → докачати нізвідки, наступний запит
            //   принесе файл із початку. Тоді це остаточно **короткий
            //   файл**, і сказати треба саме так. Інакше рушій піде на друге
            //   коло, отримає `200` і поскаржиться на нього — а справжня
            //   причина (сервер обіцяв більше, ніж віддав) загубиться.
            Err(source) => {
                if shared.resumable {
                    return Err(DownloadError::TooManyRetries {
                        segment: id,
                        attempts: 1,
                        source,
                    });
                }

                let missing = snapshot(shared, id).map_or(0, |s| s.end.saturating_sub(offset));
                return Err(DownloadError::ShortBody {
                    url: shared.url.clone(),
                    segment: id,
                    missing,
                });
            }
        };

        // ⚠️ Межа могла змінитись, поки ми чекали байти: інший воркер міг
        // украсти хвіст цього сегмента. Пишемо тільки те, що досі наше.
        let Some(seg) = snapshot(shared, id) else {
            return Ok(());
        };
        if offset >= seg.end {
            return Ok(());
        }

        let room = seg.end - offset;
        let take = usize::try_from(room)
            .unwrap_or(usize::MAX)
            .min(chunk.len());
        let piece = &chunk[..take];

        if piece.is_empty() {
            return Ok(());
        }

        // Ліміт швидкості. Шматок може піти на диск не цілком за раз:
        // відро віддає стільки, скільки має, і каже, коли спитати ще.
        //
        // ⚠️ Записуємо саме те, що дозволено, і одразу — не накопичуючи в
        // пам'яті. Інакше при жорсткому ліміті буфери росли б швидше, ніж
        // спорожнялись.
        // Зупинка перевіряється між чанками: це найчастіша точка, і вона
        // дешева — одне атомарне читання.
        if shared.is_cancelled() {
            return Ok(());
        }

        let mut left = piece;
        while !left.is_empty() {
            // Ліміт швидкості питаємо **поза локом**: усередині є пауза, а
            // спати з захопленим м'ютексом означало б зупинити всіх воркерів
            // разом із собою.
            let allowance = shared.limiter.take(left.len() as u64);

            if allowance.allowed == 0 {
                if let Some(pause) = allowance.wait {
                    tokio::time::sleep(pause).await;
                }
                continue;
            }

            let want = usize::try_from(allowance.allowed)
                .unwrap_or(usize::MAX)
                .min(left.len());

            // ⚠️ Звірка межі, запис і просування — **під одним локом**.
            //
            // Спокуса розділити їх велика: подивитись межу, відпустити лок,
            // спокійно записати. Саме так і було — і саме так виникала
            // гонка: між звіркою і `advance` інший воркер устигав украсти
            // хвіст цього сегмента, і ми писали в чужий діапазон.
            // `SegmentOverrun` це ловив, але вже після зіпсованого файла.
            //
            // Лок тут тримається на час одного системного виклику в
            // сторінковий кеш — без очікування диска (`sync` окремо) і без
            // жодного `await`. Це десятки мікросекунд, і воно того варте.
            let written = {
                let mut state = shared
                    .state
                    .lock()
                    .map_err(|_| DownloadError::Internal("стан завантаження отруєно".into()))?;

                let Some(seg_now) = state.table.get(id) else {
                    return Ok(());
                };
                if offset >= seg_now.end {
                    return Ok(());
                }

                let room = usize::try_from(seg_now.end - offset).unwrap_or(usize::MAX);
                let n = want.min(room);
                if n == 0 {
                    return Ok(());
                }

                shared.file.write_all_at(offset, &left[..n])?;
                state.table.advance(id, n as u64)?;
                n
            };

            offset += written as u64;
            left = &left[written..];
        }

        // Межу перечитуємо: `seg` вище зчитано до циклу, і за цей час хвіст
        // могли вкрасти. Зі старим значенням ми зробили б зайвий оберт по
        // стріму замість того, щоб чесно завершитись.
        if snapshot(shared, id).is_none_or(|s| offset >= s.end) {
            return Ok(());
        }
    }

    // Тіло скінчилось, а сегмент — ні. Це не «просто кінець»: так виглядає
    // сервер, який збрехав у `Content-Length`. Мовчки прийняти — значить
    // віддати людині файл із діркою.
    let missing = snapshot(shared, id).map_or(0, |s| s.end.saturating_sub(offset));
    if missing > 0 && to != u64::MAX {
        return Err(DownloadError::ShortBody {
            url: shared.url.clone(),
            segment: id,
            missing,
        });
    }

    Ok(())
}

/// Копія стану сегмента під м'ютексом, який одразу відпускається.
fn snapshot(shared: &Arc<Shared>, id: SegmentId) -> Option<downloader_core::Segment> {
    shared.state.lock().ok()?.table.get(id)
}

/// Перевірити результат і закрити файл.
fn finish(shared: &Arc<Shared>, info: &Probe) -> Result<Outcome, DownloadError> {
    let state = shared
        .state
        .lock()
        .map_err(|_| DownloadError::Internal("стан завантаження отруєно".into()))?;

    let bytes = state.table.downloaded();
    let segments = state.table.len();
    let cancelled = shared.is_cancelled();

    shared.file.sync()?;

    if cancelled {
        // Зупинено на прохання: файл і стан лишаються як є, щоб наступний
        // запуск продовжив. Обрізати файл тут було б катастрофою —
        // зарезервоване місце зникло б разом із можливістю докачати.
        drop(state);
        checkpoint(shared);

        return Ok(Outcome {
            path: shared.file.path().to_path_buf(),
            bytes,
            segments,
            cancelled: true,
        });
    }

    // Розмір був невідомий — файл створювався без резервування, обрізати
    // нема чого. Інакше вкорочуємо до фактично завантаженого: сервер міг
    // заявити більше, ніж віддав.
    if info.size.is_some() {
        shared.file.truncate_to(bytes)?;
        shared.file.sync()?;
    }

    // Завантаження завершене — файл стану більше не потрібен і лише
    // спантеличував би людину, лежачи поруч із готовим файлом.
    if let Err(e) = state::remove(&shared.dest) {
        tracing::warn!(error = %e, "не вдалося прибрати файл стану");
    }

    Ok(Outcome {
        path: shared.file.path().to_path_buf(),
        bytes,
        segments,
        cancelled: false,
    })
}
