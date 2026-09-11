//! Сценарії. Кожен шлях — окрема пастка для рушія завантажень.

use crate::ServerState;
use crate::body::{BodyGen, TINY_BODY, body_bytes, parse_count, parse_size, path_segments};
use crate::request::{Request, read_request};
use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use std::io::Write as _;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;

/// Дати фіксовані навмисно: тест, що звіряє байти відповіді, не має падати
/// від того, що годинник цокнув.
const DATE: &str = "Thu, 01 Jan 2026 00:00:00 GMT";
const LAST_MODIFIED: &str = "Wed, 31 Dec 2025 23:00:00 GMT";

/// Порція запису в сокет.
const CHUNK: usize = 64 * 1024;

/// Одне з'єднання — один запит. Keep-alive свідомо не підтримуємо: усі
/// відповіді йдуть із `Connection: close`. Це прибирає цілий клас неоднозначно
/// стей (де саме закінчилось тіло, якщо `Content-Length` збрехав) і робить
/// обрив у `/cut` однозначним для клієнта.
pub(crate) async fn handle_connection(stream: TcpStream, state: Arc<ServerState>) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut reader = BufReader::new(rd);
    let req = read_request(&mut reader).await?;
    let outcome = route(&mut wr, &req, &state).await;
    // Reader тримаємо живим до кінця: якщо дропнути його раніше, ядро може
    // відповісти RST на дані, що ще летять від клієнта, і знищити вже
    // надіслане нами тіло.
    drop(reader);
    outcome
}

/// Розбір шляху й вибір сценарію.
async fn route(wr: &mut OwnedWriteHalf, req: &Request, state: &ServerState) -> Result<()> {
    let segs = path_segments(&req.target);
    let s: Vec<&str> = segs.iter().map(String::as_str).collect();
    // Ключ лічильника — сценарій без хвостового імені файлу, щоб
    // `/flaky/1k/2` і `/flaky/1k/2/file.bin` рахувались разом.
    let key = format!("/{}", segs.join("/"));

    match s.as_slice() {
        ["plain", size] => {
            let total = parse_size(size)?;
            serve_honest(wr, req, total, &format!("\"plain-{total}\"")).await
        }
        ["norange", size] => serve_norange(wr, req, parse_size(size)?).await,
        ["cut", size, at] => serve_cut(wr, req, parse_size(size)?, parse_size(at)?).await,
        ["flaky", size, n] => {
            let (total, fails) = (parse_size(size)?, parse_count(n)?);
            if state.bump(&key) <= fails {
                serve_status(wr, 503, &[("Retry-After", "1")], "flaky: ще ні".as_bytes()).await
            } else {
                serve_honest(wr, req, total, &format!("\"flaky-{total}\"")).await
            }
        }
        ["changing", size] => {
            let total = parse_size(size)?;
            // ETag змінюється на КОЖЕН запит — рушій із If-Range мусить
            // отримати 200 замість 206 і зрозуміти, що докачувати нікуди.
            let etag = format!("\"changing-{}\"", state.bump(&key));
            serve_honest(wr, req, total, &etag).await
        }
        ["liar-length", size, claimed] => {
            serve_liar_length(wr, req, parse_size(size)?, parse_size(claimed)?).await
        }
        ["gzip", size] => serve_gzip(wr, req, parse_size(size)?).await,
        ["slow", size, bps] => serve_slow(wr, req, parse_size(size)?, parse_size(bps)?).await,
        // Чесний Range + дросель: єдиний сценарій, у якому тест устигає
        // перервати качання й перевірити докачування.
        ["slow-range", size, bps] => {
            let total = parse_size(size)?;
            let bps = parse_size(bps)?;
            if bps == 0 {
                bail!("/slow-range: швидкість 0 байт/с — сценарій ніколи не завершиться");
            }
            let portion = (bps / 10).max(1);
            serve_honest_throttled(
                wr,
                req,
                total,
                &format!("\"slow-range-{total}\""),
                Some((portion, Duration::from_millis(100))),
            )
            .await
        }
        ["redirect", n, size] => serve_redirect(wr, req, parse_count(n)?, size).await,
        ["auth", size] => serve_auth(wr, req, parse_size(size)?).await,
        ["disposition", case] => serve_disposition(wr, req, case).await,
        ["head-only", size] => {
            let total = parse_size(size)?;
            serve_honest(wr, req, total, &format!("\"head-only-{total}\"")).await
        }
        ["no-head", size] => {
            let total = parse_size(size)?;
            if req.is_head() {
                // Рушій має вміти відкотитись на `GET Range: bytes=0-0`.
                return serve_status(
                    wr,
                    405,
                    &[("Allow", "GET")],
                    "HEAD тут заборонений".as_bytes(),
                )
                .await;
            }
            serve_honest(wr, req, total, &format!("\"no-head-{total}\"")).await
        }
        ["hls", "vod"] => serve_hls_vod(wr, req).await,
        ["hls", "live"] => serve_hls_live(wr, req, state).await,
        ["hls", "range"] => serve_hls_range(wr, req).await,
        ["hls", "aes"] => serve_hls_aes(wr, req).await,
        ["hls", "media"] => serve_hls_media(wr, req).await,
        ["hls", "drm"] => serve_hls_drm(wr, req).await,
        // `seg-init` / `seg-0` без крапки не відкидаються `path_segments`.
        ["dash", "vod"] | ["dash", "vod", _] => serve_dash_vod(wr, req).await,
        _ => {
            // Гучно: мовчазна 404 у тестовому стенді — це години пошуку
            // «чому рушій качає 9 байтів».
            let msg = format!(
                "невідомий сценарій: target={:?}, сегменти={:?}. \
                 Доступні: /plain /norange /cut /flaky /changing /liar-length \
                 /gzip /slow /slow-range /redirect /auth /disposition /head-only /no-head \
                 /hls/vod /hls/live /hls/media /hls/drm /dash/vod",
                req.target, segs
            );
            tracing::warn!("{msg}");
            serve_status(wr, 404, &[], msg.as_bytes()).await
        }
    }
}

