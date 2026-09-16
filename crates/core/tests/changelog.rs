//! Перевірка журналу змін (CHANGELOG.txt) та відповідності версій у проєкті.
//!
//! Гарантує:
//! 1. Файл CHANGELOG.txt існує у корені репозиторію та має розширення .txt (не .md).
//! 2. Заборонено створення CHANGELOG.md (інваріант відсутності зайвих .md).
//! 3. Версія у Cargo.toml (downloader_core::VERSION) збігається з найновішою версією в журналі.
//! 4. Парсер журналу Keep a Changelog ігнорує секцію [Невипущено] / [Unreleased] і знаходить перший конкретний випуск.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "у тестах перевірка через assert!/expect є нормою"
)]

use std::path::{Path, PathBuf};

fn корінь_репозиторію() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("корінь репозиторію")
}

/// Витягає верхню конкретну версію з журналу змін у форматі Keep a Changelog.
///
/// Ігнорує секції на кшталт ## [Невипущено] або ## [Unreleased],
/// і повертає перший знайдений номер версії (наприклад, "0.1.0").
fn витягти_верхню_версію(вміст: &str) -> Option<String> {
    for рядок in вміст.lines() {
        let рядок = рядок.trim();
        let Some(хвіст) = рядок.strip_prefix("## ") else {
            continue;
        };
        let хвіст = хвіст.trim();
        let Some(початок) = хвіст.find('[') else {
            continue;
        };
        let Some(кінець) = хвіст[початок + 1..].find(']') else {
            continue;
        };
        let версія = &хвіст[початок + 1..початок + 1 + кінець];
        let нижній = версія.to_lowercase();
        if нижній == "unreleased" || нижній == "невипущено" || нижній == "не випущено" {
            continue;
        }
        return Some(версія.to_string());
    }
    None
}

/// Витягає версію з кореневого Cargo.toml (поле ersion = "..." у [workspace.package]).
fn витягти_версію_з_cargo_toml(вміст: &str) -> Option<String> {
    let mut у_секції_workspace_package = false;
    for рядок in вміст.lines() {
        let рядок = рядок.trim();
        if рядок.starts_with('[') {
            у_секції_workspace_package = рядок == "[workspace.package]";
            continue;
        }
        if у_секції_workspace_package && рядок.starts_with("version") {
            let Some((_, val)) = рядок.split_once('=') else {
                continue;
            };
            let val = val.trim().trim_matches('"').trim_matches('\'');
            return Some(val.to_string());
        }
    }
    None
}

#[test]
fn файл_changelog_txt_існує_і_не_порожній() {
    let шлях = корінь_репозиторію().join("CHANGELOG.txt");
    assert!(шлях.exists(), "Файл CHANGELOG.txt має існувати у корені репозиторію");
    let вміст = std::fs::read_to_string(&шлях).expect("читання CHANGELOG.txt");
    assert!(!вміст.trim().is_empty(), "CHANGELOG.txt не повинен бути порожнім");
}

#[test]
fn заборонено_створювати_changelog_md() {
    let шлях = корінь_репозиторію().join("CHANGELOG.md");
    assert!(
        !шлях.exists(),
        "CHANGELOG.md ЗАБОРОНЕНО створювати (правило: лише README.md у корені, журнал має бути .txt)"
    );
}

#[test]
fn версія_у_changelog_збігається_з_cargo_toml_та_версією_ядра() {
    let шлях_журналу = корінь_репозиторію().join("CHANGELOG.txt");
    let вміст_журналу = std::fs::read_to_string(&шлях_журналу).expect("читання CHANGELOG.txt");
    let верхня_версія = витягти_верхню_версію(&вміст_журналу)
        .expect("у CHANGELOG.txt має бути знайдено хоча б один випуск");

    // Звірка з константою версії ядра
    assert_eq!(
        верхня_версія,
        downloader_core::VERSION,
        "Верхній запис у CHANGELOG.txt ({верхня_версія}) не збігається з downloader_core::VERSION ({})",
        downloader_core::VERSION
    );

    // Звірка з версією в кореневому Cargo.toml
    let шлях_cargo = корінь_репозиторію().join("Cargo.toml");
    let вміст_cargo = std::fs::read_to_string(&шлях_cargo).expect("читання Cargo.toml");
    let версія_cargo = витягти_версію_з_cargo_toml(&вміст_cargo)
        .expect("у [workspace.package] має бути вказана version");

    assert_eq!(
        верхня_версія,
        версія_cargo,
        "Верхній запис у CHANGELOG.txt ({верхня_версія}) не збігається з версією в Cargo.toml ({версія_cargo})"
    );
}

#[test]
fn парсер_версій_ігнорує_невипущено_і_знаходить_перший_реліз() {
    let приклад = r#"
# Журнал

## [Невипущено]
- Якась нова функція в розробці

## [Unreleased]
- Something in progress

## [1.2.3] - 2026-10-01
- Реліз 1.2.3

## [1.2.2] - 2026-09-01
- Старий реліз
"#;
    let версія = витягти_верхню_версію(приклад).expect("має знайти версію");
    assert_eq!(версія, "1.2.3");
}

#[test]
fn парсер_повертає_none_якщо_немає_випусків() {
    let приклад = r#"
# Журнал

## [Невипущено]
- Лише майбутні зміни без випусків
"#;
    assert_eq!(витягти_верхню_версію(приклад), None);
}
