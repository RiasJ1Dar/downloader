//! Кадрування повідомлень у потоці.
//!
//! Named pipe і сокет — це потік байтів без меж повідомлень. Тому кожен
//! кадр несе довжину попереду: чотири байти little-endian, далі JSON у UTF-8.
//!
//! # Чому не «рядок до \n»
//!
//! Спокусливо розділяти повідомлення переносом рядка — так роблять багато
//! саморобних протоколів. Але тоді будь-який `\n` усередині даних (а імена
//! файлів приходять з мережі й містять що завгодно) розриває кадр навпіл, і
//! обидві половини стають несправним JSON. Довжина попереду знімає це
//! питання цілком.
//!
//! # Межа розміру
//!
//! Кадр більший за [`MAX_FRAME`] відхиляється **до** читання тіла. Без цієї
//! перевірки досить надіслати чотири байти `FF FF FF FF`, і процес спробує
//! виділити чотири гігабайти — локальна відмова в обслуговуванні коштом
//! одного пакета.

use std::io;

use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Найбільший припустимий кадр — 16 МіБ.
///
/// Реальні повідомлення — сотні байтів; список із тисячі завдань не сягне й
/// мегабайта. Запас великий навмисно, щоб межа спрацьовувала лише на
/// зіпсованому або зловмисному потоці.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Помилки кадрування.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// Співрозмовник закрив з'єднання. Нормальне завершення, не збій.
    #[error("з'єднання закрито")]
    Closed,

    #[error("кадр завеликий: {size} байтів при межі {MAX_FRAME}")]
    TooLarge { size: usize },

    #[error("кадр не є коректним JSON: {0}")]
    Malformed(String),

    #[error("помилка вводу-виводу: {0}")]
    Io(#[from] io::Error),
}

/// Надіслати повідомлення.
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<(), FrameError>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize,
{
    let mut packet = Vec::with_capacity(512);
    packet.extend_from_slice(&[0u8; 4]);
    serde_json::to_writer(&mut packet, value).map_err(|e| FrameError::Malformed(e.to_string()))?;

    let body_len = packet.len() - 4;
    if body_len > MAX_FRAME {
        return Err(FrameError::TooLarge { size: body_len });
    }

    let len = u32::try_from(body_len).unwrap_or(u32::MAX);
    packet[..4].copy_from_slice(&len.to_le_bytes());

    writer.write_all(&packet).await?;
    writer.flush().await?;
    Ok(())
}

/// Прочитати повідомлення.
///
/// [`FrameError::Closed`] означає, що співрозмовник пішов — це не помилка, а
/// звичайний кінець розмови, і викликач має розрізняти ці випадки.
pub async fn read_frame<R, T>(reader: &mut R) -> Result<T, FrameError>
where
    R: AsyncReadExt + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Err(FrameError::Closed),
        // Розрив каналу на Windows приходить як BrokenPipe, а не EOF.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Err(FrameError::Closed),
        Err(e) => return Err(FrameError::Io(e)),
    }

    let size = u32::from_le_bytes(len_buf) as usize;

    // Перевірка **до** виділення пам'яті — інакше чотири байти від
    // зловмисника коштують нам гігабайтів.
    if size > MAX_FRAME {
        return Err(FrameError::TooLarge { size });
    }

    let mut body = vec![0u8; size];
    match reader.read_exact(&mut body).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Err(FrameError::Closed),
        Err(e) => return Err(FrameError::Io(e)),
    }

    serde_json::from_slice(&body).map_err(|e| FrameError::Malformed(e.to_string()))
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct Проба {
        текст: String,
        число: u64,
    }

    #[tokio::test]
    async fn кадр_обходить_потік_і_повертається_таким_самим() {
        let msg = Проба {
            текст: "звіт за 2026 рік.pdf".to_owned(),
            число: 42,
        };

        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).await.unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let back: Проба = read_frame(&mut cursor).await.unwrap();

        assert_eq!(back, msg);
    }

    #[tokio::test]
    async fn перенос_рядка_в_даних_не_розриває_кадр() {
        // Саме через це кадруємо довжиною, а не рядками: ім'я файла з
        // мережі може містити будь-що.
        let msg = Проба {
            текст: "рядок\nдругий\r\nтретій".to_owned(),
            число: 1,
        };

        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).await.unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let back: Проба = read_frame(&mut cursor).await.unwrap();
        assert_eq!(back, msg);
    }

    #[tokio::test]
    async fn кілька_кадрів_читаються_по_черзі() {
        let mut buf = Vec::new();
        for i in 0..3u64 {
            let m = Проба {
                текст: format!("повідомлення {i}"),
                число: i,
            };
            write_frame(&mut buf, &m).await.unwrap();
        }

        let mut cursor = std::io::Cursor::new(buf);
        for i in 0..3u64 {
            let back: Проба = read_frame(&mut cursor).await.unwrap();
            assert_eq!(back.число, i, "кадри переплутались");
        }
    }

    #[tokio::test]
    async fn закритий_потік_це_окремий_випадок_а_не_помилка() {
        let mut cursor = std::io::Cursor::new(Vec::new());
        let err = read_frame::<_, Проба>(&mut cursor).await.unwrap_err();

        assert!(
            matches!(err, FrameError::Closed),
            "порожній потік — це «співрозмовник пішов», а не збій: {err:?}"
        );
    }

    #[tokio::test]
    async fn завеликий_заявлений_розмір_відхиляється_до_виділення_памʼяті() {
        // Чотири байти, що обіцяють чотири гігабайти. Наївний читач саме тут
        // і лягає.
        let buf = vec![0xFF, 0xFF, 0xFF, 0xFF];
        let mut cursor = std::io::Cursor::new(buf);

        let err = read_frame::<_, Проба>(&mut cursor).await.unwrap_err();
        assert!(matches!(err, FrameError::TooLarge { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn обрізаний_кадр_дає_помилку_а_не_паніку() {
        let msg = Проба {
            текст: "довгий текст".to_owned(),
            число: 7,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).await.unwrap();

        // Ріжемо тіло навпіл — так виглядає обірване з'єднання.
        buf.truncate(buf.len() / 2);

        let mut cursor = std::io::Cursor::new(buf);
        let err = read_frame::<_, Проба>(&mut cursor).await.unwrap_err();
        assert!(matches!(err, FrameError::Closed), "{err:?}");
    }

    #[tokio::test]
    async fn сміття_замість_json_розпізнається() {
        let body = "це не json".as_bytes();
        let mut buf = (body.len() as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(body);

        let mut cursor = std::io::Cursor::new(buf);
        let err = read_frame::<_, Проба>(&mut cursor).await.unwrap_err();

        assert!(matches!(err, FrameError::Malformed(_)), "{err:?}");
    }

    #[tokio::test]
    async fn порожній_кадр_не_ламає_читача() {
        let buf = 0u32.to_le_bytes().to_vec();
        let mut cursor = std::io::Cursor::new(buf);

        // Порожнє тіло — не валідний JSON, але й не привід падати.
        let err = read_frame::<_, Проба>(&mut cursor).await.unwrap_err();
        assert!(matches!(err, FrameError::Malformed(_)), "{err:?}");
    }
}
