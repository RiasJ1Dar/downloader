//! Інтеграційні тести розбору HLS-маніфестів на живих фікстурах.
//!
//! Перевіряємо розбір майстер-плейлистів із доріжками аудіо/субтитрів,
//! розшифрування ключів AES-128 з явними IV, а також резолвінг
//! відносних та абсолютних адрес сегментів.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "у тестах падіння і є повідомленням про помилку"
)]

use downloader_proto_hls::decrypt::МетодКлюча;
use downloader_proto_hls::playlist::{
    Маніфест, ТипДоріжки, розібрати, розібрати_доріжки,
};
use downloader_proto_hls::якості;

const MASTER_FIXTURE: &[u8] = include_bytes!("fixtures/master_with_audio_and_subs.m3u8");
const AES128_FIXTURE: &[u8] = include_bytes!("fixtures/media_aes128.m3u8");
const URIS_FIXTURE: &[u8] = include_bytes!("fixtures/media_uris.m3u8");

#[test]
fn master_розбирає_доріжки_audio_та_subtitles_з_мови_і_дефолтами() {
    let base = "https://cdn.example.com/live/hls/master.m3u8";
    let tracks = розібрати_доріжки(MASTER_FIXTURE, base).unwrap();

    // 3 аудіодоріжки + 2 доріжки субтитрів.
    assert_eq!(tracks.len(), 5, "очікували 3 аудіо і 2 субтитри: {tracks:?}");

    let audio_tracks: Vec<_> = tracks
        .iter()
        .filter(|t| t.kind == ТипДоріжки::Audio)
        .collect();
    assert_eq!(audio_tracks.len(), 3);

    // 1. Англійська: muxed, без окремого URI, дефолтна.
    assert_eq!(audio_tracks[0].name, "English");
    assert_eq!(audio_tracks[0].language.as_deref(), Some("en"));
    assert!(audio_tracks[0].default);
    assert_eq!(audio_tracks[0].uri, None);

    // 2. Українська: відносний URI розгортається відносно master.
    assert_eq!(audio_tracks[1].name, "Ukrainian");
    assert_eq!(audio_tracks[1].language.as_deref(), Some("uk"));
    assert!(!audio_tracks[1].default);
    assert_eq!(
        audio_tracks[1].uri.as_deref(),
        Some("https://cdn.example.com/live/hls/audio/uk.m3u8")
    );

    // 3. Іспанська: абсолютний URI лишається абсолютним.
    assert_eq!(audio_tracks[2].name, "Spanish");
    assert_eq!(audio_tracks[2].language.as_deref(), Some("es"));
    assert!(!audio_tracks[2].default);
    assert_eq!(
        audio_tracks[2].uri.as_deref(),
        Some("https://cdn.example.com/audio/es.m3u8")
    );

    let subs_tracks: Vec<_> = tracks
        .iter()
        .filter(|t| t.kind == ТипДоріжки::Subtitles)
        .collect();
    assert_eq!(subs_tracks.len(), 2);

    // 1. Українські субтитри (дефолтні, відносний vtt).
    assert_eq!(subs_tracks[0].name, "Ukrainian");
    assert_eq!(subs_tracks[0].language.as_deref(), Some("uk"));
    assert!(subs_tracks[0].default);
    assert_eq!(
        subs_tracks[0].uri.as_deref(),
        Some("https://cdn.example.com/live/hls/subs/uk.vtt")
    );

    // 2. Англійські субтитри (недефолтні, абсолютний URI).
    assert_eq!(subs_tracks[1].name, "English");
    assert_eq!(subs_tracks[1].language.as_deref(), Some("en"));
    assert!(!subs_tracks[1].default);
    assert_eq!(
        subs_tracks[1].uri.as_deref(),
        Some("https://cdn.example.com/subs/en.vtt")
    );
}

