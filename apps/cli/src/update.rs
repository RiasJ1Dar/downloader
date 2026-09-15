use std::path::Path;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DEFAULT_UPDATE_MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/RiasJ1Dar/downloader/master/packaging/update.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdatePackage {
    pub name: String,
    #[serde(rename = "type", default)]
    pub package_type: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateManifest {
    pub version: String,
    #[serde(default)]
    pub min_supported_version: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub changelog_url: Option<String>,
    #[serde(default)]
    pub packages: Vec<UpdatePackage>,
    #[serde(default)]
    pub pubkey_id: Option<String>,
    #[serde(default)]
    pub signature: Option<String>,
}

impl UpdateManifest {
    pub fn parse(json_str: &str) -> Result<Self> {
        serde_json::from_str(json_str).context("не вдалося розібрати JSON маніфесту оновлень")
    }
}

/// Порівнює версії у форматі SemVer (наприклад "0.2.0" проти "0.1.0").
/// Повертає true, якщо new_version строго новіша за current_version.
pub fn is_newer_version(new_version: &str, current_version: &str) -> bool {
    fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
        let clean = s.trim().trim_start_matches('v');
        let parts: Vec<&str> = clean.split('.').collect();
        if parts.len() < 2 {
            return None;
        }
        let major = parts[0].parse::<u64>().ok()?;
        let minor = parts[1].parse::<u64>().ok()?;
        let patch = if parts.len() >= 3 {
            let patch_clean = parts[2].split(|c: char| !c.is_ascii_digit()).next().unwrap_or("0");
            patch_clean.parse::<u64>().unwrap_or(0)
        } else {
            0
        };
        Some((major, minor, patch))
    }

    match (parse_semver(new_version), parse_semver(current_version)) {
        (Some(new), Some(cur)) => new > cur,
        _ => new_version.trim() != current_version.trim(),
    }
}

