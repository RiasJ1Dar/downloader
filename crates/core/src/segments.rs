//! Таблиця сегментів із **динамічним поділом**.
//!
//! Це те, чим IDM обганяє решту менеджерів. Звичайна качалка ріже файл на
//! N частин на старті — і потім чекає, поки останній повільний потік дотягне
//! свій шматок, коли решта 31 уже вільні. Тут інакше: вільний потік бере
//! **половину незавантаженого залишку** найповільнішого сегмента.
//!
//! Модуль навмисно не знає ні про мережу, ні про файли — лише арифметика
//! діапазонів. Тому його можна довести тестами до кінця, без жодного байта
//! з мережі.
//!
//! # Чому ідентифікатор, а не індекс
//!
//! [`SegmentId`] — окреме стабільне число, а не позиція у списку. Інакше
//! виходила б тиха катастрофа: воркер бере сегмент, іде качати, тим часом
//! інший воркер робить [`SegmentTable::steal`], вставка зсуває позиції — і
//! перший воркер, повернувшись, пише байти **в чужий діапазон**. Позиції
//! змінюються при кожному поділі; ідентифікатор не змінюється ніколи.
//!
//! # Інваріанти
//!
//! Після будь-якої операції:
//! 1. сегменти впорядковані за `start`;
//! 2. не перекриваються;
//! 3. разом покривають `[0, total)` без дірок;
//! 4. `done` жодного сегмента не виходить за його межі.
//!
//! Порушення будь-якого — це биті дані на диску, тому перевіряється тестами
//! після кожної операції, а не «на око».

use crate::error::{Error, Result};

/// Стабільний ідентифікатор сегмента.
///
/// Живе, доки живе таблиця, і переживає будь-яку кількість поділів.
pub type SegmentId = u64;

/// Один діапазон файла, за який відповідає один воркер.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Segment {
    /// Стабільний ідентифікатор.
    pub id: SegmentId,
    /// Зсув початку в цільовому файлі.
    pub start: u64,
    /// Зсув кінця, **не включно**.
    pub end: u64,
    /// Скільки байтів від `start` уже лежить на диску.
    pub done: u64,
}

impl Segment {
    /// Повна довжина діапазону.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.end - self.start
    }

    /// Чи діапазон порожній.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Скільки ще лишилось завантажити.
    #[must_use]
    pub const fn remaining(&self) -> u64 {
        self.len() - self.done
    }

    /// Наступний байт, який має записати воркер.
    ///
    /// Саме це число йде в заголовок `Range: bytes=cursor-end`.
    #[must_use]
    pub const fn cursor(&self) -> u64 {
        self.start + self.done
    }

    /// Чи сегмент дотягнуто до кінця.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.done == self.len()
    }
}

/// Таблиця сегментів одного файла.
#[derive(Debug, Clone)]
pub struct SegmentTable {
    /// Завжди впорядковані за `start`.
    segments: Vec<Segment>,
    total: u64,
    next_id: SegmentId,
}

impl SegmentTable {
    /// Один сегмент на весь файл.
    ///
    /// Це стан за замовчуванням для сервера, який не тримає `Range`:
    /// ділити нема сенсу, качаємо в один потік.
    #[must_use]
    pub fn single(total: u64) -> Self {
        Self {
            segments: vec![Segment {
                id: 0,
                start: 0,
                end: total,
                done: 0,
            }],
            total,
            next_id: 1,
        }
    }

