//! Інтеграційні тести розбору DASH MPD на живих фікстурах.
//!
//! Покриваємо:
//! - SegmentTimeline з r-повторами та підстановкою $Time$;
//! - $Number%0Nd$ форматування номерів сегментів із нульовим вирівнюванням;
//! - Маніфести з кількома тегами <Period>;
//! - Вкладені відносні та абсолютні BaseURL;
//! - Формування переліку якостей (відсікання звуку, сортування від найкращого).

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "у тестах падіння і є повідомленням про помилку"
)]

use downloader_proto_dash::{варіанти, розібрати_mpd};

const TIMELINE_FIXTURE: &str = include_str!("fixtures/timeline_with_r_and_time.mpd");
const NUMBER_FIXTURE: &str = include_str!("fixtures/number_formatting.mpd");
const MULTI_PERIOD_FIXTURE: &str = include_str!("fixtures/multi_period.mpd");
const BASE_URL_FIXTURE: &str = include_str!("fixtures/base_url_hierarchical.mpd");

#[test]
fn timeline_розгортає_r_повтори_і_підставляє_time() {
    let source = "https://dash.example.com/live/manifest.mpd";
    let якості = розібрати_mpd(TIMELINE_FIXTURE, source).unwrap();

    assert_eq!(якості.len(), 2, "має бути 1 відео і 1 звук: {якості:?}");

    let відео = якості.iter().find(|я| !я.звук).expect("має бути відео");
    assert_eq!(відео.height, Some(720));
    assert_eq!(відео.bandwidth, 2_500_000);

    // init + 4 відрізки (r=3 означає 1 + 3) + 2 відрізки (r=1 означає 1 + 1) = 7 сегментів
    assert_eq!(
        відео.сегменти.len(),
        7,
        "init + 4 + 2 сегменти: {:?}",
        відео.сегменти
    );
    assert_eq!(
        відео.сегменти[0],
        "https://dash.example.com/live/init-v720.mp4"
    );
    assert_eq!(
        відео.сегменти[1],
        "https://dash.example.com/live/chunk-v720-0.m4s"
    );
    assert_eq!(
        відео.сегменти[2],
        "https://dash.example.com/live/chunk-v720-2000.m4s"
    );
    assert_eq!(
        відео.сегменти[3],
        "https://dash.example.com/live/chunk-v720-4000.m4s"
    );
    assert_eq!(
        відео.сегменти[4],
        "https://dash.example.com/live/chunk-v720-6000.m4s"
    );
    assert_eq!(
        відео.сегменти[5],
        "https://dash.example.com/live/chunk-v720-8000.m4s"
    );
    assert_eq!(
        відео.сегменти[6],
        "https://dash.example.com/live/chunk-v720-9000.m4s"
    );

    let звук = якості.iter().find(|я| я.звук).expect("має бути звук");
    assert_eq!(звук.bandwidth, 128_000);
    assert_eq!(
        звук.сегменти.len(),
        3,
        "init + 2 сегменти звуку: {:?}",
        звук.сегменти
    );
    assert_eq!(
        звук.сегменти[0],
        "https://dash.example.com/live/init-a128.mp4"
    );
    assert_eq!(
        звук.сегменти[1],
        "https://dash.example.com/live/audio-a128-0.m4s"
    );
    assert_eq!(
        звук.сегменти[2],
        "https://dash.example.com/live/audio-a128-5000.m4s"
    );
}

#[test]
fn шаблон_підставляє_number_з_форматом_ширини() {
    let source = "https://dash.example.com/vod/stream.mpd";
    let якості = розібрати_mpd(NUMBER_FIXTURE, source).unwrap();

    assert_eq!(якості.len(), 1);
    let q = &якості[0];
    assert_eq!(q.height, Some(1080));
    assert_eq!(q.bandwidth, 5_000_000);

    // 20 секунд / 4 секунди = 5 сегментів + 1 init
    assert_eq!(q.сегменти.len(), 6);
    assert_eq!(
        q.сегменти[0],
        "https://dash.example.com/vod/init-1080p.mp4"
    );
    assert_eq!(
        q.сегменти[1],
        "https://dash.example.com/vod/segment-1080p-00001.m4s"
    );
    assert_eq!(
        q.сегменти[2],
        "https://dash.example.com/vod/segment-1080p-00002.m4s"
    );
    assert_eq!(
        q.сегменти[5],
        "https://dash.example.com/vod/segment-1080p-00005.m4s"
    );
}

