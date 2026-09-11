//! Детермінований генератор тіла і розбір розмірів зі шляху.
//!
//! Тіло сценарію — псевдовипадковий потік байтів, повністю визначений
//! розміром. Тому тест може порахувати очікуваний SHA-256 не зберігаючи
//! еталонного файла: `expected_sha256("/plain/10m")`.
//!
//! Генератор власний (splitmix64), а не з крейта `rand`: `rand` не дає
//! гарантії, що потік байтів не зміниться між версіями, а нам потрібна
//! відтворюваність назавжди — інакше збережений у тесті хеш одного дня
//! просто перестане збігатись без жодної зміни в коді.

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

/// Довільна непарна константа — «сіль», щоб потік не був схожий на splitmix64
/// із нульового стану (такий легко сплутати з нулями при налагодженні).
const SEED_SALT: u64 = 0x9E37_79B9_7F4A_7C15;

/// Розмір тіла для сценаріїв, де воно неважливе (`/disposition/<case>`).
pub const TINY_BODY: u64 = 64;

/// Крок splitmix64. Швидкий, детермінований, без залежностей.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Потік тіла заданого розміру.
///
/// Байти віддаються порціями будь-якого розміру, але сам потік від нарізки
/// не залежить: слово з 8 байтів довиробляється до кінця, перш ніж узяти
/// наступне. Інакше `/slow` (дрібні чанки) і `/plain` (великі) давали б
/// різні файли на однаковому розмірі.
pub struct BodyGen {
    state: u64,
    word: [u8; 8],
    word_pos: usize,
    produced: u64,
    total: u64,
}

impl BodyGen {
    /// Новий потік на `total` байтів. Зерно виводиться з розміру.
    pub fn new(total: u64) -> Self {
        let state = SEED_SALT ^ total.wrapping_mul(0xD6E8_FEB8_6659_FD93);
        Self {
            state,
            word: [0; 8],
            word_pos: 8, // «слово вичерпане» — перший fill візьме нове
            produced: 0,
            total,
        }
    }

    /// Скільки ще лишилось віддати.
    pub fn remaining(&self) -> u64 {
        self.total - self.produced
    }

    /// Перемотати потік на `n` байтів уперед — для 206 з `Range: bytes=n-…`.
    pub fn skip(&mut self, n: u64) {
        // Чесно прокручуємо генератор: інакше байт зі зміщення n у 206
        // не збігся б із тим самим байтом у 200, і докачка «склеїла» б
        // биті дані, чого тест якраз не мав би помітити.
        let mut left = n;
        let mut sink = [0u8; 4096];
        while left > 0 {
            let take = left.min(sink.len() as u64) as usize;
            let got = self.fill(&mut sink[..take]);
            if got == 0 {
                break;
            }
            left -= got as u64;
        }
    }

    /// Записує наступні байти в `buf`, повертає скільки записав.
    /// `0` означає, що тіло вичерпане.
    pub fn fill(&mut self, buf: &mut [u8]) -> usize {
        let limit = buf.len().min(self.remaining() as usize);
        let mut written = 0;
        while written < limit {
            if self.word_pos == 8 {
                self.word = splitmix64(&mut self.state).to_le_bytes();
                self.word_pos = 0;
            }
            let take = (8 - self.word_pos).min(limit - written);
            buf[written..written + take]
                .copy_from_slice(&self.word[self.word_pos..self.word_pos + take]);
            self.word_pos += take;
            written += take;
        }
        self.produced += written as u64;
        written
    }
}

/// Повне тіло заданого розміру одним шматком. Для тестів і gzip.
pub fn body_bytes(total: u64) -> Vec<u8> {
    let mut bg = BodyGen::new(total);
    let mut out = vec![0u8; total as usize];
    let mut off = 0usize;
    while off < out.len() {
        let n = bg.fill(&mut out[off..]);
        if n == 0 {
            break;
        }
        off += n;
    }
    out
}

/// SHA-256 тіла заданого розміру, у нижньому регістрі hex.
/// Рахується потоково — 10 МіБ не матеріалізуються в пам'яті.
pub fn expected_sha256_of_size(total: u64) -> String {
    let mut bg = BodyGen::new(total);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = bg.fill(&mut buf);
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    hex::encode(hasher.finalize())
}

/// SHA-256 того тіла, яке сценарій за цим шляхом **обіцяє** віддати повністю.
///
/// ⚠️ Тонкощі, які варто знати:
/// * `/cut/<size>/<at>` — повертає хеш **повного** тіла `<size>`, якого сервер
///   свідомо не віддасть. Це і є еталон, до якого має дотягнутись рушій
///   після повтору.
/// * `/gzip/<size>` — хеш **розпакованого** тіла: на диск має лягти саме воно.
/// * `/liar-length/<size>/<claimed>` — хеш `<size>`, тобто того, що реально
///   пішло на дріт, а не того, що заявлено в `Content-Length`.
pub fn expected_sha256(path: &str) -> Result<String> {
    let segs = path_segments(path);
    let s: Vec<&str> = segs.iter().map(String::as_str).collect();
    let total = match s.as_slice() {
        ["plain", size]
        | ["norange", size]
        | ["changing", size]
        | ["gzip", size]
        | ["auth", size]
        | ["head-only", size]
        | ["no-head", size] => parse_size(size)?,
        ["cut", size, _]
        | ["flaky", size, _]
        | ["liar-length", size, _]
        | ["slow", size, _]
        | ["slow-range", size, _] => parse_size(size)?,
        ["redirect", _, size] => parse_size(size)?,
        ["disposition", _case] => TINY_BODY,
        _ => bail!(
            "expected_sha256: шлях {path:?} не відповідає жодному сценарію \
             (сегменти після відкидання імені файлу: {segs:?})"
        ),
    };
    Ok(expected_sha256_of_size(total))
}

