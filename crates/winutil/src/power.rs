//! Післядії черги: сон і вимкнення ПК.
//!
//! Викликає ядро, не вікно: закритий клієнт не має скасовувати «вимкнути
//! після черги». На Linux/macOS — best-effort через systemctl/pmset/shutdown;
//! на інших ОС — чесна відмова, не заглушка «ніби вимкнули».

use std::io;
use std::process::Command;

/// Помилка післядії.
#[derive(Debug, thiserror::Error)]
pub enum PowerError {
    /// Ця ОС не вміє сон/вимкнення з ядра (або немає потрібних утиліт).
    #[error("післядія (сон/вимкнення) недоступна на цій ОС")]
    Unsupported,
    /// Системна команда не запустилась або повернула помилку.
    #[error("не вдалося виконати післядію: {0}")]
    Spawn(#[from] io::Error),
    /// Команда завершилась із ненульовим кодом.
    #[error("післядія завершилась кодом {0}")]
    Status(i32),
}

fn dry_run() -> bool {
    matches!(
        std::env::var("DOWNLOADER_POST_ACTION_DRY").as_deref(),
        Ok("1")
    )
}

/// Сон (не гібернація).
pub fn sleep() -> Result<(), PowerError> {
    if dry_run() {
        tracing::info!("dry-run: сон");
        return Ok(());
    }
    sleep_os()
}

/// Вимкнути комп'ютер одразу. Скасування — `shutdown /a`, поки система
/// ще не пішла; ядро саме чекає хвилину перед викликом.
pub fn shutdown() -> Result<(), PowerError> {
    if dry_run() {
        tracing::info!("dry-run: вимкнення");
        return Ok(());
    }
    shutdown_os()
}

#[cfg(windows)]
fn sleep_os() -> Result<(), PowerError> {
    // SetSuspendState через PowerShell, без `unsafe` у нашому коді.
    // Hibernate=false, Force=false, DisableWake=false — звичайний сон.
    let status = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-WindowStyle",
            "Hidden",
            "-Command",
            "Add-Type -Namespace Dl -Name P -MemberDefinition '[DllImport(\"powrprof.dll\")] public static extern bool SetSuspendState(bool h, bool f, bool d);'; [Dl.P]::SetSuspendState($false, $false, $false)",
        ])
        .status()?;
    check(status)
}

#[cfg(windows)]
fn shutdown_os() -> Result<(), PowerError> {
    let status = Command::new("shutdown.exe")
        .args([
            "/s",
            "/t",
            "0",
            "/c",
            "Downloader: черга порожня",
        ])
        .status()?;
    check(status)
}

#[cfg(target_os = "linux")]
fn sleep_os() -> Result<(), PowerError> {
    // systemctl — типовий шлях на systemd; loginctl — запасний (також systemd).
    if try_status("systemctl", &["suspend"]).is_ok() {
        return Ok(());
    }
    try_status("loginctl", &["suspend"])
}

#[cfg(target_os = "linux")]
fn shutdown_os() -> Result<(), PowerError> {
    if try_status("systemctl", &["poweroff"]).is_ok() {
        return Ok(());
    }
    try_status("shutdown", &["-h", "now"])
}

#[cfg(target_os = "macos")]
fn sleep_os() -> Result<(), PowerError> {
    try_status("pmset", &["sleepnow"])
}

#[cfg(target_os = "macos")]
fn shutdown_os() -> Result<(), PowerError> {
    // osascript питає GUI-підтвердження рідше за сирий shutdown у сесії користувача.
    if try_status(
        "osascript",
        &["-e", "tell application \"System Events\" to shut down"],
    )
    .is_ok()
    {
        return Ok(());
    }
    try_status("shutdown", &["-h", "now"])
}

#[cfg(all(not(windows), not(target_os = "linux"), not(target_os = "macos")))]
fn sleep_os() -> Result<(), PowerError> {
    Err(PowerError::Unsupported)
}

#[cfg(all(not(windows), not(target_os = "linux"), not(target_os = "macos")))]
fn shutdown_os() -> Result<(), PowerError> {
    Err(PowerError::Unsupported)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn try_status(program: &str, args: &[&str]) -> Result<(), PowerError> {
    let status = Command::new(program).args(args).status()?;
    check(status)
}

fn check(status: std::process::ExitStatus) -> Result<(), PowerError> {
    if status.success() {
        Ok(())
    } else {
        Err(PowerError::Status(status.code().unwrap_or(-1)))
    }
}
