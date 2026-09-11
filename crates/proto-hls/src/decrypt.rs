//! AES-128 для `#EXT-X-KEY`.
//!
//! Найчастіша помилка — IV: явний `IV=` або неявний із media sequence
//! (16 байтів, big-endian). Переплутати їх — файл є, але не грається.

use aes::Aes128;
use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use downloader_core::error::{Error, Result};

type Aes128CbcDec = cbc::Decryptor<Aes128>;

/// Метод шифрування сегмента.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum МетодКлюча {
    Aes128,
}

/// Параметри ключа, зібрані з плейлиста.
#[derive(Debug, Clone)]
pub struct КлючСегмента {
    pub метод: МетодКлюча, // потрібен, щоб не сплутати AES-128 з DRM, якщо з'являться ще методи
    pub uri: String,
    /// Якщо `None` — IV = media sequence як 16 байтів BE.
    pub iv: Option<[u8; 16]>,
}

/// IV за специфікацією HLS, коли тег його не дав.
#[must_use]
pub fn iv_з_sequence(media_sequence: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&media_sequence.to_be_bytes());
    iv
}

/// Розшифрувати тіло сегмента. PKCS7 на останньому блоці.
pub fn розшифрувати(cipher: &[u8], key: &[u8], iv: &[u8; 16]) -> Result<Vec<u8>> {
    if key.len() != 16 {
        return Err(Error::Store(format!(
            "ключ AES-128 має бути 16 байтів, маємо {}",
            key.len()
        )));
    }
    if cipher.len() < 16 || !cipher.len().is_multiple_of(16) {
        return Err(Error::Store(format!(
            "шифротекст AES-128 має бути кратний 16, маємо {}",
            cipher.len()
        )));
    }
    let mut buf = cipher.to_vec();
    let dec = Aes128CbcDec::new_from_slices(key, iv).map_err(|e| {
        Error::Store(format!("не зібрати AES-128 CBC: {e}"))
    })?;
    let plain = dec
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| Error::Store("AES-128: недійсний PKCS7".to_owned()))?;
    Ok(plain.to_vec())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "у тестах падіння і є повідомлення")]
mod tests {
    use super::*;
    use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};

    type Aes128CbcEnc = cbc::Encryptor<Aes128>;

    #[test]
    fn неявний_iv_це_sequence_у_старших_байтах() {
        let iv = iv_з_sequence(1);
        assert_eq!(&iv[8..], &1u64.to_be_bytes());
        assert!(iv[..8].iter().all(|&b| b == 0));
    }

    #[test]
    fn туди_й_назад() {
        let key = *b"0123456789abcdef";
        let iv = iv_з_sequence(7);
        let plain = b"SEG0-PAYLOAD-AAAAAAAAAAAAAAAA";
        let mut buf = vec![0u8; plain.len() + 16];
        buf[..plain.len()].copy_from_slice(plain);
        let enc = Aes128CbcEnc::new_from_slices(&key, &iv).unwrap();
        let cipher = enc
            .encrypt_padded_mut::<Pkcs7>(&mut buf, plain.len())
            .unwrap()
            .to_vec();
        let out = розшифрувати(&cipher, &key, &iv).unwrap();
        assert_eq!(out, plain);
    }
}
