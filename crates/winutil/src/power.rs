//! Післядії черги: сон і вимкнення ПК.
//!
//! Викликає ядро, не вікно: закритий клієнт не має скасовувати «вимкнути
//! після черги». На не-Windows це чесна відмова, не заглушка «ніби вимкнули».

use std::io;
use std::process::Command;

/// Помилка післядії.
#[derive(Debug, thiserror::Error)]
pub enum PowerError {
    /// Ця ОС не вміє сон/вимкнення з ядра.
    #[error("післядія доступна лише на Windows")]
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

#[cfg(not(windows))]
fn sleep_os() -> Result<(), PowerError> {
    Err(PowerError::Unsupported)
}

#[cfg(not(windows))]
fn shutdown_os() -> Result<(), PowerError> {
    Err(PowerError::Unsupported)
}

fn check(status: std::process::ExitStatus) -> Result<(), PowerError> {
    if status.success() {
        Ok(())
    } else {
        Err(PowerError::Status(status.code().unwrap_or(-1)))
    }
}
