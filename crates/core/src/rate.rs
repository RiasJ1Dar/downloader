//! Обмеження швидкості — «текуче відро» (token bucket).
//!
//! Відро наповнюється рівномірно, кожен воркер бере з нього байти перед
//! записом. Порожньо — чекає.
//!
//! # Три рішення, від яких залежить, чи це працюватиме
//!
//! **Місткість дорівнює секунді трафіку.** Спокусливо зробити відро
//! більшим, «щоб не гальмувало». Але тоді після хвилини простою воно
//! віддасть хвилинний трафік одним ривком — і людина, яка поставила ліміт
//! саме щоб не забивати канал, отримає рівно те, чого уникала.
//!
//! **Ділити відмовою, а не чергою.** Воркер, якому не вистачило, отримує
//! **скільки є** плюс підказку, скільки чекати. Черга з порядком доступу
//! виглядає справедливішою, але на 32 потоках перетворює обмежувач на
//! вузьке місце: кожен байт проходить через один м'ютекс із чеканням.
//!
//! **Спати має викликач, а не ми.** Тому тут немає `async` і немає `tokio`.
//! Ядро лишається без рантайму, а протокол-модуль сам вирішує, чим зайняти
//! паузу. Заразом це робить обмежувач придатним для синхронного коду.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Скільки дозволено взяти зараз і скільки чекати, якщо мало.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allowance {
    /// Скільки байтів можна використати негайно. Може бути менше, ніж
    /// просили, і навіть нуль.
    pub allowed: u64,
    /// Скільки чекати перед наступною спробою, якщо дозволу не вистачило.
    pub wait: Option<Duration>,
}

/// Обмежувач швидкості.
///
/// Один на все завантаження (або один на програму — залежно від того, який
/// ліміт налаштовано). Дешевий у копіюванні через `Arc`.
#[derive(Debug)]
pub struct RateLimiter {
    bucket: Mutex<Option<Bucket>>,
}

#[derive(Debug)]
struct Bucket {
    /// Доступні байти. Дробові — щоб повільні ліміти не округлювались у нуль.
    tokens: f64,
    /// Стеля накопичення: рівно секунда трафіку.
    capacity: f64,
    /// Скільки байтів додається щосекунди.
    refill_per_sec: f64,
    last: Instant,
}

/// Найдовша пауза за один раз.
///
/// Довше спати немає сенсу: за цей час умови змінюються — інші воркери
/// звільняють токени, людина міняє ліміт, завдання зупиняють.
const MAX_WAIT: Duration = Duration::from_millis(50);

impl RateLimiter {
    /// Обмежувач на задану швидкість у байтах за секунду.
    ///
    /// Нуль означає «без обмежень»: так простіше зверху — не треба окремо
    /// розрізняти «ліміт вимкнено» і «ліміт нуль», який зупинив би все.
    #[must_use]
    pub fn new(bytes_per_sec: u64) -> Self {
        if bytes_per_sec == 0 {
            return Self::unlimited();
        }

        let rate = bytes_per_sec as f64;
        Self {
            bucket: Mutex::new(Some(Bucket {
                // Стартуємо з повним відром: перший шматок має піти одразу,
                // інакше кожне завантаження починалося б із паузи.
                tokens: rate,
                capacity: rate,
                refill_per_sec: rate,
                last: Instant::now(),
            })),
        }
    }