// ── Сценарій 1, 5, 12: чесна віддача ────────────────────────────────────────

/// Чесний сервер: `Accept-Ranges: bytes`, коректні 206 з `Content-Range`,
/// 416 на незадовільний діапазон, повага до `If-Range`.
///
/// Використовується і як еталон (`/plain`), і як «нормальна» гілка інших
/// сценаріїв — різниця лише в `ETag`.
async fn serve_honest(
    wr: &mut OwnedWriteHalf,
    req: &Request,
    total: u64,
    etag: &str,
) -> Result<()> {
    serve_honest_throttled(wr, req, total, etag, None).await
}

/// Те саме, але з можливістю віддавати по краплині.
///
/// Потрібно там, де тест має **встигнути втрутитись** посеред качання:
/// перервати завантаження, перевірити файл стану, зміряти швидкість. На
/// localhost чесний сервер віддає мегабайти швидше, ніж тест устигає
/// прокинутись.
async fn serve_honest_throttled(
    wr: &mut OwnedWriteHalf,
    req: &Request,
    total: u64,
    etag: &str,
    throttle: Option<(u64, Duration)>,
) -> Result<()> {
    // `If-Range` із чужим ETag означає «ресурс міг змінитись» — за RFC 9110
    // сервер зобов'язаний віддати 200 і все тіло. Саме на цьому ловиться
    // /changing.
    let if_range_ok = match req.header("if-range") {
        Some(v) => v.trim() == etag,
        None => true,
    };

    let range = req.range().filter(|_| if_range_ok);

    if let Some(spec) = range {
        let Some((start, end)) = spec.resolve(total) else {
            return serve_status(
                wr,
                416,
                &[("Content-Range", &format!("bytes */{total}"))],
                "діапазон поза межами тіла".as_bytes(),
            )
            .await;
        };
        let len = end - start + 1;
        let head = build_head(
            206,
            &[
                ("Accept-Ranges", "bytes"),
                ("Content-Type", "application/octet-stream"),
                ("Content-Length", &len.to_string()),
                ("Content-Range", &format!("bytes {start}-{end}/{total}")),
                ("ETag", etag),
                ("Last-Modified", LAST_MODIFIED),
            ],
        );
        wr.write_all(&head).await.context("запис заголовків 206")?;
        if !req.is_head() {
            let mut bg = BodyGen::new(total);
            bg.skip(start);
            write_body(wr, &mut bg, len, throttle).await?;
        }
        return wr.flush().await.context("flush 206");
    }

    let head = build_head(
        200,
        &[
            ("Accept-Ranges", "bytes"),
            ("Content-Type", "application/octet-stream"),
            ("Content-Length", &total.to_string()),
            ("ETag", etag),
            ("Last-Modified", LAST_MODIFIED),
        ],
    );
    wr.write_all(&head).await.context("запис заголовків 200")?;
    if !req.is_head() {
        let mut bg = BodyGen::new(total);
        write_body(wr, &mut bg, total, throttle).await?;
    }
    wr.flush().await.context("flush 200")
}

// ── Сценарій 2: ігнорує Range ───────────────────────────────────────────────

