//! Проба URL: що це за файл і чи можна різати його на сегменти.
//!
//! Робиться до першого записаного байта, і від її чесності залежить усе
//! наступне. Головне правило: **не вгадувати**. Сервер, який мовчить про
//! `Accept-Ranges`, часто чудово тримає `Range` — і навпаки, сервер, який
//! пише `Accept-Ranges: bytes`, може віддати `200` на реальний запит.
//! Тому останнє слово завжди за пробним запитом, а не за заголовком.

use crate::headers::{ContentRange, RangeSupport, Validator, filename_from_disposition,
    filename_from_url};
use reqwest::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, ETAG, HeaderMap,
    LAST_MODIFIED, RANGE,
};
use reqwest::{Client, StatusCode};

/// Що ми дізнались про ресурс перед качанням.
#[derive(Debug, Clone)]
pub struct Probe {
    /// URL після всіх редиректів — качати треба саме його.
    pub final_url: String,
    /// Розмір файла. `None` — сервер не сказав; качаємо в один потік наосліп.
    pub size: Option<u64>,
    /// Чи справді можна різати на сегменти. Це **результат перевірки**,
    /// а не переказ заголовка.
    pub resumable: bool,
    /// Що заявив сервер у `Accept-Ranges` — для журналу й діагностики.
    pub declared: RangeSupport,
    /// Ознака для `If-Range` при докачуванні.
    pub validator: Validator,
    /// Ім'я саме з `Content-Disposition`, якщо сервер його дав.
    ///
    /// Зберігається окремо від запасного імені з URL навмисно: запасне є
    /// майже завжди, і якби вони жили в одному полі, ім'я із заголовка
    /// ніколи б не перемогло при злитті `HEAD` і пробного `GET`.
    pub disposition_name: Option<String>,
}

impl Probe {
    /// Ім'я, під яким зберігати файл.
    ///
    /// `Content-Disposition` важливіший за URL: у ньому сервер каже, як
    /// файл насправді називається, тоді як у шляху часто стоїть
    /// ідентифікатор на кшталт `/download/8a3f21`.
    #[must_use]
    pub fn filename(&self) -> Option<String> {
        self.disposition_name
            .clone()
            .or_else(|| filename_from_url(&self.final_url))
    }

    /// На скільки сегментів має сенс різати цей ресурс.
    ///
    /// Нуль сегментів не буває: якщо різати не можна, це один потік.
    #[must_use]
    pub fn usable_parts(&self, wanted: usize) -> usize {
        if self.resumable && self.size.is_some_and(|s| s > 0) {
            wanted.max(1)
        } else {
            1
        }
    }
}

/// Помилки проби.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("сервер відповів {status} на {url}")]
    BadStatus { url: String, status: u16 },

    #[error("мережева помилка для {url}: {source}")]
    Network {
        url: String,
        #[source]
        source: reqwest::Error,
    },
}

/// Дізнатись усе, що можна, до першого записаного байта.
///
/// Порядок: `HEAD`, а якщо він не дав відповіді (сервери часто віддають на
/// нього `405`) — `GET` з `Range: bytes=0-0`. Другий шлях кращий тим, що
/// **одночасно доводить підтримку `Range`**: `206` з `Content-Range` — це
/// доказ, а не обіцянка.
pub async fn probe(client: &Client, url: &str) -> Result<Probe, ProbeError> {
    probe_with_retries(client, url, 3).await
}

/// Проба з повторами на тимчасових відмовах.
///
/// ⚠️ Без цього завантаження гине ще до першого байта: файлообмінники
/// звично зустрічають `503` під навантаженням, і сервер, який віддасть файл
/// із третьої спроби, лишився б «недоступним».
pub async fn probe_with_retries(
    client: &Client,
    url: &str,
    attempts: u32,
) -> Result<Probe, ProbeError> {
    let mut last = None;

    for attempt in 0..attempts.max(1) {
        match probe_once(client, url).await {
            Ok(p) => return Ok(p),
            Err(err) if is_temporary(&err) => {
                if attempt + 1 < attempts {
                    let pause = std::time::Duration::from_millis(200 * u64::from(attempt + 1));
                    tokio::time::sleep(pause).await;
                }
                last = Some(err);
            }
            Err(err) => return Err(err),
        }
    }

    Err(last.unwrap_or_else(|| ProbeError::BadStatus {
        url: url.to_owned(),
        status: 0,
    }))
}

/// Чи варто повторити пробу після цієї помилки.
const fn is_temporary(err: &ProbeError) -> bool {
    match err {
        ProbeError::BadStatus { status, .. } => {
            matches!(*status, 408 | 425 | 429 | 500 | 502 | 503 | 504)
        }
        // Обрив на першому ж запиті часто означає перевантажений сервер,
        // а не мертвий URL.
        ProbeError::Network { .. } => true,
    }
}

