//! Розгортання шаблону IDM `file[001-100].zip` у список адрес.
//!
//! Один діапазон на рядок. Без regex: одна прохідка по байтах шукає `[` і
//! `]`, тож зловмисний рядок не роздує розбір.

use anyhow::{Result, bail};

/// Стеля розгортання: більше адрес — помилка, не тихий обріз.
const СТЕЛЯ: u64 = 1000;

/// Розгорнути `file[001-100].zip` у список адрес.
///
/// Немає пари `[цифри-цифри]` — повертає сам `source` одним елементом.
/// Ширина нулів береться з лівої межі: `[001-003]` → `001`, `002`, `003`.
pub fn розгорнути_шаблон(source: &str) -> Result<Vec<String>> {
    let Some((open, close)) = знайти_єдину_пару(source)? else {
        return Ok(vec![source.to_owned()]);
    };
    let inner = &source[open + 1..close];
    let Some((start, end, width)) = розібрати_діапазон(inner)? else {
        return Ok(vec![source.to_owned()]);
    };

    if start > end {
        bail!("ліва межа діапазону більша за праву: {start} > {end}");
    }

    // start <= end, тож end - start не переповниться. +1 для кількості
    // адрес переповнилось би лише на всьому u64 — тоді стеля і так б'є.
    let span = end - start;
    if span >= СТЕЛЯ {
        let n = span.saturating_add(1);
        bail!("шаблон розгортається в {n} адрес, стеля {СТЕЛЯ}");
    }

    let prefix = &source[..open];
    let suffix = &source[close + 1..];
    let mut urls = Vec::with_capacity((span + 1) as usize);
    for n in start..=end {
        urls.push(format!("{prefix}{n:0width$}{suffix}"));
    }
    Ok(urls)
}

/// Єдина пара `[` `]`. Друга `[` — кілька діапазонів на рядок.
///
/// Незакрита `[` або гола `]` шаблоном не є: повертаємо `None`, і виклик
/// віддасть оригінал.
fn знайти_єдину_пару(source: &str) -> Result<Option<(usize, usize)>> {
    let mut open = None;
    let mut close = None;
    for (i, b) in source.bytes().enumerate() {
        match b {
            b'[' => {
                if open.is_some() {
                    bail!("у рядку кілька пар дужок — дозволено один діапазон");
                }
                open = Some(i);
            }
            b']' if open.is_some() && close.is_none() => close = Some(i),
            _ => {}
        }
    }
    Ok(match (open, close) {
        (Some(o), Some(c)) => Some((o, c)),
        _ => None,
    })
}

/// `001-003` → (1, 3, ширина 3). Інакше це не шаблон.
///
/// Переповнення `u64` — помилка, не «немає шаблону»: цифри є, просто
/// число не вміщається.
fn розібрати_діапазон(inner: &str) -> Result<Option<(u64, u64, usize)>> {
    let Some((left, right)) = inner.split_once('-') else {
        return Ok(None);
    };
    if !лише_цифри(left) || !лише_цифри(right) {
        return Ok(None);
    }
    let start: u64 = left
        .parse()
        .map_err(|_| anyhow::anyhow!("ліва межа діапазону завелика: {left}"))?;
    let end: u64 = right
        .parse()
        .map_err(|_| anyhow::anyhow!("права межа діапазону завелика: {right}"))?;
    Ok(Some((start, end, left.len())))
}

fn лише_цифри(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn без_шаблону_повертає_оригінал() {
        for url in [
            "http://ex.com/file.zip",
            "http://ex.com/file[abc].zip",
            "http://ex.com/file[001].zip",
            "http://ex.com/file[001-].zip",
            "http://ex.com/file[-003].zip",
        ] {
            let got = розгорнути_шаблон(url).expect(url);
            assert_eq!(got, vec![url.to_owned()], "{url}");
        }
    }

    #[test]
    fn розгортає_001_003_з_нулями() {
        let got = розгорнути_шаблон("http://ex.com/a[001-003].zip").expect("001-003");
        assert_eq!(
            got,
            [
                "http://ex.com/a001.zip",
                "http://ex.com/a002.zip",
                "http://ex.com/a003.zip",
            ]
        );
    }

    #[test]
    fn розгортає_1_3_без_нулів() {
        let got = розгорнути_шаблон("http://ex.com/a[1-3].zip").expect("1-3");
        assert_eq!(
            got,
            [
                "http://ex.com/a1.zip",
                "http://ex.com/a2.zip",
                "http://ex.com/a3.zip",
            ]
        );
    }

    #[test]
    fn start_більше_end_це_помилка() {
        let err =
            розгорнути_шаблон("http://ex.com/a[003-001].zip").expect_err("003-001 має впасти");
        let msg = err.to_string();
        assert!(msg.contains("ліва межа діапазону більша за праву"), "{msg}");
        assert!(msg.contains('>'), "{msg}");
    }

    #[test]
    fn стеля_тисяча_адрес() {
        let err = розгорнути_шаблон("http://ex.com/a[1-1001].zip").expect_err("1001 має впасти");
        assert_eq!(
            err.to_string(),
            "шаблон розгортається в 1001 адрес, стеля 1000"
        );

        let ok = розгорнути_шаблон("http://ex.com/a[1-1000].zip").expect("рівно 1000");
        assert_eq!(ok.len(), 1000);
        assert_eq!(ok[0], "http://ex.com/a1.zip");
        assert_eq!(ok[999], "http://ex.com/a1000.zip");
    }

    #[test]
    fn кілька_пар_дужок_це_помилка() {
        let err =
            розгорнути_шаблон("http://ex.com/a[1-2]b[3-4].zip").expect_err("дві пари мають впасти");
        assert!(err.to_string().contains("кілька пар дужок"), "{err}");
    }
}