/// Сегменти шляху без порожніх, без query і **без хвостового імені файлу**.
///
/// Хвостовий сегмент із крапкою відкидається, щоб тест міг писати
/// `/norange/1m/payload.bin` — рушієві потрібне правдоподібне ім'я в URL,
/// а сценарію воно байдуже. Виняток — коли такий сегмент єдиний: тоді це
/// сама назва сценарію, і відкидати нічого.
pub fn path_segments(path: &str) -> Vec<String> {
    let no_query = path.split('?').next().unwrap_or("");
    let mut segs: Vec<String> = no_query
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let drop_tail = segs.len() >= 2 && segs.last().is_some_and(|s| s.contains('.'));
    if drop_tail {
        segs.pop();
    }
    segs
}

/// Розбір розміру: `1234`, `512k`, `10m`. Суфікси двійкові (k = 1024).
pub fn parse_size(raw: &str) -> Result<u64> {
    let t = raw.trim();
    if t.is_empty() {
        bail!("порожній розмір у шляху");
    }
    let (digits, mult) = match t.as_bytes()[t.len() - 1] {
        b'k' | b'K' => (&t[..t.len() - 1], 1024u64),
        b'm' | b'M' => (&t[..t.len() - 1], 1024 * 1024),
        _ => (t, 1),
    };
    let n: u64 = digits
        .parse()
        .with_context(|| format!("розмір {raw:?} не число (очікується N, Nk або Nm)"))?;
    n.checked_mul(mult)
        .ok_or_else(|| anyhow!("розмір {raw:?} переповнює u64"))
}

/// Розбір цілого без суфікса — для лічильників (`<n>` у `/flaky`, `/redirect`).
pub fn parse_count(raw: &str) -> Result<u64> {
    raw.trim()
        .parse()
        .with_context(|| format!("очікувалось ціле число, а в шляху {raw:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn потік_не_залежить_від_нарізки() {
        // Той самий розмір, різні розміри чанків — байти мусять збігтись.
        let cilkom = body_bytes(5000);
        let mut bg = BodyGen::new(5000);
        let mut po_shmatkah = Vec::new();
        for chunk in [1usize, 3, 7, 8, 9, 1000] {
            let mut buf = vec![0u8; chunk];
            loop {
                let n = bg.fill(&mut buf);
                if n == 0 {
                    break;
                }
                po_shmatkah.extend_from_slice(&buf[..n]);
                if po_shmatkah.len() % 997 == 0 {
                    break; // час від часу міняємо розмір чанка
                }
            }
        }
        assert_eq!(cilkom, po_shmatkah);
    }

    #[test]
    fn skip_дає_той_самий_байт() {
        let cilkom = body_bytes(4096);
        let mut bg = BodyGen::new(4096);
        bg.skip(1000);
        let mut buf = [0u8; 16];
        let n = bg.fill(&mut buf);
        assert_eq!(&buf[..n], &cilkom[1000..1016]);
    }

    #[test]
    fn розміри_з_суфіксами() -> Result<()> {
        assert_eq!(parse_size("1234")?, 1234);
        assert_eq!(parse_size("512k")?, 512 * 1024);
        assert_eq!(parse_size("10m")?, 10 * 1024 * 1024);
        assert_eq!(parse_size("10M")?, 10 * 1024 * 1024);
        assert!(parse_size("abc").is_err());
        assert!(parse_size("").is_err());
        Ok(())
    }

    #[test]
    fn імʼя_файла_в_хвості_відкидається() {
        assert_eq!(path_segments("/norange/1m/payload.bin"), ["norange", "1m"]);
        assert_eq!(path_segments("/plain/10m"), ["plain", "10m"]);
        assert_eq!(path_segments("/plain/10m?x=1"), ["plain", "10m"]);
        assert_eq!(
            path_segments("/disposition/cyrillic"),
            ["disposition", "cyrillic"]
        );
    }

    #[test]
    fn хеш_стабільний_і_залежить_від_розміру() -> Result<()> {
        let a = expected_sha256("/plain/1k")?;
        let b = expected_sha256("/norange/1k/file.bin")?;
        let c = expected_sha256("/plain/2k")?;
        assert_eq!(a, b, "однаковий розмір — однакове тіло");
        assert_ne!(a, c);
        assert_eq!(a, expected_sha256_of_size(1024));
        assert!(expected_sha256("/nema-takogo/1k").is_err());
        Ok(())
    }
}