async fn probe_once(client: &Client, url: &str) -> Result<Probe, ProbeError> {
    if let Some(probe) = try_head(client, url).await? {
        // `HEAD` не доводить `Range` — сервер міг збрехати. Якщо він заявив
        // підтримку, перевіряємо пробним байтом; якщо промовчав — тим паче.
        if probe.declared != RangeSupport::None && probe.size.is_some_and(|s| s > 1) {
            if let Ok(ranged) = try_ranged_get(client, url).await {
                return Ok(merge(probe, ranged));
            }
        }
        return Ok(probe);
    }

    try_ranged_get(client, url).await
}

/// `HEAD`. `None` означає «сервер його не тримає», а не помилку.
async fn try_head(client: &Client, url: &str) -> Result<Option<Probe>, ProbeError> {
    let resp = client
        .head(url)
        .send()
        .await
        .map_err(|source| ProbeError::Network {
            url: url.to_owned(),
            source,
        })?;

    let status = resp.status();
    if status == StatusCode::METHOD_NOT_ALLOWED || status == StatusCode::NOT_IMPLEMENTED {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(ProbeError::BadStatus {
            url: url.to_owned(),
            status: status.as_u16(),
        });
    }

    let final_url = resp.url().to_string();
    let headers = resp.headers().clone();

    Ok(Some(Probe {
        size: content_length(&headers),
        declared: declared_support(&headers),
        resumable: false, // ще не доведено
        validator: validator(&headers),
        disposition_name: disposition_name(&headers),
        final_url,
    }))
}

/// `GET` з `Range: bytes=0-0`. Найчесніший спосіб: відповідь `206` з
/// `Content-Range` одночасно дає і розмір, і доказ підтримки діапазонів.
async fn try_ranged_get(client: &Client, url: &str) -> Result<Probe, ProbeError> {
    let resp = client
        .get(url)
        .header(RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(|source| ProbeError::Network {
            url: url.to_owned(),
            source,
        })?;

    let status = resp.status();
    if !(status.is_success() || status == StatusCode::PARTIAL_CONTENT) {
        return Err(ProbeError::BadStatus {
            url: url.to_owned(),
            status: status.as_u16(),
        });
    }

    let final_url = resp.url().to_string();
    let headers = resp.headers().clone();

    // `206` + зрозумілий `Content-Range` — єдиний доказ, який ми приймаємо.
    let (size, resumable) = if status == StatusCode::PARTIAL_CONTENT {
        match headers
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(ContentRange::parse)
        {
            Some(cr) => (cr.total, true),
            // `206` без розбірливого `Content-Range` — сервер поводиться
            // дивно. Краще один потік, ніж записати байти не туди.
            None => (content_length(&headers), false),
        }
    } else {
        // `200` у відповідь на `Range` означає «діапазони я ігнорую».
        // `Content-Length` тут — розмір усього файла.
        (content_length(&headers), false)
    };

    Ok(Probe {
        size,
        resumable,
        declared: declared_support(&headers),
        validator: validator(&headers),
        disposition_name: disposition_name(&headers),
        final_url,
    })
}

/// Скласти дані `HEAD` і пробного `GET`: беремо найнадійніше з обох.
fn merge(head: Probe, ranged: Probe) -> Probe {
    Probe {
        // Розмір із `Content-Range` точніший за `Content-Length` у `HEAD`.
        size: ranged.size.or(head.size),
        resumable: ranged.resumable,
        declared: head.declared,
        validator: if ranged.validator.is_reliable() {
            ranged.validator
        } else {
            head.validator
        },
        disposition_name: head.disposition_name.or(ranged.disposition_name),
        final_url: ranged.final_url,
    }
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn declared_support(headers: &HeaderMap) -> RangeSupport {
    RangeSupport::parse(headers.get(ACCEPT_RANGES).and_then(|v| v.to_str().ok()))
}

fn validator(headers: &HeaderMap) -> Validator {
    Validator::choose(
        headers.get(ETAG).and_then(|v| v.to_str().ok()),
        headers.get(LAST_MODIFIED).and_then(|v| v.to_str().ok()),
    )
}

/// Ім'я виключно з `Content-Disposition`. Запасне з URL додається пізніше,
/// у [`Probe::filename`], щоб не затерти заголовок при злитті двох проб.
fn disposition_name(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(CONTENT_DISPOSITION)?;
    filename_from_disposition(&decode_header_text(raw.as_bytes()))
}

/// Прочитати заголовок, який може містити не-ASCII байти.
///
/// ⚠️ `HeaderValue::to_str()` тут не годиться: він вимагає видимого ASCII і
/// на кириличному імені повертає помилку. А сирий UTF-8 у
/// `Content-Disposition` шле добра половина реальних серверів — формально
/// це порушення, практично це норма.
///
/// Наслідок мовчазної втрати був би підступний: ім'я не «зламалося б», а
/// підмінилося шматком URL — файл ліг би на диск як `8a3f21` замість
/// `Звіт за 2026 рік.bin`, і ніхто б не зрозумів, коли саме це почалось.
///
/// Тому: спершу пробуємо UTF-8, а якщо не склалось — тлумачимо байти як
/// ISO-8859-1, як і велить стара буква стандарту.
fn decode_header_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => bytes.iter().map(|&b| char::from(b)).collect(),
    }
}