/// Завжди 200 і повне тіло, хай би що просив клієнт. `Accept-Ranges: none`.
/// Рушій має відкотитись в один потік і не намагатись докачувати.
async fn serve_norange(wr: &mut OwnedWriteHalf, req: &Request, total: u64) -> Result<()> {
    let head = build_head(
        200,
        &[
            ("Accept-Ranges", "none"),
            ("Content-Type", "application/octet-stream"),
            ("Content-Length", &total.to_string()),
            ("ETag", &format!("\"norange-{total}\"")),
            ("Last-Modified", LAST_MODIFIED),
        ],
    );
    wr.write_all(&head)
        .await
        .context("запис заголовків norange")?;
    if !req.is_head() {
        let mut bg = BodyGen::new(total);
        write_body(wr, &mut bg, total, None).await?;
    }
    wr.flush().await.context("flush norange")
}

// ── Сценарій 3: обрив посеред тіла ──────────────────────────────────────────

/// Обіцяє `<size>`, віддає `<at>` байтів і закриває з'єднання.
///
/// Рвемо через FIN (звичайне закриття), а не RST із `SO_LINGER=0`: RST на
/// частині платформ викидає вже доставлені байти з приймального буфера, і тест
/// побачив би нуль замість `<at>`. FIN посеред недобраного `Content-Length` —
/// для клієнта така сама несподівана обірваність.
async fn serve_cut(wr: &mut OwnedWriteHalf, req: &Request, total: u64, at: u64) -> Result<()> {
    let head = build_head(
        200,
        &[
            ("Accept-Ranges", "bytes"),
            ("Content-Type", "application/octet-stream"),
            ("Content-Length", &total.to_string()),
            ("ETag", &format!("\"cut-{total}\"")),
            ("Last-Modified", LAST_MODIFIED),
        ],
    );
    wr.write_all(&head).await.context("запис заголовків cut")?;
    if !req.is_head() {
        let mut bg = BodyGen::new(total);
        write_body(wr, &mut bg, at.min(total), None).await?;
    }
    wr.flush().await.context("flush cut")?;
    // Далі — тиша: `wr` дропається разом зі з'єднанням, клієнт бачить EOF
    // задовго до обіцяного Content-Length.
    Ok(())
}

// ── Сценарій 6: брехливий Content-Length ────────────────────────────────────

/// У `Content-Length` пише `<claimed>`, на дріт кладе `<size>`.
/// Працює в обидва боки: `claimed > size` — клієнт чекає й ловить EOF;
/// `claimed < size` — клієнт вважає файл готовим, недорахувавши хвіст.
async fn serve_liar_length(
    wr: &mut OwnedWriteHalf,
    req: &Request,
    total: u64,
    claimed: u64,
) -> Result<()> {
    let head = build_head(
        200,
        &[
            ("Accept-Ranges", "bytes"),
            ("Content-Type", "application/octet-stream"),
            ("Content-Length", &claimed.to_string()),
            ("ETag", &format!("\"liar-{total}-{claimed}\"")),
            ("Last-Modified", LAST_MODIFIED),
        ],
    );
    wr.write_all(&head)
        .await
        .context("запис заголовків liar-length")?;
    if !req.is_head() {
        let mut bg = BodyGen::new(total);
        write_body(wr, &mut bg, total, None).await?;
    }
    wr.flush().await.context("flush liar-length")
}

// ── Сценарій 7: gzip ────────────────────────────────────────────────────────

/// `Content-Encoding: gzip`, а `Content-Length` — розмір **стисненого** тіла.
///
/// За RFC це абсолютно чесно: `Content-Length` описує те, що на дроті. Пастка
/// в тому, що на диск лягає розпаковане, і наївний лічильник «завантажено /
/// усього» не сходиться, а перевірка розміру файла хибно кричить про биті дані.
///
/// ⚠️ Тіло сценаріїв псевдовипадкове, тобто **нестисне**: gzip на ньому не
/// економить, а додає близько двох десятків службових байтів. Тому тут
/// заявлена довжина трохи **більша** за розпаковану, а не менша, як звикли
/// бачити на текстах. Для рушія різниця та сама: `Content-Length` і розмір
/// файла на диску не збігаються.
async fn serve_gzip(wr: &mut OwnedWriteHalf, req: &Request, total: u64) -> Result<()> {
    let raw = body_bytes(total);
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&raw).context("gzip: стиснення тіла")?;
    let packed = enc.finish().context("gzip: закриття потоку")?;

    let head = build_head(
        200,
        &[
            // Range на стиснутому потоці — окрема банка з павуками,
            // свідомо вимикаємо.
            ("Accept-Ranges", "none"),
            ("Content-Type", "application/octet-stream"),
            ("Content-Encoding", "gzip"),
            ("Content-Length", &packed.len().to_string()),
            ("ETag", &format!("\"gzip-{total}\"")),
            ("Last-Modified", LAST_MODIFIED),
        ],
    );
    wr.write_all(&head).await.context("запис заголовків gzip")?;
    if !req.is_head() {
        wr.write_all(&packed).await.context("запис тіла gzip")?;
    }
    wr.flush().await.context("flush gzip")
}

// ── Сценарій 8: повільна віддача ────────────────────────────────────────────

