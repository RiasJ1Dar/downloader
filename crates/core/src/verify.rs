//! Перевірка готового файла: довжина, потім SHA-256.
//!
//! Лічильник байтів у пам'яті — не доказ. Після качання дивимось на диск.

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Файл на диску має бути рівно `expected` байтів.
pub fn length(path: &Path, expected: u64) -> Result<()> {
    let actual = std::fs::metadata(path)?.len();
    if actual != expected {
        return Err(Error::VerificationFailed {
            path: path.to_path_buf(),
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(())
}

/// SHA-256 файла з диска, hex малими літерами.
pub fn sha256(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::let_underscore_must_use,
    reason = "у тестах падіння — це і є повідомлення про помилку"
)]
mod tests {
    use super::*;

    #[test]
    fn довжина_ловиться() {
        let dir = std::env::temp_dir().join(format!("dl-verify-len-{}", std::process::id()));
        std::fs::write(&dir, b"abcd").unwrap();
        length(&dir, 4).unwrap();
        let err = length(&dir, 3).unwrap_err();
        let text = err.to_string();
        assert!(text.contains('3'), "{text}");
        assert!(text.contains('4'), "{text}");
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn sha256_відомого() {
        let dir = std::env::temp_dir().join(format!("dl-verify-hash-{}", std::process::id()));
        std::fs::write(&dir, b"abc").unwrap();
        assert_eq!(
            sha256(&dir).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(&dir);
    }
}