    /// Обмежувач, який нічого не обмежує.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            bucket: Mutex::new(None),
        }
    }

    /// Чи ліміт узагалі діє.
    #[must_use]
    pub fn is_limited(&self) -> bool {
        self.bucket.lock().map(|b| b.is_some()).unwrap_or(false)
    }

    /// Змінити ліміт швидкості на льоту без перезапуску завантаження.
    ///
    /// Нуль вимикає обмеження.
    pub fn set_limit(&self, bytes_per_sec: u64) {
        let Ok(mut guard) = self.bucket.lock() else {
            tracing::warn!("обмежувач швидкості отруєно при set_limit");
            return;
        };

        if bytes_per_sec == 0 {
            *guard = None;
        } else {
            let rate = bytes_per_sec as f64;
            match guard.as_mut() {
                Some(b) => {
                    b.refill();
                    b.capacity = rate;
                    b.refill_per_sec = rate;
                    if b.tokens > rate {
                        b.tokens = rate;
                    }
                }
                None => {
                    *guard = Some(Bucket {
                        tokens: rate,
                        capacity: rate,
                        refill_per_sec: rate,
                        last: Instant::now(),
                    });
                }
            }
        }
    }

    /// Спитати дозволу на `want` байтів.
    ///
    /// Повертає, скільки можна взяти **зараз**. Якщо менше, ніж просили,
    /// у [`Allowance::wait`] буде пауза перед наступною спробою.
    pub fn take(&self, want: u64) -> Allowance {
        if want == 0 {
            return Allowance {
                allowed: 0,
                wait: None,
            };
        }

        let Ok(mut guard) = self.bucket.lock() else {
            // Отруєний м'ютекс не привід зупиняти завантаження: гірше, що
            // станеться — ліміт на мить перестане діяти.
            tracing::warn!("обмежувач швидкості отруєно, пропускаю без ліміту");
            return Allowance {
                allowed: want,
                wait: None,
            };
        };

        let Some(b) = guard.as_mut() else {
            return Allowance {
                allowed: want,
                wait: None,
            };
        };

        b.refill();

        if b.tokens >= 1.0 {
            let allowed = (b.tokens.floor() as u64).min(want);
            b.tokens -= allowed as f64;

            return Allowance {
                allowed,
                wait: (allowed < want).then(|| b.wait_for(want - allowed)),
            };
        }

        Allowance {
            allowed: 0,
            wait: Some(b.wait_for(want)),
        }
    }
}

impl Bucket {
    /// Долити токенів за час, що минув.
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;

        // Стеля навмисна: без неї відро накопичило б простій і віддало його
        // одним ривком.
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
    }

    /// Скільки чекати, щоб набралось `need` байтів.
    fn wait_for(&self, need: u64) -> Duration {
        let secs = need as f64 / self.refill_per_sec;
        Duration::from_secs_f64(secs.max(0.0)).min(MAX_WAIT)
    }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]
mod tests {
    use super::*;

    #[test]
    fn без_ліміту_дозволяється_все() {
        let r = RateLimiter::unlimited();
        assert!(!r.is_limited());

        let a = r.take(1_000_000);
        assert_eq!(a.allowed, 1_000_000);
        assert!(a.wait.is_none());
    }

    #[test]
    fn нульовий_ліміт_означає_без_обмежень() {
        // Інакше «ліміт 0» зупинив би завантаження назавжди, і кожен
        // виклик згори мусив би окремо розрізняти ці випадки.
        let r = RateLimiter::new(0);
        assert!(!r.is_limited());
        assert_eq!(r.take(500).allowed, 500);
    }

    #[test]
    fn перший_шматок_іде_без_паузи() {
        let r = RateLimiter::new(1000);
        let a = r.take(1000);

        assert_eq!(a.allowed, 1000, "старт із повним відром");
        assert!(a.wait.is_none());
    }

    #[test]
    fn вичерпане_відро_дає_нуль_і_паузу() {
        let r = RateLimiter::new(1000);
        r.take(1000);

        let a = r.take(500);
        assert_eq!(a.allowed, 0);
        assert!(a.wait.is_some(), "мала бути підказка, скільки чекати");
    }

    #[test]
    fn часткова_видача_каже_скільки_ще_чекати() {
        let r = RateLimiter::new(1000);
        r.take(800);

        let a = r.take(500);
        assert!(a.allowed > 0 && a.allowed < 500, "видали {}", a.allowed);
        assert!(a.wait.is_some(), "решту доведеться чекати");
    }