    /// Початкова нарізка на `parts` приблизно рівних частин.
    ///
    /// `parts` обрізається так, щоб жодна частина не була меншою за
    /// `min_chunk`: тридцять два потоки по 4 КБ — це тридцять два зайві
    /// з'єднання, а не швидкість.
    #[must_use]
    pub fn new(total: u64, parts: usize, min_chunk: u64) -> Self {
        if total == 0 {
            return Self {
                segments: Vec::new(),
                total: 0,
                next_id: 0,
            };
        }

        // `checked_div` замість перевірки на нуль вручну: нульовий
        // `min_chunk` означає «обмеження немає», і це той самий випадок, що
        // «поділити не вийшло».
        let max_parts = match total.checked_div(min_chunk) {
            Some(fit) => {
                let fit = usize::try_from(fit.max(1)).unwrap_or(usize::MAX);
                parts.max(1).min(fit)
            }
            None => parts.max(1),
        };

        let parts = max_parts.max(1);
        let base = total / parts as u64;
        let extra = total % parts as u64;

        let mut segments = Vec::with_capacity(parts);
        let mut start = 0u64;
        for i in 0..parts {
            // Перші `extra` сегментів беруть на байт більше — так сума
            // точно дорівнює total, без округлень «десь загубився байт».
            let len = base + u64::from((i as u64) < extra);
            segments.push(Segment {
                id: i as SegmentId,
                start,
                end: start + len,
                done: 0,
            });
            start += len;
        }

        Self {
            segments,
            total,
            next_id: parts as SegmentId,
        }
    }

    /// Відновити таблицю з того, що збереглося на диску.
    ///
    /// Перевіряє інваріанти й **відмовляється** зібрати таблицю з дірками
    /// або перекриттями: краще чесно перекачати, ніж тихо склеїти сміття.
    pub fn from_parts(mut segments: Vec<Segment>, total: u64) -> Result<Self> {
        segments.sort_by_key(|s| s.start);
        let next_id = segments.iter().map(|s| s.id).max().map_or(0, |m| m + 1);

        let table = Self {
            segments,
            total,
            next_id,
        };
        table.check()?;
        Ok(table)
    }

    /// Сегменти таблиці, впорядковані за початком діапазону.
    #[must_use]
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// Сегмент за ідентифікатором.
    #[must_use]
    pub fn get(&self, id: SegmentId) -> Option<Segment> {
        self.segments.iter().find(|s| s.id == id).copied()
    }

    /// Повний розмір файла.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// Скільки байтів уже на диску.
    #[must_use]
    pub fn downloaded(&self) -> u64 {
        self.segments.iter().map(|s| s.done).sum()
    }