/// Не швидше `<bps>` байтів за секунду. Порція = десята частина ліміту,
/// пауза 100 мс — достатньо дрібно, щоб ETA й лічильник швидкості встигли
/// показати щось осмислене.
async fn serve_slow(wr: &mut OwnedWriteHalf, req: &Request, total: u64, bps: u64) -> Result<()> {
    if bps == 0 {
        bail!("/slow: швидкість 0 байт/с — такий сценарій ніколи не завершиться");
    }
    let head = build_head(
        200,
        &[
            ("Accept-Ranges", "bytes"),
            ("Content-Type", "application/octet-stream"),
            ("Content-Length", &total.to_string()),
            ("ETag", &format!("\"slow-{total}\"")),
            ("Last-Modified", LAST_MODIFIED),
        ],
    );
    wr.write_all(&head).await.context("запис заголовків slow")?;
    if !req.is_head() {
        let mut bg = BodyGen::new(total);
        let portion = (bps / 10).max(1);
        write_body(
            wr,
            &mut bg,
            total,
            Some((portion, Duration::from_millis(100))),
        )
        .await?;
    }
    wr.flush().await.context("flush slow")
}

// ── Сценарій 9: ланцюг редиректів ───────────────────────────────────────────

/// `n > 1` → на `/redirect/{n-1}/{size}`; `n == 1` → на `/plain/{size}`
/// (той самий сервер, **інший** шлях); `n == 0` → віддає файл.
/// Отже, `/redirect/3/1m` — це рівно 3 відповіді 302, четверта з файлом.
async fn serve_redirect(wr: &mut OwnedWriteHalf, req: &Request, n: u64, size: &str) -> Result<()> {
    let total = parse_size(size)?;
    if n == 0 {
        return serve_honest(wr, req, total, &format!("\"redirect-{total}\"")).await;
    }
    let target = if n == 1 {
        format!("/plain/{size}")
    } else {
        format!("/redirect/{}/{size}", n - 1)
    };
    // Абсолютний Location, якщо клієнт прислав Host — так поводиться
    // більшість реальних серверів, і саме це треба вміти розбирати.
    let location = match req.header("host") {
        Some(host) => format!("http://{host}{target}"),
        None => target,
    };
    serve_status(wr, 302, &[("Location", &location)], "redirect".as_bytes()).await
}

// ── Сценарій 10: заголовки з браузера ───────────────────────────────────────

/// Без `Cookie: session=ok` **і** непорожнього `Referer` — 403.
/// Перевіряє, що заголовки, взяті з браузера, доїжджають до сервера
/// у кожному запиті, зокрема в повторах і в паралельних потоках.
async fn serve_auth(wr: &mut OwnedWriteHalf, req: &Request, total: u64) -> Result<()> {
    let cookie_ok = req
        .header("cookie")
        .is_some_and(|c| c.split(';').any(|kv| kv.trim() == "session=ok"));
    let referer_ok = req.header("referer").is_some_and(|r| !r.trim().is_empty());

    if !(cookie_ok && referer_ok) {
        let msg = format!(
            "403: cookie session=ok {}, Referer {}",
            if cookie_ok { "є" } else { "НЕМА" },
            if referer_ok { "є" } else { "НЕМА" }
        );
        return serve_status(wr, 403, &[], msg.as_bytes()).await;
    }
    serve_honest(wr, req, total, &format!("\"auth-{total}\"")).await
}

// ── Сценарій 11: небезпечні імена файлів ────────────────────────────────────

/// Значення `Content-Disposition` для кожного кейса.
/// Це корм для майбутньої санітизації імен — усе, на чому вона має не впасти
/// і не вийти за теку завантажень.
fn disposition_value(case: &str) -> Option<String> {
    let long = "a".repeat(300);
    Some(match case {
        // Кирилиця сирими UTF-8 байтами — формально порушення (заголовок мав
        // би бути ISO-8859-1), але так робить половина реальних серверів.
        "cyrillic" => "attachment; filename=\"Звіт за 2026 рік.bin\"".to_string(),
        "spaces" => "attachment; filename=\"my report file.bin\"".to_string(),
        // Зарезервоване ім'я пристрою у Windows: створити такий файл не можна.
        "reserved" => "attachment; filename=\"CON.txt\"".to_string(),
        // Кінцева крапка: Windows її мовчки з'їдає — ім'я на диску інше.
        "trailing-dot" => "attachment; filename=\"report.bin.\"".to_string(),
        // Вихід із теки — найнебезпечніший кейс.
        "traversal" => "attachment; filename=\"..\\..\\evil.exe\"".to_string(),
        "long" => format!("attachment; filename=\"{long}.bin\""),
        "rfc5987" => "attachment; filename=\"report.bin\"; \
             filename*=UTF-8''%D0%97%D0%B2%D1%96%D1%82.bin"
            .to_string(),
        // Заголовка немає взагалі — ім'я треба брати з URL.
        "none" => return None,
        _ => return None,
    })
}