#[test]
fn багатоперіодний_mpd_збирає_якості_з_усіх_періодів() {
    let source = "https://dash.example.com/multi/manifest.mpd";
    let якості = розібрати_mpd(MULTI_PERIOD_FIXTURE, source).unwrap();

    // 1 з Period 1 (720p 2.0M) + 2 з Period 2 (720p 2.1M та 1080p 4.0M)
    assert_eq!(якості.len(), 3, "три представлення з двох періодів: {якості:?}");

    // Період 1 (8 сек / 4 сек = 2 сегменти)
    let p1_720 = якості
        .iter()
        .find(|я| я.bandwidth == 2_000_000)
        .expect("має бути 720p з p1");
    assert_eq!(p1_720.сегменти.len(), 2);
    assert_eq!(
        p1_720.сегменти[0],
        "https://dash.example.com/multi/p1-1.m4s"
    );
    assert_eq!(
        p1_720.сегменти[1],
        "https://dash.example.com/multi/p1-2.m4s"
    );

    // Період 2 (12 сек / 4 сек = 3 сегменти)
    let p2_720 = якості
        .iter()
        .find(|я| я.bandwidth == 2_100_000)
        .expect("має бути 720p з p2");
    assert_eq!(p2_720.сегменти.len(), 3);
    assert_eq!(
        p2_720.сегменти[0],
        "https://dash.example.com/multi/p2-1.m4s"
    );
    assert_eq!(
        p2_720.сегменти[2],
        "https://dash.example.com/multi/p2-3.m4s"
    );

    let p2_1080 = якості
        .iter()
        .find(|я| я.bandwidth == 4_000_000)
        .expect("має бути 1080p з p2");
    assert_eq!(p2_1080.height, Some(1080));
    assert_eq!(p2_1080.сегменти.len(), 3);
    assert_eq!(
        p2_1080.сегменти[0],
        "https://dash.example.com/multi/p2-1080-1.m4s"
    );
}

#[test]
fn вкладені_base_url_коректно_визначають_кінцеві_адреси() {
    let source = "https://cdn.example.com/live/manifest.mpd";
    let якості = розібрати_mpd(BASE_URL_FIXTURE, source).unwrap();

    assert_eq!(якості.len(), 1);
    let q = &якості[0];

    // MPD base: "media/" -> Period base: "stream1/" -> Adaptation base: "video/"
    // Разом: https://cdn.example.com/live/media/stream1/video/
    assert_eq!(q.сегменти.len(), 3); // init + 2 сегменти
    assert_eq!(
        q.сегменти[0],
        "https://cdn.example.com/live/media/stream1/video/init.mp4"
    );
    assert_eq!(
        q.сегменти[1],
        "https://cdn.example.com/live/media/stream1/video/seg-1.m4s"
    );
    // Абсолютна адреса не повинна залежати від BaseURL
    assert_eq!(q.сегменти[2], "https://cdn.other.com/seg-2.m4s");
}

#[test]
fn перелік_якостей_dash_ігнорує_звукові_доріжки_і_сортує_відео() {
    let source = "https://dash.example.com/live/manifest.mpd";
    let якості = розібрати_mpd(TIMELINE_FIXTURE, source).unwrap();

    // розібрати_mpd повертає якості, відсортовані за зростанням бітрейта
    let перелік = варіанти(&якості);

    // У варіантах МАЄ бути відео і НЕ МАЄ бути звуку (звук качається окремо)
    assert_eq!(перелік.len(), 1, "звук мав бути відфільтрований: {перелік:?}");
    assert_eq!(перелік[0].id, "720p.mp4");
    assert_eq!(перелік[0].height, Some(720));
    assert_eq!(перелік[0].label, "720p");
}
