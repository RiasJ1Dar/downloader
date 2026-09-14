//! Післядія порожньої черги: затримка, скасування, виклик `winutil`.
//!
//! Ядро-рушій лише каже «черга порожня / знову ні». Сон і вимкнення — тут,
//! бо це не качання. Теми вікна тут немає.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use downloader_core::PostAction;

/// Вартовий післядії: нове покоління скасовує вже запланований сон.
#[derive(Default)]
pub struct AfterQueue {
    ticket: AtomicU64,
}

impl AfterQueue {
    /// Черга змінилася: або зброїти післядію, або скасувати попередню.
    pub fn notify<F>(self: &Arc<Self>, idle: bool, action: PostAction, still_idle: F)
    where
        F: Fn() -> bool + Send + 'static,
    {
        if !idle || action == PostAction::None {
            self.ticket.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let покоління = self.ticket.fetch_add(1, Ordering::Relaxed) + 1;
        let delay = delay();
        tracing::info!(
            дія = action.as_str(),
            сек = delay.as_secs(),
            "черга порожня, післядія"
        );

        let me = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if me.ticket.load(Ordering::Relaxed) != покоління {
                return;
            }
            if !still_idle() {
                return;
            }
            if let Err(e) = execute(action) {
                tracing::error!(error = %e, "післядія не вдалась");
            }
        });
    }
}

fn delay() -> Duration {
    std::env::var("DOWNLOADER_POST_DELAY_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(60))
}

fn execute(action: PostAction) -> Result<(), downloader_winutil::PowerError> {
    match action {
        PostAction::None => Ok(()),
        PostAction::Sleep => downloader_winutil::sleep(),
        PostAction::Shutdown => downloader_winutil::shutdown(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_скасовує_а_не_планує() {
        let q = Arc::new(AfterQueue::default());
        q.notify(true, PostAction::None, || true);
        assert_eq!(q.ticket.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn не_idle_скасовує() {
        let q = Arc::new(AfterQueue::default());
        q.notify(false, PostAction::Sleep, || false);
        assert_eq!(q.ticket.load(Ordering::Relaxed), 1);
    }
}