    /// Чи весь файл завантажено.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.downloaded() == self.total
    }

    /// Кількість сегментів.
    #[must_use]
    pub fn len(&self) -> usize {
        self.segments.len()
    }

    /// Чи таблиця порожня (файл нульового розміру).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Просунути прогрес сегмента на `bytes`.
    ///
    /// Вихід за межу сегмента — це **баг воркера**, а не гранична ситуація:
    /// значить, він записав чужі байти. Тому помилка, а не обрізання.
    pub fn advance(&mut self, id: SegmentId, bytes: u64) -> Result<()> {
        let count = self.segments.len();
        let Some(seg) = self.segments.iter_mut().find(|s| s.id == id) else {
            return Err(Error::SegmentOutOfBounds {
                id: usize::try_from(id).unwrap_or(usize::MAX),
                count,
            });
        };

        let new_done = seg.done.saturating_add(bytes);
        if new_done > seg.len() {
            return Err(Error::SegmentOverrun {
                id: usize::try_from(id).unwrap_or(usize::MAX),
                start: seg.start,
                end: seg.end,
                attempted: seg.start + new_done,
            });
        }

        seg.done = new_done;
        Ok(())
    }

    /// **Динамічний поділ.** Забрати роботу в найповільнішого сегмента.
    ///
    /// Знаходить сегмент із найбільшим незавантаженим залишком і віддає
    /// половину цього залишку новому сегменту. Повертає його [`SegmentId`].
    ///
    /// `min_chunk` — поріг здорового глузду: ділити залишок, з якого вийдуть
    /// два шматки менші за поріг, не варто. Тому потрібен залишок від
    /// `2 * min_chunk`.
    ///
    /// `None` означає «роботи для ще одного потоку немає» — це нормальний
    /// кінець, а не помилка.
    ///
    /// ⚠️ Воркер, у якого відібрали хвіст, мусить звірятися з `end` свого
    /// сегмента **перед кожним записом**: інакше він допише байти в чужий
    /// діапазон і зіпсує файл.
    pub fn steal(&mut self, min_chunk: u64) -> Option<SegmentId> {
        let threshold = min_chunk.saturating_mul(2).max(2);

        let (pos, remaining) = self
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.is_complete())
            .map(|(i, s)| (i, s.remaining()))
            .max_by_key(|&(_, rem)| rem)?;

        if remaining < threshold {
            return None;
        }

        // Ділимо саме **залишок**, а не весь сегмент: те, що вже на диску,
        // перекачувати немає жодних причин.
        let seg = self.segments.get_mut(pos)?;
        let split_at = seg.cursor() + remaining / 2;
        let old_end = seg.end;
        seg.end = split_at;

        let id = self.next_id;
        self.next_id += 1;

        // Порядок за `start` тримаємо самі — на ньому стоїть перевірка покриття.
        self.segments.insert(
            pos + 1,
            Segment {
                id,
                start: split_at,
                end: old_end,
                done: 0,
            },
        );

        Some(id)
    }

    /// Перевірити інваріанти таблиці.
    ///
    /// Викликається після відновлення з диска й у тестах після кожної
    /// операції. Дешево — і рятує від найдорожчого класу багів.
    pub fn check(&self) -> Result<()> {
        if self.segments.is_empty() {
            return if self.total == 0 {
                Ok(())
            } else {
                Err(Error::SegmentCoverageBroken {
                    reason: format!("таблиця порожня, а файл має {} байтів", self.total),
                })
            };
        }

        let mut expected_start = 0u64;
        for (i, seg) in self.segments.iter().enumerate() {
            if seg.start != expected_start {
                return Err(Error::SegmentCoverageBroken {
                    reason: format!(
                        "сегмент {i} (id {}) починається на {}, а мав на {expected_start}",
                        seg.id, seg.start
                    ),
                });
            }
            if seg.end < seg.start {
                return Err(Error::SegmentCoverageBroken {
                    reason: format!(
                        "сегмент id {} має кінець {} до початку {}",
                        seg.id, seg.end, seg.start
                    ),
                });
            }
            if seg.done > seg.len() {
                return Err(Error::SegmentCoverageBroken {
                    reason: format!(
                        "сегмент id {} має done={} при довжині {}",
                        seg.id,
                        seg.done,
                        seg.len()
                    ),
                });
            }
            expected_start = seg.end;
        }

        if expected_start != self.total {
            return Err(Error::SegmentCoverageBroken {
                reason: format!(
                    "сегменти покривають {expected_start} байтів замість {}",
                    self.total
                ),
            });
        }

        // Ідентифікатори мусять бути унікальні, інакше `advance` просуне
        // не той сегмент — а це знову байти в чужому діапазоні.
        let mut ids: Vec<SegmentId> = self.segments.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        if ids.len() != before {
            return Err(Error::SegmentCoverageBroken {
                reason: "у таблиці повторюються ідентифікатори сегментів".to_owned(),
            });
        }

        Ok(())
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]
mod tests {
    use super::*;

    /// Мінімальний шматок у тестах — маленький, щоб числа лишались читними.
    const MIN: u64 = 4;

    /// Коротко: діапазон сегмента за ідентифікатором.
    fn range(t: &SegmentTable, id: SegmentId) -> (u64, u64, u64) {
        let s = t.get(id).unwrap_or_else(|| panic!("немає сегмента id {id}"));
        (s.start, s.end, s.done)
    }

    #[test]
    fn нарізка_покриває_файл_без_дірок() {
        let t = SegmentTable::new(100, 3, 1);
        t.check().unwrap();

        assert_eq!(t.len(), 3);
        assert_eq!(range(&t, 0), (0, 34, 0));
        assert_eq!(range(&t, 1), (34, 67, 0));
        assert_eq!(range(&t, 2), (67, 100, 0));

        let sum: u64 = t.segments().iter().map(Segment::len).sum();
        assert_eq!(sum, 100, "жодного загубленого байта");
    }

    #[test]
    fn кількість_частин_обмежена_мінімальним_шматком() {
        let t = SegmentTable::new(10, 32, 4);
        t.check().unwrap();
        assert_eq!(t.len(), 2, "не можна різати файл на шматки, менші за поріг");
    }

    #[test]
    fn порожній_файл_дає_порожню_таблицю() {
        let t = SegmentTable::new(0, 8, MIN);
        t.check().unwrap();
        assert!(t.is_empty());
        assert!(t.is_complete(), "нуль байтів — це вже завершено");
    }