/// Маленьке тіло (64 байти) з небезпечним `Content-Disposition`.
async fn serve_disposition(wr: &mut OwnedWriteHalf, req: &Request, case: &str) -> Result<()> {
    let known = [
        "cyrillic",
        "spaces",
        "reserved",
        "trailing-dot",
        "traversal",
        "long",
        "rfc5987",
        "none",
    ];
    if !known.contains(&case) {
        let msg = format!("/disposition: невідомий кейс {case:?}, є лише {known:?}");
        tracing::warn!("{msg}");
        return serve_status(wr, 404, &[], msg.as_bytes()).await;
    }

    let total = TINY_BODY;
    let len_s = total.to_string();
    let disposition = disposition_value(case);
    let mut headers: Vec<(&str, &str)> = vec![
        ("Accept-Ranges", "bytes"),
        ("Content-Type", "application/octet-stream"),
        ("Content-Length", len_s.as_str()),
        ("ETag", "\"disposition\""),
    ];
    if let Some(v) = disposition.as_deref() {
        headers.push(("Content-Disposition", v));
    }
    let head = build_head(200, &headers);
    wr.write_all(&head)
        .await
        .context("запис заголовків disposition")?;
    if !req.is_head() {
        let mut bg = BodyGen::new(total);
        write_body(wr, &mut bg, total, None).await?;
    }
    wr.flush().await.context("flush disposition")
}

// ── Спільні цеглинки ────────────────────────────────────────────────────────

const HLS_VOD_MASTER: &str = "\
#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\n\
media360.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720\n\
media.m3u8\n";

const HLS_VOD_MEDIA360: &str = "\
#EXTM3U\n\
#EXT-X-VERSION:3\n\
#EXT-X-TARGETDURATION:4\n\
#EXT-X-MEDIA-SEQUENCE:1\n\
#EXTINF:4.0,\n\
seg360.ts\n\
#EXT-X-ENDLIST\n";

const HLS_SEG360: &[u8] = b"LOW-QUALITY-PAYLOAD-360P-AAAA";

const HLS_VOD_MEDIA: &str = "\
#EXTM3U\n\
#EXT-X-VERSION:3\n\
#EXT-X-TARGETDURATION:4\n\
#EXT-X-MEDIA-SEQUENCE:1\n\
#EXTINF:4.0,\n\
seg0.ts\n\
#EXTINF:4.0,\n\
seg1.ts\n\
#EXT-X-ENDLIST\n";

/// Два сегменти з відомими байтами: тест звіряє SHA-256 склейки, не довжину.
const HLS_SEG0: &[u8] = b"SEG0-PAYLOAD-AAAAAAAAAAAAAAAA";
const HLS_SEG1: &[u8] = b"SEG1-PAYLOAD-BBBBBBBBBBBBBBBB";

fn hls_імʼя(req: &Request) -> &str {
    let path = req.target.split('?').next().unwrap_or(req.target.as_str());
    path.rsplit('/').next().unwrap_or("")
}

async fn serve_hls_vod(wr: &mut OwnedWriteHalf, req: &Request) -> Result<()> {
    match hls_імʼя(req) {
        "master.m3u8" => {
            serve_bytes(
                wr,
                req,
                "application/vnd.apple.mpegurl",
                HLS_VOD_MASTER.as_bytes(),
            )
            .await
        }
        "media.m3u8" => {
            serve_bytes(
                wr,
                req,
                "application/vnd.apple.mpegurl",
                HLS_VOD_MEDIA.as_bytes(),
            )
            .await
        }
        "media360.m3u8" => {
            serve_bytes(
                wr,
                req,
                "application/vnd.apple.mpegurl",
                HLS_VOD_MEDIA360.as_bytes(),
            )
            .await
        }
        "seg0.ts" => serve_bytes(wr, req, "video/mp2t", HLS_SEG0).await,
        "seg1.ts" => serve_bytes(wr, req, "video/mp2t", HLS_SEG1).await,
        "seg360.ts" => serve_bytes(wr, req, "video/mp2t", HLS_SEG360).await,
        other => {
            let msg = format!("невідомий HLS vod файл: {other}");
            serve_status(wr, 404, &[], msg.as_bytes()).await
        }
    }
}

const HLS_RANGE_MEDIA: &str = "\
#EXTM3U\n\
#EXT-X-VERSION:4\n\
#EXT-X-TARGETDURATION:4\n\
#EXT-X-MEDIA-SEQUENCE:1\n\
#EXTINF:4.0,\n\
#EXT-X-BYTERANGE:29@0\n\
blob.ts\n\
#EXTINF:4.0,\n\
#EXT-X-BYTERANGE:29@29\n\
blob.ts\n\
#EXT-X-ENDLIST\n";

