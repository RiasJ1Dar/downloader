//! Поставити native-host маніфест у Chrome/Edge/Firefox.
//!
//! Без цього розширення каже «Specified native messaging host not found».
//! ID розширення зашитий (поле `key` у `ext/chrome/manifest.json`).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::HOST_NAME;

/// Стабільний ID unpacked-розширення (з `key` у manifest.json).
pub const EXTENSION_ID: &str = "pionjhjgjaehkcpkidlblhonbejfdcdj";

/// JSON маніфесту native host.
#[must_use]
pub fn host_manifest_json(exe: &Path) -> String {
    let exe = exe.display().to_string().replace('\\', "\\\\");
    format!(
        r#"{{
  "name": "{HOST_NAME}",
  "description": "Downloader native messaging host",
  "path": "{exe}",
  "type": "stdio",
  "allowed_origins": [
    "chrome-extension://{EXTENSION_ID}/"
  ]
}}
"#
    )
}

/// Куди класти JSON поруч із даними користувача.
pub fn default_manifest_path() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("немає LOCALAPPDATA"))?;
    Ok(base.join("Downloader").join("nm").join(format!("{HOST_NAME}.json")))
}

/// Записати JSON і прописати реєстр Windows / теку Firefox.
pub fn install(exe: &Path) -> Result<PathBuf> {
    let exe = exe
        .canonicalize()
        .with_context(|| format!("немає {}", exe.display()))?;
    let path = default_manifest_path()?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&path, host_manifest_json(&exe))?;

    #[cfg(windows)]
    {
        прописати_chrome_edge(&path)?;
        прописати_firefox(&path)?;
    }
    #[cfg(not(windows))]
    {
        let _ = &path;
    }
    Ok(path)
}

#[cfg(windows)]
fn прописати_chrome_edge(manifest: &Path) -> Result<()> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    for key in [
        r"Software\Google\Chrome\NativeMessagingHosts\com.downloader.host",
        r"Software\Microsoft\Edge\NativeMessagingHosts\com.downloader.host",
    ] {
        let (k, _) = hkcu.create_subkey(key).with_context(|| format!("реєстр {key}"))?;
        k.set_value("", &manifest.display().to_string())
            .with_context(|| format!("запис {key}"))?;
    }
    Ok(())
}

#[cfg(windows)]
fn прописати_firefox(manifest: &Path) -> io::Result<()> {
    let Some(roaming) = std::env::var_os("APPDATA") else {
        return Ok(());
    };
    let dest = PathBuf::from(roaming)
        .join("Mozilla")
        .join("NativeMessagingHosts")
        .join(format!("{HOST_NAME}.json"));
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::copy(manifest, dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_містить_id_і_stdio() {
        let j = host_manifest_json(Path::new(r"C:\dl\downloader-nmhost.exe"));
        assert!(j.contains(EXTENSION_ID));
        assert!(j.contains("stdio"));
        assert!(j.contains("downloader-nmhost.exe"));
        assert!(j.contains(HOST_NAME));
    }
}