    #[test]
    fn крадіжка_ділить_саме_залишок_а_не_весь_сегмент() {
        let mut t = SegmentTable::single(100);
        t.advance(0, 60).unwrap();

        // Залишок 40 → новий сегмент має початись на 60 + 20 = 80.
        let new_id = t.steal(MIN).expect("залишку 40 вистачає на поділ");
        t.check().unwrap();

        assert_eq!(range(&t, 0), (0, 80, 60));
        assert_eq!(range(&t, new_id), (80, 100, 0));
        assert_eq!(t.downloaded(), 60, "поділ не змінює кількість завантаженого");
    }

    #[test]
    fn крадіжка_обирає_найдовший_залишок() {
        let mut t = SegmentTable::new(300, 3, 1);
        t.advance(0, 99).unwrap(); // залишок 1
        t.advance(1, 10).unwrap(); // залишок 90 ← найбільший
        t.advance(2, 50).unwrap(); // залишок 50

        let new_id = t.steal(MIN).expect("є що ділити");
        t.check().unwrap();

        assert_eq!(range(&t, 1).1, 155, "жертву мали вкоротити до 155");
        assert_eq!(range(&t, new_id), (155, 200, 0));
    }

    /// Найважливіший тест нової моделі: після чужої крадіжки ідентифікатор
    /// далі вказує на **той самий** діапазон. З індексами замість id тут
    /// був би зсув — і запис у чужі байти.
    #[test]
    fn ідентифікатор_переживає_чужу_крадіжку() {
        let mut t = SegmentTable::new(300, 3, 1);
        let (start_before, _, _) = range(&t, 2);

        // Крадіжка вставляє новий сегмент у середину списку.
        t.advance(0, 10).unwrap();
        let stolen = t.steal(MIN).expect("перший сегмент має що віддати");

        assert_eq!(
            range(&t, 2).0,
            start_before,
            "сегмент id 2 мусить лишитись на своєму місці, хоч його позиція у списку зсунулась"
        );
        assert_ne!(stolen, 2, "новий сегмент не має привласнити чужий ідентифікатор");

        // І просування пише саме туди, куди треба.
        t.advance(2, 5).unwrap();
        assert_eq!(range(&t, 2).2, 5);
        t.check().unwrap();
    }

    #[test]
    fn крадіжка_не_чіпає_завершені_сегменти() {
        let mut t = SegmentTable::new(200, 2, 1);
        t.advance(0, 100).unwrap(); // перший дотягнуто

        let new_id = t.steal(MIN).expect("другий ще не готовий");
        assert!(
            range(&t, new_id).0 >= 100,
            "поділ заліз у завершений сегмент"
        );
        t.check().unwrap();
    }

    #[test]
    fn крадіжка_відмовляє_коли_ділити_нема_чого() {
        let mut t = SegmentTable::single(10);
        t.advance(0, 4).unwrap(); // залишок 6, поріг 2*4 = 8

        assert!(
            t.steal(MIN).is_none(),
            "залишок менший за два мінімальні шматки — ділити безглуздо"
        );
    }

    #[test]
    fn крадіжка_відмовляє_на_завершеній_таблиці() {
        let mut t = SegmentTable::single(50);
        t.advance(0, 50).unwrap();

        assert!(t.is_complete());
        assert!(t.steal(1).is_none(), "усе завантажено — красти нема чого");
    }

    #[test]
    fn курсор_показує_наступний_байт_для_range() {
        let mut t = SegmentTable::new(100, 2, 1);
        t.advance(1, 7).unwrap();

        let seg = t.get(1).unwrap();
        assert_eq!(seg.start, 50);
        assert_eq!(seg.cursor(), 57, "Range має початись із 57, а не з 50");
    }

    #[test]
    fn вихід_за_межу_сегмента_це_помилка_а_не_обрізання() {
        let mut t = SegmentTable::new(100, 2, 1);

        let err = t
            .advance(0, 51)
            .expect_err("51 байт не влазить у діапазон 0..50");
        let text = err.to_string();
        assert!(text.contains("сегмент"), "помилка не називає сегмент: {text}");

        t.check().unwrap();
        assert_eq!(t.downloaded(), 0, "невдале просування не має псувати стан");
    }