const HLS_RANGE_BLOB: &[u8] = b"SEG0-PAYLOAD-AAAAAAAAAAAAAAAASEG1-PAYLOAD-BBBBBBBBBBBBBBBB";

const HLS_AES_KEY: &[u8] = b"0123456789abcdef";

const HLS_AES_MEDIA: &str = "\
#EXTM3U\n\
#EXT-X-VERSION:3\n\
#EXT-X-TARGETDURATION:4\n\
#EXT-X-MEDIA-SEQUENCE:1\n\
#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n\
#EXTINF:4.0,\n\
seg0.ts\n\
#EXTINF:4.0,\n\
seg1.ts\n\
#EXT-X-ENDLIST\n";

fn aes128_cbc_encrypt(plain: &[u8], sequence: u64) -> Result<Vec<u8>> {
    use aes::Aes128;
    use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
    type Enc = cbc::Encryptor<Aes128>;
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&sequence.to_be_bytes());
    let mut buf = vec![0u8; plain.len() + 16];
    buf[..plain.len()].copy_from_slice(plain);
    let enc = Enc::new_from_slices(HLS_AES_KEY, &iv)
        .map_err(|e| anyhow::anyhow!("AES: {e}"))?;
    let n = plain.len();
    let out = enc
        .encrypt_padded_mut::<Pkcs7>(&mut buf, n)
        .map_err(|_| anyhow::anyhow!("AES pad"))?;
    Ok(out.to_vec())
}

async fn serve_hls_range(wr: &mut OwnedWriteHalf, req: &Request) -> Result<()> {
    match hls_імʼя(req) {
        "media.m3u8" => {
            serve_bytes(
                wr,
                req,
                "application/vnd.apple.mpegurl",
                HLS_RANGE_MEDIA.as_bytes(),
            )
            .await
        }
        "blob.ts" => serve_bytes(wr, req, "video/mp2t", HLS_RANGE_BLOB).await,
        other => {
            let msg = format!("невідомий HLS range файл: {other}");
            serve_status(wr, 404, &[], msg.as_bytes()).await
        }
    }
}

async fn serve_hls_aes(wr: &mut OwnedWriteHalf, req: &Request) -> Result<()> {
    match hls_імʼя(req) {
        "media.m3u8" => {
            serve_bytes(
                wr,
                req,
                "application/vnd.apple.mpegurl",
                HLS_AES_MEDIA.as_bytes(),
            )
            .await
        }
        "key.bin" => serve_bytes(wr, req, "application/octet-stream", HLS_AES_KEY).await,
        "seg0.ts" => {
            let body = aes128_cbc_encrypt(HLS_SEG0, 1)?;
            serve_bytes(wr, req, "video/mp2t", &body).await
        }
        "seg1.ts" => {
            let body = aes128_cbc_encrypt(HLS_SEG1, 2)?;
            serve_bytes(wr, req, "video/mp2t", &body).await
        }
        other => {
            let msg = format!("невідомий HLS aes файл: {other}");
            serve_status(wr, 404, &[], msg.as_bytes()).await
        }
    }
}

/// Live з ковзним вікном: хто качає лише «поточний» плейлист на ENDLIST —
/// втратить seg0. Рушій має забирати сегменти щойно вони з'явились.
fn live_media_body(tick: u8) -> String {
    match tick {
        1 => "\
#EXTM3U\n\
#EXT-X-VERSION:3\n\
#EXT-X-TARGETDURATION:1\n\
#EXT-X-MEDIA-SEQUENCE:100\n\
#EXTINF:1.0,\n\
seg0.ts\n"
            .to_owned(),
        2 => "\
#EXTM3U\n\
#EXT-X-VERSION:3\n\
#EXT-X-TARGETDURATION:1\n\
#EXT-X-MEDIA-SEQUENCE:101\n\
#EXTINF:1.0,\n\
seg1.ts\n"
            .to_owned(),
        _ => "\
#EXTM3U\n\
#EXT-X-VERSION:3\n\
#EXT-X-TARGETDURATION:1\n\
#EXT-X-MEDIA-SEQUENCE:101\n\
#EXTINF:1.0,\n\
seg1.ts\n\
#EXT-X-ENDLIST\n"
            .to_owned(),
    }
}

async fn serve_hls_live(
    wr: &mut OwnedWriteHalf,
    req: &Request,
    state: &ServerState,
) -> Result<()> {
    match hls_імʼя(req) {
        "media.m3u8" => {
            let body = live_media_body(state.live_tick());
            serve_bytes(
                wr,
                req,
                "application/vnd.apple.mpegurl",
                body.as_bytes(),
            )
            .await
        }
        "seg0.ts" => serve_bytes(wr, req, "video/mp2t", HLS_SEG0).await,
        "seg1.ts" => serve_bytes(wr, req, "video/mp2t", HLS_SEG1).await,
        other => {
            let msg = format!("невідомий HLS live файл: {other}");
            serve_status(wr, 404, &[], msg.as_bytes()).await
        }
    }
}

