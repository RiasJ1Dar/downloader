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

/// Куди класти JSON поруч із даними користувача або поруч із exe у портативному режимі.
pub fn manifest_path_for(exe: Option<&Path>) -> PathBuf {
    let base = downloader_winutil::data_dir_for_exe(exe);
    base.join("nm").join(format!("{HOST_NAME}.json"))
}

/// Куди класти JSON поруч із даними користувача.
#[allow(dead_code)]
pub fn default_manifest_path() -> Result<PathBuf> {
    Ok(manifest_path_for(std::env::current_exe().ok().as_deref()))
}

/// Записати JSON і прописати реєстр Windows / теку Firefox.
pub fn install(exe: &Path) -> Result<PathBuf> {
    let exe = exe
        .canonicalize()
        .with_context(|| format!("немає {}", exe.display()))?;
    let path = manifest_path_for(Some(&exe));
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

    #[test]
    fn шлях_маніфесту_враховує_портативний_режим() -> Result<()> {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let temp = std::env::temp_dir().join(format!("nm-port-{unique}"));
        std::fs::create_dir_all(&temp)?;
        let fake_exe = temp.join("downloader-nmhost.exe");
        std::fs::write(&fake_exe, b"")?;

        // Без маркера
        let normal_path = manifest_path_for(Some(&fake_exe));
        assert!(
            normal_path.ends_with(format!("Downloader\\nm\\{HOST_NAME}.json"))
                || normal_path.ends_with(format!("Downloader/nm/{HOST_NAME}.json"))
        );

        // З маркером
        std::fs::write(temp.join("portable.txt"), b"")?;
        let port_path = manifest_path_for(Some(&fake_exe));
        assert_eq!(
            port_path,
            temp.join("nm").join(format!("{HOST_NAME}.json")),
            "у портативному режимі маніфест кладеться поруч"
        );

        assert!(default_manifest_path().is_ok());

        std::fs::remove_dir_all(&temp)?;
        Ok(())
    }
}