    #[test]
    fn неіснуючий_сегмент_дає_помилку() {
        let mut t = SegmentTable::single(10);
        assert!(t.advance(42, 1).is_err());
    }

    #[test]
    fn відновлення_з_дірки_відхиляється() {
        let broken = vec![
            Segment { id: 0, start: 0, end: 10, done: 10 },
            // дірка 10..20
            Segment { id: 1, start: 20, end: 30, done: 0 },
        ];

        let err =
            SegmentTable::from_parts(broken, 30).expect_err("таблиця з діркою не має збиратись");
        assert!(err.to_string().contains("починається"), "{err}");
    }

    #[test]
    fn відновлення_з_перекриттям_відхиляється() {
        let broken = vec![
            Segment { id: 0, start: 0, end: 20, done: 0 },
            Segment { id: 1, start: 10, end: 30, done: 0 },
        ];
        assert!(SegmentTable::from_parts(broken, 30).is_err());
    }

    #[test]
    fn відновлення_з_недопокриттям_відхиляється() {
        let short = vec![Segment { id: 0, start: 0, end: 10, done: 0 }];
        let err = SegmentTable::from_parts(short, 100).expect_err("покрито лише 10 зі 100");
        assert!(err.to_string().contains("покривають"), "{err}");
    }

    #[test]
    fn відновлення_з_повтореним_ідентифікатором_відхиляється() {
        // Два сегменти з одним id: `advance` просував би не той, що треба.
        let broken = vec![
            Segment { id: 7, start: 0, end: 10, done: 0 },
            Segment { id: 7, start: 10, end: 20, done: 0 },
        ];
        let err = SegmentTable::from_parts(broken, 20)
            .expect_err("повторені ідентифікатори не можна пропускати");
        assert!(err.to_string().contains("ідентифікатор"), "{err}");
    }

    #[test]
    fn відновлення_впорядковує_сегменти_за_початком() {
        // Файл стану міг зберегти їх у порядку завершення, не за зсувом.
        let shuffled = vec![
            Segment { id: 1, start: 50, end: 100, done: 0 },
            Segment { id: 0, start: 0, end: 50, done: 50 },
        ];
        let t = SegmentTable::from_parts(shuffled, 100).expect("порядок має відновитись сам");

        assert_eq!(t.segments()[0].id, 0);
        assert_eq!(t.downloaded(), 50);
    }

    /// Скільки б поділів і просувань не було, таблиця лишається цілісною,
    /// а сума завантаженого сходиться.
    #[test]
    fn інваріанти_тримаються_після_сотні_операцій() {
        let mut t = SegmentTable::new(1_000, 4, 1);
        let mut крок = 0u64;

        for i in 0..100 {
            крок = (крок * 31 + i) % 97;

            if i % 3 == 0 {
                let _ = t.steal(8);
            } else {
                let pos = (крок as usize) % t.len();
                let seg = t.segments()[pos];
                if seg.remaining() > 0 {
                    let step = (крок % seg.remaining().max(1)).max(1).min(seg.remaining());
                    t.advance(seg.id, step).unwrap();
                }
            }

            t.check()
                .unwrap_or_else(|e| panic!("інваріант зламано на кроці {i}: {e}"));

            let sum: u64 = t.segments().iter().map(Segment::len).sum();
            assert_eq!(sum, 1_000, "покриття зіпсовано на кроці {i}");
            assert!(t.downloaded() <= 1_000, "завантажено більше за файл на кроці {i}");
        }
    }

    #[test]
    fn послідовні_крадіжки_дають_рівно_стільки_сегментів_скільки_просили() {
        let mut t = SegmentTable::single(1024);

        let mut ids = Vec::new();
        for _ in 0..7 {
            ids.push(t.steal(16).expect("є що ділити"));
        }
        t.check().unwrap();

        assert_eq!(t.len(), 8, "сім поділів одного сегмента дають вісім");

        // Усі видані ідентифікатори різні й усі досі дійсні.
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before, "ідентифікатори повторились");
    }
}
