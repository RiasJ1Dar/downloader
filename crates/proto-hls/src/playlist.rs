//! Розбір master / media playlist.

use downloader_core::error::{Error, Result};
use m3u8_rs::{AlternativeMediaType, KeyMethod, Playlist};

use crate::decrypt::{КлючСегмента, МетодКлюча};

/// Розібраний маніфест.
#[derive(Debug, Clone)]
pub enum Маніфест {
    Master { варіанти: Vec<Варіант> },
    Media(Медіа),
}

/// Варіант якості з master playlist.
#[derive(Debug, Clone)]
pub struct Варіант {
    pub uri: String,
    pub bandwidth: u64,
    #[allow(dead_code)]
    pub width: Option<u64>,
    pub height: Option<u64>,
}

/// Media playlist (VOD або live).
#[derive(Debug, Clone)]
pub struct Медіа {
    pub end_list: bool,
    #[allow(dead_code)]
    pub media_sequence: u64,
    /// Секунди; для live — інтервал перечитування маніфесту.
    pub target_duration: u64,
    pub сегменти: Vec<Сегмент>,
}

/// Один медіасегмент.
#[derive(Debug, Clone)]
pub struct Сегмент {
    pub uri: String,
    #[allow(dead_code)]
    pub duration: f32,
    pub sequence: u64,
    pub key: Option<КлючСегмента>,
    pub byte_range: Option<(u64, Option<u64>)>,
    pub discontinuity: bool,
}

/// Доріжка AUDIO або SUBTITLES з `#EXT-X-MEDIA` на master.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Доріжка {
    pub kind: ТипДоріжки,
    pub group_id: String,
    pub name: String,
    pub language: Option<String>,
    pub uri: Option<String>,
    pub default: bool,
}

/// VIDEO і CLOSED-CAPTIONS не входять у цей enum — їх пропускаємо.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ТипДоріжки {
    Audio,
    Subtitles,
}

/// Чи URL схожий на HLS-маніфест.
#[must_use]
pub fn схожий_на_hls(source: &str) -> bool {
    let path = source.split(['?', '#']).next().unwrap_or(source);
    let last = path.rsplit('/').next().unwrap_or(path);
    let lower = last.to_ascii_lowercase();
    lower.ends_with(".m3u8") || lower.ends_with(".m3u")
}

pub fn розібрати(bytes: &[u8], base: &str) -> Result<Маніфест> {
    let parsed = m3u8_rs::parse_playlist_res(bytes).map_err(|e| {
        Error::Store(format!("не розібрати m3u8: {e}"))
    })?;
    match parsed {
        Playlist::MasterPlaylist(m) => {
            let mut варіанти = Vec::new();
            for v in m.variants {
                варіанти.push(Варіант {
                    uri: абсолютний(base, &v.uri)?,
                    bandwidth: v.bandwidth,
                    width: v.resolution.map(|r| r.width),
                    height: v.resolution.map(|r| r.height),
                });
            }
            if варіанти.is_empty() {
                return Err(Error::Store("master playlist без варіантів".to_owned()));
            }
            Ok(Маніфест::Master { варіанти })
        }
        Playlist::MediaPlaylist(m) => Ok(Маніфест::Media(медіа(m, base)?)),
    }
}

/// AUDIO/SUBTITLES з master. Media playlist → порожній Vec, не помилка.
pub fn розібрати_доріжки(bytes: &[u8], base: &str) -> Result<Vec<Доріжка>> {
    let parsed = m3u8_rs::parse_playlist_res(bytes).map_err(|e| {
        Error::Store(format!("не розібрати m3u8: {e}"))
    })?;
    match parsed {
        Playlist::MediaPlaylist(_) => Ok(Vec::new()),
        Playlist::MasterPlaylist(m) => {
            let mut доріжки = Vec::new();
            for a in m.alternatives {
                let kind = match a.media_type {
                    AlternativeMediaType::Audio => ТипДоріжки::Audio,
                    AlternativeMediaType::Subtitles => ТипДоріжки::Subtitles,
                    AlternativeMediaType::Video
                    | AlternativeMediaType::ClosedCaptions
                    | AlternativeMediaType::Other(_) => continue,
                };
                let uri = match a.uri {
                    Some(u) => Some(абсолютний(base, &u)?),
                    None => None,
                };
                доріжки.push(Доріжка {
                    kind,
                    group_id: a.group_id,
                    name: a.name,
                    language: a.language,
                    uri,
                    default: a.default,
                });
            }
            Ok(доріжки)
        }
    }
}

