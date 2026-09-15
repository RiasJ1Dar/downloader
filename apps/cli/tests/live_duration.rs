use std::process::Command;

#[test]
fn live_без_duration_відмовляє_зрозуміло() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let server = rt.block_on(downloader_testserver::EvilServer::start()).expect("стенд");
    let url = server.url("/hls/live/media.m3u8");

    let output = Command::new(env!("CARGO_BIN_EXE_dl"))
        .args(["get", &url])
        .output()
        .expect("dl get");

    assert!(!output.status.success(), "має завершитись із помилкою");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("нескінченний live-потік потребує явного обмеження тривалості"),
        "stderr має пояснювати потребу в --duration: {stderr}"
    );

    rt.block_on(server.shutdown());
}

#[test]
fn live_з_duration_успішно_записує_відрізок() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let server = rt.block_on(downloader_testserver::EvilServer::start()).expect("стенд");
    let url = server.url("/hls/live/media.m3u8");

    let dir = std::env::temp_dir().join(format!(
        "dl-test-live-dur-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("тека");
    let dest = dir.join("recorded.ts");

    let output = Command::new(env!("CARGO_BIN_EXE_dl"))
        .args([
            "get",
            &url,
            "-o",
            dest.to_str().expect("шлях"),
            "--duration",
            "1s",
        ])
        .output()
        .expect("dl get");

    assert!(output.status.success(), "має завершитись успішно: {:?}", output);
    assert!(dest.exists(), "файл має бути записаний на диск");
    let data = std::fs::read(&dest).expect("прочитати");
    assert!(!data.is_empty(), "файл не має бути порожнім");

    rt.block_on(server.shutdown());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn live_з_rewind_без_буфера_чесно_відмовляє() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let server = rt.block_on(downloader_testserver::EvilServer::start()).expect("стенд");
    let url = server.url("/hls/live/media.m3u8");

    let output = Command::new(env!("CARGO_BIN_EXE_dl"))
        .args(["get", &url, "--duration", "5s", "--rewind"])
        .output()
        .expect("dl get");

    assert!(!output.status.success(), "має завершитись із помилкою");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("джерело не дає перемотування назад"),
        "stderr має пояснювати неможливість перемотування: {stderr}"
    );

    rt.block_on(server.shutdown());
}