    #[test]
    fn пауза_не_довша_за_стелю() {
        let r = RateLimiter::new(10);
        r.take(10);

        // Просимо стільки, що чекати довелось би сто секунд.
        let a = r.take(1000);
        assert!(
            a.wait.unwrap() <= MAX_WAIT,
            "довга пауза заморозила б воркер попри зміну умов"
        );
    }

    #[test]
    fn відро_наповнюється_з_часом() {
        let r = RateLimiter::new(10_000);
        r.take(10_000);
        assert_eq!(r.take(100).allowed, 0);

        std::thread::sleep(Duration::from_millis(50));

        let a = r.take(100);
        assert!(a.allowed > 0, "за 50 мс мало натекти близько 500 байтів");
    }

    /// Головна властивість: за проміжок часу не можна взяти більше, ніж
    /// дозволяє ліміт.
    #[test]
    fn за_півсекунди_не_видається_більше_ніж_пів_ліміту() {
        const ЛІМІТ: u64 = 100_000;
        let r = RateLimiter::new(ЛІМІТ);

        let старт = Instant::now();
        let mut взято = 0u64;

        while старт.elapsed() < Duration::from_millis(500) {
            взято += r.take(4096).allowed;
            std::thread::sleep(Duration::from_millis(1));
        }

        let минуло = старт.elapsed().as_secs_f64();
        // Стартове відро — це ще секунда трафіку понад норму.
        let стеля = (ЛІМІТ as f64 * (минуло + 1.0)) as u64;

        assert!(
            взято <= стеля,
            "видано {взято} байтів за {минуло:.2} с при ліміті {ЛІМІТ}/с — це понад стелю {стеля}"
        );
    }

    #[test]
    fn простій_не_дає_накопичити_запас() {
        let r = RateLimiter::new(1000);
        r.take(1000);

        // Довгий простій: наївне відро набрало б трафік за весь цей час.
        std::thread::sleep(Duration::from_millis(300));

        let a = r.take(100_000);
        assert!(
            a.allowed <= 1000,
            "після простою видано {} — відро накопичило запас понад секунду",
            a.allowed
        );
    }

    #[test]
    fn нульовий_запит_нічого_не_витрачає() {
        let r = RateLimiter::new(1000);
        assert_eq!(r.take(0).allowed, 0);
        assert_eq!(r.take(1000).allowed, 1000, "нульовий запит з'їв токени");
    }

    #[test]
    fn кілька_потоків_разом_не_перевищують_ліміту() {
        use std::sync::Arc;

        const ЛІМІТ: u64 = 50_000;
        let r = Arc::new(RateLimiter::new(ЛІМІТ));
        let взято = Arc::new(std::sync::atomic::AtomicU64::new(0));

        let старт = Instant::now();
        let mut ручки = Vec::new();

        for _ in 0..8 {
            let r = Arc::clone(&r);
            let взято = Arc::clone(&взято);
            ручки.push(std::thread::spawn(move || {
                while старт.elapsed() < Duration::from_millis(300) {
                    let a = r.take(2048);
                    взято.fetch_add(a.allowed, std::sync::atomic::Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(1));
                }
            }));
        }
        for h in ручки {
            h.join().unwrap();
        }

        let минуло = старт.elapsed().as_secs_f64();
        let стеля = (ЛІМІТ as f64 * (минуло + 1.0)) as u64;
        let разом = взято.load(std::sync::atomic::Ordering::Relaxed);

        assert!(
            разом <= стеля,
            "вісім потоків узяли {разом} за {минуло:.2} с — понад стелю {стеля}"
        );
    }

    #[test]
    fn динамічна_зміна_ліміту_працює_на_льоту() {
        let r = RateLimiter::new(1000);
        assert!(r.is_limited());
        r.set_limit(0);
        assert!(!r.is_limited());
        assert_eq!(r.take(5000).allowed, 5000);

        r.set_limit(2000);
        assert!(r.is_limited());
        assert_eq!(r.take(2000).allowed, 2000);
    }
}