fn медіа(m: m3u8_rs::MediaPlaylist, base: &str) -> Result<Медіа> {
    let mut seq = m.media_sequence;
    let mut поточний_ключ = None;
    let mut сегменти = Vec::new();
    for s in m.segments {
        if let Some(k) = s.key {
            поточний_ключ = ключ_з_тега(k, base)?;
        }
        сегменти.push(Сегмент {
            uri: абсолютний(base, &s.uri)?,
            duration: s.duration,
            sequence: seq,
            key: поточний_ключ.clone(),
            byte_range: s.byte_range.map(|b| (b.length, b.offset)),
            discontinuity: s.discontinuity,
        });
        seq = seq.saturating_add(1);
    }
    Ok(Медіа {
        end_list: m.end_list,
        media_sequence: m.media_sequence,
        target_duration: m.target_duration,
        сегменти,
    })
}

fn ключ_з_тега(k: m3u8_rs::Key, base: &str) -> Result<Option<КлючСегмента>> {
    match k.method {
        KeyMethod::None => Ok(None),
        KeyMethod::SampleAES => Err(Error::Store(
            "HLS захищено SAMPLE-AES / DRM — не обходимо".to_owned(),
        )),
        KeyMethod::AES128 => {
            let uri = k.uri.ok_or_else(|| {
                Error::Store("EXT-X-KEY AES-128 без URI ключа".to_owned())
            })?;
            let iv = match k.iv {
                Some(hex) => Some(розбір_iv(&hex)?),
                None => None,
            };
            Ok(Some(КлючСегмента {
                метод: МетодКлюча::Aes128,
                uri: абсолютний(base, &uri)?,
                iv,
            }))
        }
        other => Err(Error::Store(format!(
            "невідомий метод EXT-X-KEY: {other:?}"
        ))),
    }
}

fn розбір_iv(raw: &str) -> Result<[u8; 16]> {
    let s = raw.trim().trim_start_matches("0x").trim_start_matches("0X");
    let bytes = hex::decode(s).map_err(|e| Error::Store(format!("IV не hex: {e}")))?;
    if bytes.len() != 16 {
        return Err(Error::Store(format!(
            "IV має бути 16 байтів, маємо {}",
            bytes.len()
        )));
    }
    let mut iv = [0u8; 16];
    iv.copy_from_slice(&bytes);
    Ok(iv)
}

fn абсолютний(base: &str, rel: &str) -> Result<String> {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return Ok(rel.to_owned());
    }
    // Без крейта `url`: кореневий Cargo.toml чіпати не можна.
    let без_якоря = base.split(['?', '#']).next().unwrap_or(base);
    if rel.starts_with('/') {
        let scheme_end = без_якоря.find("://").map(|i| i + 3).ok_or_else(|| {
            Error::Store("базовий URL без схеми".to_owned())
        })?;
        let host_end = без_якоря[scheme_end..]
            .find('/')
            .map(|i| scheme_end + i)
            .unwrap_or(без_якоря.len());
        return Ok(format!("{}{rel}", &без_якоря[..host_end]));
    }
    let dir = match без_якоря.rfind('/') {
        Some(i) if i + 1 > без_якоря.find("://").map(|j| j + 3).unwrap_or(0) => {
            &без_якоря[..=i]
        }
        _ => без_якоря,
    };
    Ok(format!("{dir}{rel}"))
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "у тестах падіння і є повідомлення")]
mod tests {
    use super::*;

    #[test]
    fn відносний_шлях_від_маніфесту() {
        assert_eq!(
            абсолютний("http://127.0.0.1:9/hls/vod/media.m3u8", "seg0.ts").unwrap(),
            "http://127.0.0.1:9/hls/vod/seg0.ts"
        );
    }