#[test]
fn media_розбирає_ext_x_key_aes128_і_iv() {
    let base = "https://media.example.com/vod/stream.m3u8";
    let manifest = розібрати(AES128_FIXTURE, base).unwrap();

    let Маніфест::Media(media) = manifest else {
        panic!("мав бути media playlist");
    };

    assert!(media.end_list, "має бути завершений VOD-список");
    assert_eq!(media.media_sequence, 100);
    assert_eq!(media.сегменти.len(), 3);

    // Перший сегмент із ключем key1.bin і явним IV.
    let seg0 = &media.сегменти[0];
    assert_eq!(seg0.uri, "https://media.example.com/vod/seg-100.ts");
    let key0 = seg0.key.as_ref().expect("seg0 має містити ключ");
    assert_eq!(key0.метод, МетодКлюча::Aes128);
    assert_eq!(key0.uri, "https://media.example.com/vod/keys/key1.bin");
    assert_eq!(
        key0.iv,
        Some([
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
        ])
    );

    // Другий сегмент успадковує key1.
    let seg1 = &media.сегменти[1];
    let key1 = seg1.key.as_ref().expect("seg1 має наслідувати ключ");
    assert_eq!(key1.uri, "https://media.example.com/vod/keys/key1.bin");
    assert_eq!(key1.iv, key0.iv);

    // Третій сегмент перемикається на абсолютний key2.bin з новим IV.
    let seg2 = &media.сегменти[2];
    let key2 = seg2.key.as_ref().expect("seg2 має містити оновлений ключ");
    assert_eq!(key2.uri, "https://auth.example.com/keys/key2.bin");
    assert_eq!(
        key2.iv,
        Some([
            0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10,
            0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10,
        ])
    );
}

#[test]
fn розбір_відносних_та_абсолютних_uri_для_плейлистів_і_сегментів() {
    let base = "http://example.com/vod/playlist.m3u8";
    let manifest = розібрати(URIS_FIXTURE, base).unwrap();

    let Маніфест::Media(media) = manifest else {
        panic!("мав бути media playlist");
    };

    assert_eq!(media.сегменти.len(), 4);
    assert_eq!(
        media.сегменти[0].uri,
        "http://example.com/vod/segment-relative.ts"
    );
    assert_eq!(
        media.сегменти[1].uri,
        "http://example.com/vod/subfolder/segment-sub.ts"
    );
    assert_eq!(
        media.сегменти[2].uri,
        "http://example.com/root-relative/segment-root.ts"
    );
    assert_eq!(
        media.сегменти[3].uri,
        "https://cdn.another.com/cdn/segment-abs.ts"
    );
}

#[test]
fn перелік_якостей_hls_сортується_від_найвищої_роздільності() {
    let base = "https://cdn.example.com/live/hls/master.m3u8";
    let manifest = розібрати(MASTER_FIXTURE, base).unwrap();

    let Маніфест::Master { mut варіанти } = manifest else {
        panic!("мав бути master playlist");
    };

    assert_eq!(варіанти.len(), 4);
    // Вхідний порядок сортується модулем за bandwidth.
    варіанти.sort_by_key(|v| v.bandwidth);

    let list = якості(&варіанти);
    assert_eq!(list.len(), 4);

    // Перелік іде від найкращого до найгіршого:
    assert_eq!(list[0].id, "1080p.ts");
    assert_eq!(list[0].label, "1080p");
    assert_eq!(list[0].height, Some(1080));
    assert_eq!(list[0].note.as_deref(), Some("5.0 Мбіт/с"));

    assert_eq!(list[1].id, "720p.ts");
    assert_eq!(list[1].label, "720p");
    assert_eq!(list[1].height, Some(720));
    assert_eq!(list[1].note.as_deref(), Some("2.5 Мбіт/с"));

    assert_eq!(list[2].id, "480p.ts");
    assert_eq!(list[2].label, "480p");
    assert_eq!(list[2].height, Some(480));
    assert_eq!(list[2].note.as_deref(), Some("0.8 Мбіт/с"));

    // Варіант без роздільності показує бітрейт у назві
    assert_eq!(list[3].id, "128000bps.ts");
    assert_eq!(list[3].label, "0.1 Мбіт/с");
    assert_eq!(list[3].height, None);
    assert_eq!(list[3].note, None);
}
