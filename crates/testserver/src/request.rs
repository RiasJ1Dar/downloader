//! Мінімальний розбір HTTP-запиту.
//!
//! Свідомо власний, а не hyper: сервер має вміти брехати в заголовках і
//! рвати з'єднання посеред тіла, а бібліотека, що поважає протокол, цього
//! не дасть. Тут же дріт повністю в наших руках.

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::net::tcp::OwnedReadHalf;

/// Стеля на заголовки запиту — щоб зациклений клієнт не з'їв пам'ять.
const MAX_HEAD: usize = 64 * 1024;

/// Розібраний запит. Тіло запиту не читаємо: сценарії його не мають.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// Повний target із рядка запиту, разом із query.
    pub target: String,
    /// Імена заголовків — у нижньому регістрі, значення — як прийшли.
    pub headers: Vec<(String, String)>,
}

impl Request {
    /// Перше значення заголовка (ім'я порівнюється без регістру).
    pub fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == lower)
            .map(|(_, v)| v.as_str())
    }

    /// Розібраний `Range`. `None` — заголовка немає або він не `bytes=`.
    pub fn range(&self) -> Option<RangeSpec> {
        RangeSpec::parse(self.header("range")?)
    }

    /// `true`, якщо метод — `HEAD`.
    pub fn is_head(&self) -> bool {
        self.method.eq_ignore_ascii_case("HEAD")
    }
}

/// Один діапазон `Range: bytes=…`. Кілька діапазонів через кому свідомо
/// не підтримуємо — рушій їх не шле, а multipart/byteranges лише заплутав би
/// картину.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSpec {
    /// `bytes=N-M`, межі включно.
    FromTo(u64, u64),
    /// `bytes=N-` — від N до кінця.
    From(u64),
    /// `bytes=-N` — останні N байтів.
    Suffix(u64),
}

impl RangeSpec {
    /// Розбір значення заголовка. `None` на будь-що незрозуміле — сервер тоді
    /// поводиться так, наче `Range` не було.
    pub fn parse(value: &str) -> Option<Self> {
        let rest = value.trim().strip_prefix("bytes=")?;
        if rest.contains(',') {
            return None;
        }
        let (a, b) = rest.split_once('-')?;
        let (a, b) = (a.trim(), b.trim());
        match (a.is_empty(), b.is_empty()) {
            (true, false) => b.parse().ok().map(RangeSpec::Suffix),
            (false, true) => a.parse().ok().map(RangeSpec::From),
            (false, false) => Some(RangeSpec::FromTo(a.parse().ok()?, b.parse().ok()?)),
            (true, true) => None,
        }
    }

    /// Межі (включно) для тіла завдовжки `total`.
    /// `None` — діапазон незадовільний, сервер має відповісти 416.
    pub fn resolve(&self, total: u64) -> Option<(u64, u64)> {
        if total == 0 {
            return None;
        }
        let (start, end) = match *self {
            RangeSpec::FromTo(a, b) => (a, b.min(total - 1)),
            RangeSpec::From(a) => (a, total - 1),
            RangeSpec::Suffix(0) => return None,
            RangeSpec::Suffix(n) => (total.saturating_sub(n), total - 1),
        };
        if start > end || start >= total {
            None
        } else {
            Some((start, end))
        }
    }
}

/// Читає рядок запиту й заголовки до порожнього рядка.
pub async fn read_request(reader: &mut BufReader<OwnedReadHalf>) -> Result<Request> {
    let mut raw = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        let n = reader
            .read(&mut byte)
            .await
            .context("читання заголовків запиту обірвалось")?;
        if n == 0 {
            bail!(
                "клієнт закрив з'єднання, не дописавши заголовки (прочитано {} байт)",
                raw.len()
            );
        }
        raw.push(byte[0]);
        if raw.len() > MAX_HEAD {
            bail!("заголовки запиту довші за {MAX_HEAD} байт — це вже не тест, а атака");
        }
        if raw.ends_with(b"\r\n\r\n") {
            break;
        }
    }

    let text = String::from_utf8_lossy(&raw).into_owned();
    let mut lines = text.split("\r\n");
    let start = lines
        .next()
        .filter(|l| !l.is_empty())
        .context("порожній рядок запиту")?;

    let mut parts = start.split_whitespace();
    let method = parts
        .next()
        .context("у рядку запиту немає методу")?
        .to_string();
    let target = parts
        .next()
        .with_context(|| format!("у рядку запиту {start:?} немає target"))?
        .to_string();

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((k, v)) = line.split_once(':') else {
            bail!("заголовок без двокрапки: {line:?}");
        };
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }

    Ok(Request {
        method,
        target,
        headers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn розбір_range() {
        assert_eq!(
            RangeSpec::parse("bytes=0-99"),
            Some(RangeSpec::FromTo(0, 99))
        );
        assert_eq!(RangeSpec::parse("bytes=100-"), Some(RangeSpec::From(100)));
        assert_eq!(RangeSpec::parse("bytes=-500"), Some(RangeSpec::Suffix(500)));
        assert_eq!(RangeSpec::parse("bytes=0-0"), Some(RangeSpec::FromTo(0, 0)));
        assert_eq!(RangeSpec::parse("items=0-9"), None);
        assert_eq!(RangeSpec::parse("bytes=0-9,20-29"), None);
        assert_eq!(RangeSpec::parse("bytes=-"), None);
    }

    #[test]
    fn межі_діапазону() {
        assert_eq!(RangeSpec::FromTo(0, 99).resolve(1000), Some((0, 99)));
        assert_eq!(RangeSpec::FromTo(0, 9999).resolve(1000), Some((0, 999)));
        assert_eq!(RangeSpec::From(990).resolve(1000), Some((990, 999)));
        assert_eq!(RangeSpec::Suffix(10).resolve(1000), Some((990, 999)));
        assert_eq!(RangeSpec::Suffix(5000).resolve(1000), Some((0, 999)));
        assert_eq!(
            RangeSpec::From(1000).resolve(1000),
            None,
            "поза межами → 416"
        );
        assert_eq!(RangeSpec::FromTo(0, 0).resolve(0), None);
    }
}