    #[test]
    fn абсолютний_від_кореня() {
        assert_eq!(
            абсолютний("http://127.0.0.1:9/hls/vod/media.m3u8", "/k.ts").unwrap(),
            "http://127.0.0.1:9/k.ts"
        );
    }

    #[test]
    fn впізнає_m3u_і_m3u8() {
        assert!(схожий_на_hls("https://cdn.example/a.m3u8"));
        assert!(схожий_на_hls("https://cdn.example/a.m3u"));
        assert!(!схожий_на_hls("https://cdn.example/a.mp4"));
        assert!(!схожий_на_hls("https://cdn.example/am3u8"));
    }

    const MASTER_З_ДОРІЖКАМИ: &[u8] = b"\
#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"Spanish\",LANGUAGE=\"es\",DEFAULT=NO,URI=\"audio/es.m3u8\"\n\
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"subs/en.m3u8\"\n\
#EXT-X-MEDIA:TYPE=VIDEO,GROUP-ID=\"vid\",NAME=\"Main\",DEFAULT=YES\n\
#EXT-X-MEDIA:TYPE=CLOSED-CAPTIONS,GROUP-ID=\"cc\",NAME=\"CC1\",LANGUAGE=\"en\",DEFAULT=YES,INSTREAM-ID=\"CC1\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=800000,AUDIO=\"audio\",SUBTITLES=\"subs\",CLOSED-CAPTIONS=\"cc\"\n\
media.m3u8\n";

    const MEDIA_ПЛЕЙЛИСТ: &[u8] = b"\
#EXTM3U\n\
#EXT-X-VERSION:3\n\
#EXT-X-TARGETDURATION:4\n\
#EXT-X-MEDIA-SEQUENCE:1\n\
#EXTINF:4.0,\n\
seg0.ts\n\
#EXT-X-ENDLIST\n";

    #[test]
    fn master_збирає_audio_і_subtitles() {
        let tracks = розібрати_доріжки(
            MASTER_З_ДОРІЖКАМИ,
            "http://127.0.0.1:9/hls/vod/master.m3u8",
        )
        .unwrap();
        assert_eq!(tracks.len(), 3, "VIDEO і CLOSED-CAPTIONS мали бути пропущені");
        assert_eq!(tracks[0].kind, ТипДоріжки::Audio);
        assert_eq!(tracks[0].group_id, "audio");
        assert_eq!(tracks[0].name, "English");
        assert_eq!(tracks[0].language.as_deref(), Some("en"));
        assert_eq!(tracks[0].uri, None, "muxed audio без окремого плейлиста");
        assert!(tracks[0].default);
        assert_eq!(tracks[1].kind, ТипДоріжки::Audio);
        assert_eq!(tracks[1].name, "Spanish");
        assert!(!tracks[1].default);
        assert_eq!(tracks[2].kind, ТипДоріжки::Subtitles);
        assert_eq!(tracks[2].group_id, "subs");
        assert_eq!(tracks[2].name, "English");
        assert!(tracks[2].default);
    }

    #[test]
    fn media_playlist_дає_порожній_список_доріжок() {
        let tracks = розібрати_доріжки(
            MEDIA_ПЛЕЙЛИСТ,
            "http://127.0.0.1:9/hls/vod/media.m3u8",
        )
        .unwrap();
        assert!(tracks.is_empty(), "media playlist не має EXT-X-MEDIA");
    }

    #[test]
    fn відносний_uri_доріжки_стає_абсолютним() {
        let tracks = розібрати_доріжки(
            MASTER_З_ДОРІЖКАМИ,
            "http://127.0.0.1:9/hls/vod/master.m3u8",
        )
        .unwrap();
        assert_eq!(
            tracks[1].uri.as_deref(),
            Some("http://127.0.0.1:9/hls/vod/audio/es.m3u8")
        );
        assert_eq!(
            tracks[2].uri.as_deref(),
            Some("http://127.0.0.1:9/hls/vod/subs/en.m3u8")
        );
    }
}
