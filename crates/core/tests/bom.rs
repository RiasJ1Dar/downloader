//! Запобіжник проти BOM у файлах, де він не потрібен.
//!
//! BOM — три байти `EF BB BF` на початку файлу, невидимі в редакторі. Вони не
//! заважають ані Rust, ані TOML, і саме тому проблема тиха: файл виглядає
//! звичайним, а перший рядок починається не з того символу, який видно.
//!
//! Двічі за проєкт це вже коштувало часу: `CHANGELOG.txt` із BOM їде в опис
//! релізу на GitHub через `--notes-file`, а тестовий файл із BOM показує три
//! сміттєві символи замість `//!` у кожному переглядачі, який не знає мітки.
//!
//! ⚠️ `.ps1` і `.cs` сюди **не** входять навмисно: для Windows PowerShell 5.1
//! BOM — єдиний спосіб не втратити кирилицю в скрипті, а для C# його ставить
//! редактор сам. Заборона діє лише там, де BOM нічого не дає.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "у тестах перевірка через assert!/expect є нормою"
)]

use std::path::{Path, PathBuf};

/// Розширення, у яких BOM зайвий.
const РОЗШИРЕННЯ: &[&str] = &["rs", "toml", "yml", "yaml", "ftl", "json", "txt"];

const МІТКА: [u8; 3] = [0xEF, 0xBB, 0xBF];

fn корінь_репозиторію() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("корінь репозиторію")
}

fn обхід(тека: &Path, корінь: &Path, знайдені: &mut Vec<PathBuf>) {
    let Ok(вміст) = std::fs::read_dir(тека) else {
        return;
    };

    for запис in вміст.flatten() {
        let назва = запис.file_name();
        // Чужий код і результати збірки нас не стосуються.
        let пропустити = назва == ".git"
            || назва == "target"
            || назва == "node_modules"
            || назва == "bin"
            || назва == "obj";
        if пропустити {
            continue;
        }

        let шлях = запис.path();
        if шлях.is_dir() {
            обхід(&шлях, корінь, знайдені);
            continue;
        }

        let Some(розширення) = шлях.extension().and_then(|x| x.to_str()) else {
            continue;
        };
        if !РОЗШИРЕННЯ.contains(&розширення) {
            continue;
        }

        let Ok(байти) = std::fs::read(&шлях) else {
            continue;
        };
        if байти.starts_with(&МІТКА) {
            знайдені.push(шлях.strip_prefix(корінь).unwrap_or(&шлях).to_path_buf());
        }
    }
}

#[test]
fn у_репозиторії_немає_bom_там_де_він_зайвий() {
    let корінь = корінь_репозиторію();
    let mut знайдені = Vec::new();
    обхід(&корінь, &корінь, &mut знайдені);

    assert!(
        знайдені.is_empty(),
        "файли починаються з BOM (EF BB BF), хоча для цих розширень він зайвий: {знайдені:?}\n\
         Прибрати: sed -i '1s/^\\xEF\\xBB\\xBF//' <файл>"
    );
}