/// Master з окремими AUDIO / SUBTITLES. Відео — той самий VOD-медіа
/// (`HLS_VOD_MEDIA` / `seg0.ts`). Наступний цикл proto-hls, цей їх не чіпає.
const HLS_MEDIA_MASTER: &str = "\
#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"uk\",LANGUAGE=\"uk\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"uk\",LANGUAGE=\"uk\",URI=\"subs.vtt\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720,AUDIO=\"aac\",SUBTITLES=\"subs\"\n\
media.m3u8\n";

const HLS_MEDIA_AUDIO: &str = "\
#EXTM3U\n\
#EXT-X-VERSION:3\n\
#EXT-X-TARGETDURATION:4\n\
#EXT-X-MEDIA-SEQUENCE:1\n\
#EXTINF:4.0,\n\
audio.ts\n\
#EXT-X-ENDLIST\n";

const HLS_AUDIO_SEG: &[u8] = b"AUDIO-TRACK-PAYLOAD";

const HLS_MEDIA_SUBS: &str = "\
WEBVTT\n\
\n\
00:00:00.000 --> 00:00:04.000\n\
українські субтитри\n";

/// FairPlay SAMPLE-AES: URI — `skd://`, ключ файлом не віддаємо.
const HLS_DRM_MEDIA: &str = "\
#EXTM3U\n\
#EXT-X-VERSION:3\n\
#EXT-X-TARGETDURATION:4\n\
#EXT-X-MEDIA-SEQUENCE:1\n\
#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://fairplay\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n\
#EXTINF:4.0,\n\
seg0.ts\n\
#EXT-X-ENDLIST\n";

async fn serve_hls_media(wr: &mut OwnedWriteHalf, req: &Request) -> Result<()> {
    match hls_імʼя(req) {
        "master.m3u8" => {
            serve_bytes(
                wr,
                req,
                "application/vnd.apple.mpegurl",
                HLS_MEDIA_MASTER.as_bytes(),
            )
            .await
        }
        "media.m3u8" => {
            serve_bytes(
                wr,
                req,
                "application/vnd.apple.mpegurl",
                HLS_VOD_MEDIA.as_bytes(),
            )
            .await
        }
        "audio.m3u8" => {
            serve_bytes(
                wr,
                req,
                "application/vnd.apple.mpegurl",
                HLS_MEDIA_AUDIO.as_bytes(),
            )
            .await
        }
        "subs.vtt" => serve_bytes(wr, req, "text/vtt", HLS_MEDIA_SUBS.as_bytes()).await,
        "audio.ts" => serve_bytes(wr, req, "video/mp2t", HLS_AUDIO_SEG).await,
        "seg0.ts" => serve_bytes(wr, req, "video/mp2t", HLS_SEG0).await,
        "seg1.ts" => serve_bytes(wr, req, "video/mp2t", HLS_SEG1).await,
        other => {
            let msg = format!("невідомий HLS media файл: {other}");
            serve_status(wr, 404, &[], msg.as_bytes()).await
        }
    }
}

const DASH_VOD_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT8S" minBufferTime="PT1S" profiles="urn:mpeg:dash:profile:isoff-on-demand:2011">
  <Period duration="PT8S">
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <Representation id="360" bandwidth="800000" width="640" height="360">
        <SegmentList timescale="1" duration="4">
          <Initialization sourceURL="seg-init"/>
          <SegmentURL media="seg-0"/>
          <SegmentURL media="seg-1"/>
        </SegmentList>
      </Representation>
      <Representation id="720" bandwidth="2000000" width="1280" height="720">
        <SegmentList timescale="1" duration="4">
          <Initialization sourceURL="seg-init"/>
          <SegmentURL media="seg-0"/>
          <SegmentURL media="seg-1"/>
        </SegmentList>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>
"#;

const DASH_DRM_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" xmlns:cenc="urn:mpeg:cenc:2013" type="static" mediaPresentationDuration="PT4S" minBufferTime="PT1S" profiles="urn:mpeg:dash:profile:isoff-on-demand:2011">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed" value="Widevine"/>
      <Representation id="1" bandwidth="1000000" height="720">
        <SegmentList timescale="1" duration="4">
          <Initialization sourceURL="seg-init"/>
          <SegmentURL media="seg-0"/>
        </SegmentList>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>
"#;

const DASH_LIVE_MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="dynamic" minimumUpdatePeriod="PT5S" minBufferTime="PT1S" profiles="urn:mpeg:dash:profile:isoff-live:2011">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <Representation id="1" bandwidth="1000000" height="720">
        <SegmentTemplate media="live-$Number$.m4s" timescale="1" duration="2" startNumber="1"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>