/// Обчислює SHA-256 хеш файлу на диску у шістнадцятковому форматі.
pub fn calculate_file_sha256(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("не вдалося відкрити файл для перевірки хешу: {}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .with_context(|| format!("помилка читання файлу: {}", path.display()))?;
    Ok(hex::encode(hasher.finalize()))
}

/// Виконує перевірку оновлення:
/// - завантажує маніфест (з файлу або URL)
/// - звіряє версію
/// - друкує інформацію про реліз та пакунки
/// - якщо вказано verify_file, звіряє його SHA-256 із пакунками
pub async fn check_update(
    http_client: &reqwest::Client,
    manifest_source: Option<&str>,
    verify_file: Option<&Path>,
) -> Result<()> {
    let current_version = downloader_core::VERSION;
    println!("Поточна версія програми: v{current_version}");

    let manifest_str = match manifest_source {
        Some(source) if source.starts_with("http://") || source.starts_with("https://") => {
            println!("Отримання маніфесту з мережі: {source} ...");
            let resp = http_client.get(source).send().await
                .with_context(|| format!("не вдалося завантажити маніфест за адресою {source}"))?;
            if !resp.status().is_success() {
                bail!("сервер оновлень повернув помилку: {}", resp.status());
            }
            resp.text().await.context("помилка читання тіла маніфесту")?
        }
        Some(path_str) => {
            let path = Path::new(path_str);
            println!("Читання локального маніфесту: {} ...", path.display());
            std::fs::read_to_string(path)
                .with_context(|| format!("не вдалося прочитати файл маніфесту: {}", path.display()))?
        }
        None => {
            let url = DEFAULT_UPDATE_MANIFEST_URL;
            println!("Отримання маніфесту з репозиторію: {url} ...");
            let resp = http_client.get(url).send().await
                .with_context(|| format!("не вдалося завантажити офіційний маніфест за адресою {url}"))?;
            if !resp.status().is_success() {
                bail!("сервер оновлень повернув помилку: {}", resp.status());
            }
            resp.text().await.context("помилка читання тіла маніфесту")?
        }
    };

    let manifest = UpdateManifest::parse(&manifest_str)?;

    println!("Версія у маніфесті: v{}", manifest.version);
    if let Some(ref date) = manifest.release_date {
        println!("Дата релізу: {date}");
    }
    if let Some(ref url) = manifest.changelog_url {
        println!("Журнал змін: {url}");
    }

    let newer = is_newer_version(&manifest.version, current_version);
    if newer {
        println!("\n⚡ Знайдено нову версію: v{} (поточна: v{})", manifest.version, current_version);
    } else {
        println!("\n✅ Встановлено актуальну версію (v{}). Оновлення не потрібне.", current_version);
    }

    if !manifest.packages.is_empty() {
        println!("\nДоступні пакунки в маніфесті ({}):", manifest.packages.len());
        for pkg in &manifest.packages {
            let size_mb = (pkg.size as f64) / (1024.0 * 1024.0);
            println!("  • {} ({:.1} МБ)", pkg.name, size_mb);
            println!("    Тип:    {}", pkg.package_type);
            println!("    SHA256: {}", pkg.sha256);
            println!("    URL:    {}", pkg.url);
        }
    }

    if let Some(file_path) = verify_file {
        println!("\nПеревірка цілісності локального файлу: {} ...", file_path.display());
        let calculated_hash = calculate_file_sha256(file_path)?;
        println!("Обчислений SHA-256: {calculated_hash}");

        let matched = manifest.packages.iter().find(|p| p.sha256.eq_ignore_ascii_case(&calculated_hash));
        match matched {
            Some(pkg) => {
                println!("✅ ЗБІГ: Хеш точно відповідає пакунку '{}' ({})", pkg.name, pkg.package_type);
            }
            None => {
                eprintln!("❌ УВАГА! Хеш файлу НЕ збігається з жодним пакунком у маніфесті!");
                bail!("перевірка цілісності провалена: невідомий або підроблений файл");
            }
        }
    }

    println!("\nℹ Безпека: тихе автоматичне оновлення вимкнено (принцип Р-16 / 10 Автооновлення.md).");
    println!("  Завантаження та встановлення нових бінарників здійснюються виключно людиною.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn парсинг_маніфесту_оновлень() {
        let json = r#"{
            "version": "0.2.0",
            "min_supported_version": "0.1.0",
            "release_date": "2026-10-01",
            "changelog_url": "https://example.com/changelog",
            "packages": [
                {
                    "name": "Downloader-0.2.0-web.msi",
                    "type": "msi-web",
                    "url": "https://example.com/web.msi",
                    "size": 45000000,
                    "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
                }
            ]
        }"#;

        let manifest = UpdateManifest::parse(json).expect("парсинг маніфесту");
        assert_eq!(manifest.version, "0.2.0");
        assert_eq!(manifest.packages.len(), 1);
        assert_eq!(manifest.packages[0].name, "Downloader-0.2.0-web.msi");
        assert_eq!(manifest.packages[0].size, 45000000);
        assert_eq!(
            manifest.packages[0].sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn порівняння_версій_semver() {
        assert!(is_newer_version("0.2.0", "0.1.0"));
        assert!(is_newer_version("1.0.0", "0.9.9"));
        assert!(is_newer_version("0.1.1", "0.1.0"));
        assert!(is_newer_version("v0.2.0", "v0.1.0"));

        assert!(!is_newer_version("0.1.0", "0.1.0"));
        assert!(!is_newer_version("0.1.0", "0.2.0"));
        assert!(!is_newer_version("0.0.9", "0.1.0"));
    }

    #[test]
    fn обчислення_sha256_файлу() {
        let temp = std::env::temp_dir().join(format!("test_sha256_{}.txt", std::process::id()));
        std::fs::write(&temp, b"abc").expect("запис");

        let hash = calculate_file_sha256(&temp).expect("розрахунок хешу");
        let _ = std::fs::remove_file(&temp);

        // SHA-256 від "abc":
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