"#;

const DASH_INIT: &[u8] = b"INIT-PAYLOAD-DASH-AAAAAAAAAA";

async fn serve_dash_vod(wr: &mut OwnedWriteHalf, req: &Request) -> Result<()> {
    match hls_імʼя(req) {
        "manifest.mpd" => {
            serve_bytes(wr, req, "application/dash+xml", DASH_VOD_MPD.as_bytes()).await
        }
        "drm.mpd" => serve_bytes(wr, req, "application/dash+xml", DASH_DRM_MPD.as_bytes()).await,
        "live.mpd" => serve_bytes(wr, req, "application/dash+xml", DASH_LIVE_MPD.as_bytes()).await,
        "seg-init" => serve_bytes(wr, req, "video/mp4", DASH_INIT).await,
        "seg-0" => serve_bytes(wr, req, "video/mp4", HLS_SEG0).await,
        "seg-1" => serve_bytes(wr, req, "video/mp4", HLS_SEG1).await,
        other => {
            let msg = format!("невідомий DASH vod файл: {other}");
            serve_status(wr, 404, &[], msg.as_bytes()).await
        }
    }
}

async fn serve_hls_drm(wr: &mut OwnedWriteHalf, req: &Request) -> Result<()> {
    match hls_імʼя(req) {
        "media.m3u8" => {
            serve_bytes(
                wr,
                req,
                "application/vnd.apple.mpegurl",
                HLS_DRM_MEDIA.as_bytes(),
            )
            .await
        }
        "seg0.ts" => serve_bytes(wr, req, "video/mp2t", HLS_SEG0).await,
        other => {
            let msg = format!("невідомий HLS drm файл: {other}");
            serve_status(wr, 404, &[], msg.as_bytes()).await
        }
    }
}

/// Віддати фіксоване тіло без Range. Для коротких маніфестів і сегментів.
async fn serve_bytes(
    wr: &mut OwnedWriteHalf,
    req: &Request,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let len = body.len().to_string();
    let head = build_head(
        200,
        &[
            ("Content-Type", content_type),
            ("Content-Length", &len),
            ("Accept-Ranges", "bytes"),
        ],
    );
    wr.write_all(&head)
        .await
        .context("запис заголовків сирих байтів")?;
    if !req.is_head() {
        wr.write_all(body)
            .await
            .context("запис сирого тіла")?;
    }
    wr.flush().await.context("flush сирих байтів")
}

/// Коротка відповідь зі статусом і текстовим тілом.
async fn serve_status(
    wr: &mut OwnedWriteHalf,
    code: u16,
    extra: &[(&str, &str)],
    body: &[u8],
) -> Result<()> {
    let len = body.len().to_string();
    let mut headers: Vec<(&str, &str)> = vec![
        ("Content-Type", "text/plain; charset=utf-8"),
        ("Content-Length", &len),
    ];
    headers.extend_from_slice(extra);
    let head = build_head(code, &headers);
    wr.write_all(&head)
        .await
        .context("запис заголовків статусу")?;
    wr.write_all(body).await.context("запис тіла статусу")?;
    wr.flush().await.context("flush статусу")
}

/// Складання статусного рядка й заголовків. `Connection: close` і `Date`
/// додаються завжди.
fn build_head(code: u16, headers: &[(&str, &str)]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {code} {}\r\n", reason(code));
    out.push_str(&format!("Date: {DATE}\r\n"));
    out.push_str("Connection: close\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    out.into_bytes()
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        206 => "Partial Content",
        302 => "Found",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        416 => "Range Not Satisfiable",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

/// Віддає `len` байтів із генератора. `throttle` — (порція, пауза після неї).
async fn write_body(
    wr: &mut OwnedWriteHalf,
    bg: &mut BodyGen,
    len: u64,
    throttle: Option<(u64, Duration)>,
) -> Result<()> {
    let step = match throttle {
        Some((portion, _)) => portion.min(CHUNK as u64).max(1) as usize,
        None => CHUNK,
    };
    let mut buf = vec![0u8; step];
    let mut left = len;
    while left > 0 {
        let want = (left.min(step as u64)) as usize;
        let n = bg.fill(&mut buf[..want]);
        if n == 0 {
            bail!(
                "генератор тіла вичерпався на {} байтах раніше за обіцяні {len} — \
                 це помилка в самому тестовому сервері, не в рушії",
                len - left
            );
        }
        wr.write_all(&buf[..n])
            .await
            .context("запис тіла у сокет")?;
        left -= n as u64;
        if let Some((_, pause)) = throttle
            && left > 0
        {
            tokio::time::sleep(pause).await;
        }
    }
    Ok(())
}
